//! Login through a generic OpenID Connect provider: the authorization-code flow
//! with PKCE (`S256`), a `state` bound to the browser that started the login and
//! a `nonce` the ID token must carry. The ID token is checked here, against the
//! provider's published keys: an allowed algorithm, the signature, `iss`, `aud`
//! (and `azp`), `exp`, `iat`, `nbf` and the nonce.
//!
//! The identity is `<issuer>:<sub>`: the provider of an
//! [`ExternalIdentity`] is the issuer and its subject is `sub`. Only an email
//! the provider marks verified is used to accept an email invitation.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use openporch_store::ExternalIdentity;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::auth;
use crate::browser_login::LoginSecrets;
use crate::config::OidcLoginConfig;
use crate::identity::{ProviderError, check_url, read_secret_file};

/// The ID token signing algorithms openporch can check.
pub const ALGORITHMS: &[&str] = &[
    "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA",
];
/// How far the provider's clock may be from this server's, in seconds.
pub const LEEWAY_SECS: i64 = 60;
/// How long the discovery document and the keys are reused before they are
/// fetched again.
const DOCUMENT_TTL_MS: i64 = 60 * 60 * 1000;
/// The shortest time between two fetches of the keys for a token that names a
/// key they do not hold, so forged tokens cannot make the server hammer the
/// provider.
const JWKS_MIN_REFRESH_MS: i64 = 60 * 1000;
/// The largest discovery document, key set or token response read.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// The longest subject kept, as identity subjects are elsewhere.
const MAX_SUBJECT_LEN: usize = 200;

#[derive(Debug, Clone, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
}

#[derive(Debug)]
struct Cached<T> {
    value: T,
    fetched_at: i64,
}

/// An OpenID Connect provider people sign in with.
pub struct OidcLogin {
    config: OidcLoginConfig,
    client_secret: Option<Zeroizing<String>>,
    client: reqwest::Client,
    discovery: tokio::sync::Mutex<Option<Cached<Discovery>>>,
    jwks: tokio::sync::Mutex<Option<Cached<Vec<Value>>>>,
    jwks_min_refresh_ms: AtomicI64,
}

/// Leaves out the client secret.
impl std::fmt::Debug for OidcLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcLogin")
            .field("id", &self.config.id)
            .field("issuer", &self.config.issuer)
            .field("client_id", &self.config.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .finish_non_exhaustive()
    }
}

fn unavailable(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::Unavailable(e.to_string())
}

fn invalid(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::Invalid(e.to_string())
}

/// Reads at most [`MAX_DOCUMENT_BYTES`] of a response body.
async fn read_bounded(mut res: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(unavailable)? {
        if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(unavailable("the provider's response is too large"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `application/x-www-form-urlencoded` encoding of one value, as client
/// credentials need before they go into a Basic authorization header.
fn form_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                (b as char).to_string()
            }
            b' ' => "+".to_owned(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Authorization codes are opaque: bound their size and keep them to visible
/// ASCII before forwarding them.
fn is_authorization_code(code: &str) -> bool {
    !code.is_empty() && code.len() <= 4096 && code.bytes().all(|b| b.is_ascii_graphic())
}

/// The PKCE `S256` challenge of `verifier`.
pub fn code_challenge(verifier: &str) -> String {
    B64.encode(Sha256::digest(verifier.as_bytes()))
}

/// A seconds claim, which may be written as a fraction.
fn seconds(claims: &Value, name: &str) -> Option<i64> {
    let v = &claims[name];
    v.as_i64().or_else(|| v.as_f64().map(|f| f.floor() as i64))
}

/// The JWK key type and curve an algorithm needs.
fn key_type(alg: &str) -> (&'static str, Option<&'static str>) {
    match alg {
        "ES256" => ("EC", Some("P-256")),
        "ES384" => ("EC", Some("P-384")),
        "EdDSA" => ("OKP", Some("Ed25519")),
        _ => ("RSA", None),
    }
}

/// The signing key a token names: by `kid`, or the only key that fits when the
/// token names none.
fn find_key<'a>(keys: &'a [Value], kid: Option<&str>, alg: &str) -> Option<&'a Value> {
    let (kty, crv) = key_type(alg);
    let mut fitting = keys.iter().filter(|k| {
        k["kty"] == kty
            && crv.is_none_or(|c| k["crv"] == c)
            && k.get("use").is_none_or(|u| u == "sig")
            && k.get("alg").is_none_or(|a| a == alg)
    });
    match kid {
        Some(kid) => fitting.find(|k| k["kid"] == kid),
        None => {
            let only = fitting.next()?;
            fitting.next().is_none().then_some(only)
        }
    }
}

/// Checks `signature` over `message` with `jwk` under `alg`.
fn verify_signature(
    alg: &str,
    jwk: &Value,
    message: &[u8],
    signature: &[u8],
) -> Result<(), String> {
    use ring::signature as sig;
    let part = |name: &str| -> Result<Vec<u8>, String> {
        let text = jwk[name]
            .as_str()
            .ok_or_else(|| format!("the provider's key has no `{name}`"))?;
        B64.decode(text)
            .map_err(|_| format!("the provider's key has a malformed `{name}`"))
    };
    let verified = match alg {
        "RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512" => {
            let params: &sig::RsaParameters = match alg {
                "RS256" => &sig::RSA_PKCS1_2048_8192_SHA256,
                "RS384" => &sig::RSA_PKCS1_2048_8192_SHA384,
                "RS512" => &sig::RSA_PKCS1_2048_8192_SHA512,
                "PS256" => &sig::RSA_PSS_2048_8192_SHA256,
                "PS384" => &sig::RSA_PSS_2048_8192_SHA384,
                _ => &sig::RSA_PSS_2048_8192_SHA512,
            };
            sig::RsaPublicKeyComponents {
                n: part("n")?,
                e: part("e")?,
            }
            .verify(params, message, signature)
        }
        "ES256" | "ES384" => {
            let (len, params): (usize, &'static sig::EcdsaVerificationAlgorithm) = if alg == "ES256"
            {
                (32, &sig::ECDSA_P256_SHA256_FIXED)
            } else {
                (48, &sig::ECDSA_P384_SHA384_FIXED)
            };
            let (x, y) = (part("x")?, part("y")?);
            if x.len() != len || y.len() != len {
                return Err("the provider's key has coordinates of the wrong size".into());
            }
            let mut point = Vec::with_capacity(1 + 2 * len);
            point.push(4);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            sig::UnparsedPublicKey::new(params, point).verify(message, signature)
        }
        "EdDSA" => {
            sig::UnparsedPublicKey::new(&sig::ED25519, part("x")?).verify(message, signature)
        }
        other => return Err(format!("the algorithm `{other}` is not supported")),
    };
    verified.map_err(|_| "the ID token's signature does not verify".to_owned())
}

impl OidcLogin {
    /// A provider for `config`, which [`crate::Config`] has validated.
    pub fn new(config: &OidcLoginConfig) -> Result<Self, String> {
        let client_secret = match &config.client_secret_file {
            Some(path) => Some(read_secret_file(
                path,
                &format!("the client secret of OIDC provider `{}`", config.id),
            )?),
            None => None,
        };
        Self::with_secret(config, client_secret)
    }

    /// A provider whose client secret, if any, is given rather than read from
    /// `client_secret_file`, as for providers managed through the API.
    pub fn with_secret(
        config: &OidcLoginConfig,
        client_secret: Option<Zeroizing<String>>,
    ) -> Result<Self, String> {
        // `ring` is the crypto provider; installing it twice is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let https = Url::parse(&config.issuer).is_ok_and(|u| u.scheme() == "https");
        let client = reqwest::Client::builder()
            .user_agent(concat!("openporch/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .https_only(https)
            .build()
            .map_err(|e| format!("building the client of OIDC provider `{}`: {e}", config.id))?;
        Ok(Self {
            config: config.clone(),
            client_secret,
            client,
            discovery: tokio::sync::Mutex::new(None),
            jwks: tokio::sync::Mutex::new(None),
            jwks_min_refresh_ms: AtomicI64::new(JWKS_MIN_REFRESH_MS),
        })
    }

    /// The provider's configuration.
    pub fn config(&self) -> &OidcLoginConfig {
        &self.config
    }

    /// Lets tests fetch the keys again as soon as a token names an unknown one.
    #[cfg(test)]
    pub fn refresh_keys_at_once(&self) {
        self.jwks_min_refresh_ms.store(0, Ordering::Relaxed);
    }

    /// New secrets for a login: its nonce and PKCE code verifier.
    pub fn new_secrets() -> LoginSecrets {
        LoginSecrets {
            nonce: Zeroizing::new(auth::random_id(32)),
            // 64 characters from the unreserved set, as RFC 7636 asks.
            verifier: Zeroizing::new(auth::random_id(32)),
        }
    }

    async fn get(&self, url: Url, what: &str) -> Result<Vec<u8>, ProviderError> {
        let res = self
            .client
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(unavailable)?;
        if !res.status().is_success() {
            return Err(unavailable(format!(
                "the provider answered {} for its {what}",
                res.status()
            )));
        }
        read_bounded(res).await
    }

    /// The discovery document, fetched again once it is an hour old.
    async fn discovery(&self, now: i64) -> Result<Discovery, ProviderError> {
        let mut cached = self.discovery.lock().await;
        if let Some(c) = cached.as_ref()
            && now - c.fetched_at < DOCUMENT_TTL_MS
        {
            return Ok(c.value.clone());
        }
        let url = Url::parse(&format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        ))
        .map_err(unavailable)?;
        let doc: Discovery = serde_json::from_slice(&self.get(url, "discovery document").await?)
            .map_err(|e| unavailable(format!("the discovery document is malformed: {e}")))?;
        if doc.issuer != self.config.issuer {
            return Err(unavailable(format!(
                "the discovery document names the issuer `{}`, not `{}`",
                doc.issuer, self.config.issuer
            )));
        }
        for (name, url) in [
            ("authorization_endpoint", &doc.authorization_endpoint),
            ("token_endpoint", &doc.token_endpoint),
            ("jwks_uri", &doc.jwks_uri),
        ] {
            check_url(name, url).map_err(unavailable)?;
        }
        if doc
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|m| !m.iter().any(|m| m == "S256"))
        {
            return Err(unavailable("the provider does not support PKCE with S256"));
        }
        *cached = Some(Cached {
            value: doc.clone(),
            fetched_at: now,
        });
        Ok(doc)
    }

    /// The key a token names. The keys are fetched again once they are an hour
    /// old, or when the token names a key they lack and they were last fetched
    /// long enough ago, which picks up the provider's key rotations.
    async fn key(&self, kid: Option<&str>, alg: &str, now: i64) -> Result<Value, ProviderError> {
        let mut cached = self.jwks.lock().await;
        if let Some(c) = cached.as_ref() {
            if now - c.fetched_at < DOCUMENT_TTL_MS
                && let Some(key) = find_key(&c.value, kid, alg)
            {
                return Ok(key.clone());
            }
            if now - c.fetched_at < self.jwks_min_refresh_ms.load(Ordering::Relaxed) {
                return Err(invalid(
                    "the ID token names a key the provider does not publish",
                ));
            }
        }
        let url = Url::parse(&self.discovery(now).await?.jwks_uri).map_err(unavailable)?;
        let doc: Value = serde_json::from_slice(&self.get(url, "keys").await?)
            .map_err(|e| unavailable(format!("the provider's keys are malformed: {e}")))?;
        let keys = doc["keys"]
            .as_array()
            .cloned()
            .ok_or_else(|| unavailable("the provider's key set has no `keys`"))?;
        let key = find_key(&keys, kid, alg).cloned();
        *cached = Some(Cached {
            value: keys,
            fetched_at: now,
        });
        key.ok_or_else(|| invalid("the ID token names a key the provider does not publish"))
    }

    /// Where to send a browser to sign in, carrying `state` and the login's
    /// nonce and PKCE challenge.
    pub async fn authorize_url(
        &self,
        state: &str,
        secrets: &LoginSecrets,
        now: i64,
    ) -> Result<Url, ProviderError> {
        let doc = self.discovery(now).await?;
        let mut url = Url::parse(&doc.authorization_endpoint).map_err(unavailable)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", &self.config.callback_url)
            .append_pair("scope", &self.config.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("nonce", &secrets.nonce)
            .append_pair("code_challenge", &code_challenge(&secrets.verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(url)
    }

    /// Exchanges the authorization code a login returned for the identity in its
    /// ID token. `None` when the provider refuses the code; an
    /// [`ProviderError::Invalid`] error when the ID token fails a check.
    pub async fn exchange_code(
        &self,
        code: &str,
        secrets: &LoginSecrets,
        now: i64,
    ) -> Result<Option<ExternalIdentity>, ProviderError> {
        if !is_authorization_code(code) {
            return Ok(None);
        }
        let doc = self.discovery(now).await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.config.callback_url.as_str()),
            ("code_verifier", secrets.verifier.as_str()),
        ];
        let mut req = self
            .client
            .post(Url::parse(&doc.token_endpoint).map_err(unavailable)?)
            .header("accept", "application/json");
        match &self.client_secret {
            Some(secret) => {
                req = req.basic_auth(
                    form_encode(&self.config.client_id),
                    Some(form_encode(secret)),
                );
            }
            None => form.push(("client_id", self.config.client_id.as_str())),
        }
        let res = req.form(&form).send().await.map_err(unavailable)?;
        let status = res.status();
        if status == reqwest::StatusCode::BAD_REQUEST {
            // `invalid_grant`: the code is unknown, used, expired, or was issued
            // for another verifier or redirect.
            return Ok(None);
        }
        if !status.is_success() {
            return Err(unavailable(format!(
                "the provider answered {status} for the code exchange"
            )));
        }
        let body: Value = serde_json::from_slice(&Zeroizing::new(read_bounded(res).await?))
            .map_err(|e| unavailable(format!("the token response is malformed: {e}")))?;
        let id_token = body["id_token"]
            .as_str()
            .ok_or_else(|| invalid("the token response has no ID token"))?;
        let claims = self.validate(id_token, &secrets.nonce, now).await?;
        Ok(Some(self.identity(&claims)))
    }

    /// Checks an ID token and returns its claims.
    async fn validate(&self, token: &str, nonce: &str, now: i64) -> Result<Value, ProviderError> {
        let parts: Vec<&str> = token.split('.').collect();
        let [header, payload, signature] = parts[..] else {
            return Err(invalid("the ID token is not a signed JWT"));
        };
        let decode = |part: &str, what: &str| -> Result<Value, ProviderError> {
            let bytes = B64
                .decode(part)
                .map_err(|_| invalid(format!("the ID token's {what} is malformed")))?;
            serde_json::from_slice(&bytes)
                .map_err(|_| invalid(format!("the ID token's {what} is malformed")))
        };
        let header = decode(header, "header")?;
        let alg = header["alg"].as_str().unwrap_or_default();
        if !self.config.algorithms.iter().any(|a| a == alg) {
            return Err(invalid(format!(
                "the ID token is signed with `{alg}`, which is not allowed"
            )));
        }
        if header.get("crit").is_some() {
            return Err(invalid("the ID token has critical header extensions"));
        }
        let key = self.key(header["kid"].as_str(), alg, now).await?;
        let signature = B64
            .decode(signature)
            .map_err(|_| invalid("the ID token's signature is malformed"))?;
        verify_signature(
            alg,
            &key,
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &signature,
        )
        .map_err(invalid)?;
        let claims = decode(payload, "claims")?;
        let client_id = self.config.client_id.as_str();
        if claims["iss"] != self.config.issuer.as_str() {
            return Err(invalid("the ID token is from another issuer"));
        }
        let audiences: Vec<&str> = match &claims["aud"] {
            Value::String(a) => vec![a.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !audiences.contains(&client_id) {
            return Err(invalid("the ID token is for another client"));
        }
        let azp = claims["azp"].as_str();
        if (audiences.len() > 1 || azp.is_some()) && azp != Some(client_id) {
            return Err(invalid("the ID token was issued to another client"));
        }
        let secs = now.div_euclid(1000);
        let exp = seconds(&claims, "exp").ok_or_else(|| invalid("the ID token has no `exp`"))?;
        if exp + LEEWAY_SECS <= secs {
            return Err(invalid("the ID token has expired"));
        }
        let iat = seconds(&claims, "iat").ok_or_else(|| invalid("the ID token has no `iat`"))?;
        if iat - LEEWAY_SECS > secs {
            return Err(invalid("the ID token was issued in the future"));
        }
        if seconds(&claims, "nbf").is_some_and(|nbf| nbf - LEEWAY_SECS > secs) {
            return Err(invalid("the ID token is not valid yet"));
        }
        let presented = claims["nonce"].as_str().unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), nonce.as_bytes()) {
            return Err(invalid("the ID token's nonce is not this login's"));
        }
        let sub = claims["sub"].as_str().unwrap_or_default();
        if sub.is_empty() || sub.len() > MAX_SUBJECT_LEN {
            return Err(invalid("the ID token's `sub` is missing or too long"));
        }
        Ok(claims)
    }

    /// The identity an ID token's claims name.
    fn identity(&self, claims: &Value) -> ExternalIdentity {
        let text = |name: &str, max: usize| {
            claims[name]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.len() <= max)
                .map(str::to_owned)
        };
        let verified = matches!(&claims["email_verified"], Value::Bool(true))
            || claims["email_verified"] == "true";
        let email = text("email", 254).filter(|e| verified && e.contains('@'));
        let login = text("preferred_username", 100);
        let subject = claims["sub"].as_str().unwrap_or_default().to_owned();
        let name = text("name", 200)
            .or_else(|| login.clone())
            .or_else(|| email.clone())
            .unwrap_or_else(|| subject.clone());
        ExternalIdentity {
            provider: self.config.issuer.clone(),
            subject,
            login,
            verified_emails: email.into_iter().collect(),
            name,
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pkce_challenge_follows_rfc_7636() {
        // RFC 7636, appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn keys_are_found_by_kid_or_as_the_only_fitting_key() {
        let keys = vec![
            serde_json::json!({"kty": "RSA", "kid": "a", "use": "sig"}),
            serde_json::json!({"kty": "RSA", "kid": "b", "alg": "RS512"}),
            serde_json::json!({"kty": "EC", "crv": "P-256", "kid": "c"}),
            serde_json::json!({"kty": "RSA", "kid": "d", "use": "enc"}),
        ];
        assert_eq!(find_key(&keys, Some("a"), "RS256").unwrap()["kid"], "a");
        // A key published for another algorithm or for encryption does not fit.
        assert!(find_key(&keys, Some("b"), "RS256").is_none());
        assert!(find_key(&keys, Some("d"), "RS256").is_none());
        assert!(find_key(&keys, Some("a"), "ES256").is_none());
        assert_eq!(find_key(&keys, None, "ES256").unwrap()["kid"], "c");
        // Without a `kid`, a key is chosen only when exactly one fits.
        assert_eq!(find_key(&keys, None, "RS256").unwrap()["kid"], "a");
        let two = vec![keys[0].clone(), keys[0].clone()];
        assert!(find_key(&two, None, "RS256").is_none());
    }

    #[test]
    fn client_credentials_are_form_encoded_for_basic_auth() {
        assert_eq!(form_encode("a b+c:d/é"), "a+b%2Bc%3Ad%2F%C3%A9");
    }
}
