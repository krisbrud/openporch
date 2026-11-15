//! The openporch OpenID Connect issuer: RS256 signing keys, the discovery
//! document, the JWKS and identity tokens for cloud accounts.
//!
//! Keys are generated with the `rsa` crate, which is only used for that, and
//! every signature is made by `ring`, whose RSA private-key operations run in
//! constant time. Only public keys ever leave this module.

use std::sync::RwLock;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The path under the server's public URL where the issuer lives.
pub const ISSUER_PATH: &str = "/oidc";
/// The longest an identity token may live.
pub const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// The RSA modulus size of generated keys, in bits.
pub const KEY_BITS: usize = 2048;

/// Errors from the issuer.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    /// A stored key could not be read.
    #[error("OIDC signing key `{0}` is malformed")]
    Malformed(String),
    /// A key could not be generated.
    #[error("could not generate an OIDC signing key: {0}")]
    Generate(String),
    /// There is no key to sign with.
    #[error("the OIDC issuer has no active signing key")]
    NoKey,
    /// A token was asked for with a lifetime above [`MAX_TOKEN_LIFETIME`].
    #[error("identity tokens live at most {} seconds", MAX_TOKEN_LIFETIME.as_secs())]
    Lifetime,
    /// Signing failed.
    #[error("signing an identity token failed")]
    Sign,
}

/// Generates a new private key as PKCS#8 DER. This takes a moment; call it
/// from a blocking task.
pub fn generate_key() -> Result<Vec<u8>, OidcError> {
    use rsa::pkcs8::EncodePrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, KEY_BITS)
        .map_err(|e| OidcError::Generate(e.to_string()))?;
    let der = key
        .to_pkcs8_der()
        .map_err(|e| OidcError::Generate(e.to_string()))?;
    Ok(der.as_bytes().to_vec())
}

/// A stored signing key, as the issuer loads it.
#[derive(Debug, Clone)]
pub struct StoredKey {
    /// PKCS#8 DER of the private key.
    pub pkcs8: Vec<u8>,
    /// When it was created, in milliseconds since the epoch.
    pub created_at: i64,
    /// When it stops being published, for a key that has been rotated out.
    pub retires_at: Option<i64>,
}

struct Key {
    kid: String,
    pair: RsaKeyPair,
    n: Vec<u8>,
    e: Vec<u8>,
    created_at: i64,
    retires_at: Option<i64>,
}

impl Key {
    fn load(stored: &StoredKey) -> Result<Self, OidcError> {
        let pair = RsaKeyPair::from_pkcs8(&stored.pkcs8)
            .map_err(|e| OidcError::Malformed(e.to_string()))?;
        let public = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(pair.public());
        let (n, e) = (public.n, public.e);
        Ok(Self {
            kid: thumbprint(&n, &e),
            pair,
            n,
            e,
            created_at: stored.created_at,
            retires_at: stored.retires_at,
        })
    }

    fn published(&self, now: i64) -> bool {
        self.retires_at.is_none_or(|at| at > now)
    }

    fn jwk(&self) -> Value {
        json!({
            "kty": "RSA",
            "alg": "RS256",
            "use": "sig",
            "kid": self.kid,
            "n": B64.encode(&self.n),
            "e": B64.encode(&self.e),
        })
    }
}

/// The key ID of an RSA public key: its RFC 7638 JWK thumbprint.
fn thumbprint(n: &[u8], e: &[u8]) -> String {
    // The members in lexicographic order, without whitespace.
    let canonical = format!(
        r#"{{"e":"{}","kty":"RSA","n":"{}"}}"#,
        B64.encode(e),
        B64.encode(n)
    );
    B64.encode(Sha256::digest(canonical.as_bytes()))
}

/// A published key, without its key material.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct KeyInfo {
    /// The key ID.
    pub kid: String,
    /// When it was created, in milliseconds since the epoch.
    pub created_at: i64,
    /// When it is unpublished, for a key that has been rotated out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retires_at: Option<i64>,
}

/// The claims of an identity token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// The subject.
    pub sub: String,
    /// The audience.
    pub aud: String,
    /// How long the token is valid.
    pub lifetime: Duration,
}

/// The issuer. Its keys are replaced as a whole when they are rotated.
pub struct Issuer {
    url: String,
    keys: RwLock<Vec<Key>>,
    rng: SystemRandom,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issuer")
            .field("url", &self.url)
            .field("kids", &self.kids(i64::MIN))
            .finish()
    }
}

impl Issuer {
    /// An issuer at `<public_url>/oidc` signing with `keys`.
    pub fn new(public_url: &str, keys: &[StoredKey]) -> Result<Self, OidcError> {
        let issuer = Self {
            url: format!("{}{ISSUER_PATH}", public_url.trim_end_matches('/')),
            keys: RwLock::new(Vec::new()),
            rng: SystemRandom::new(),
        };
        issuer.replace(keys)?;
        Ok(issuer)
    }

    /// The issuer URL, the `iss` of every token.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Replaces the keys, after a rotation.
    pub fn replace(&self, keys: &[StoredKey]) -> Result<(), OidcError> {
        let mut loaded = keys.iter().map(Key::load).collect::<Result<Vec<_>, _>>()?;
        // Newest first: the newest key that is not retiring signs.
        loaded.sort_by_key(|k| std::cmp::Reverse(k.created_at));
        *self.keys.write().expect("issuer keys") = loaded;
        Ok(())
    }

    /// The IDs of the keys published at `now`, newest first.
    pub fn kids(&self, now: i64) -> Vec<String> {
        self.keys
            .read()
            .expect("issuer keys")
            .iter()
            .filter(|k| k.published(now))
            .map(|k| k.kid.clone())
            .collect()
    }

    /// The keys published at `now`, newest first.
    pub fn published(&self, now: i64) -> Vec<KeyInfo> {
        self.keys
            .read()
            .expect("issuer keys")
            .iter()
            .filter(|k| k.published(now))
            .map(|k| KeyInfo {
                kid: k.kid.clone(),
                created_at: k.created_at,
                retires_at: k.retires_at,
            })
            .collect()
    }

    /// The ID of the key new tokens are signed with.
    pub fn signing_kid(&self) -> Option<String> {
        self.keys
            .read()
            .expect("issuer keys")
            .iter()
            .find(|k| k.retires_at.is_none())
            .map(|k| k.kid.clone())
    }

    /// The discovery document served at
    /// `<issuer>/.well-known/openid-configuration`.
    pub fn discovery(&self) -> Value {
        json!({
            "issuer": self.url,
            "jwks_uri": format!("{}/jwks", self.url),
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "scopes_supported": ["openid"],
            "claims_supported": ["iss", "sub", "aud", "iat", "nbf", "exp", "jti"],
        })
    }

    /// The public keys published at `now`: the signing key and keys rotated
    /// out whose grace period has not ended.
    pub fn jwks(&self, now: i64) -> Value {
        let keys = self.keys.read().expect("issuer keys");
        let keys: Vec<Value> = keys
            .iter()
            .filter(|k| k.published(now))
            .map(Key::jwk)
            .collect();
        json!({ "keys": keys })
    }

    /// Mints an identity token issued at `now` (milliseconds since the epoch).
    pub fn mint(&self, claims: &Claims, now: i64) -> Result<String, OidcError> {
        if claims.lifetime > MAX_TOKEN_LIFETIME || claims.lifetime.is_zero() {
            return Err(OidcError::Lifetime);
        }
        let keys = self.keys.read().expect("issuer keys");
        let key = keys
            .iter()
            .find(|k| k.retires_at.is_none())
            .ok_or(OidcError::NoKey)?;
        let iat = now.div_euclid(1000);
        let exp = iat + i64::try_from(claims.lifetime.as_secs()).unwrap_or(i64::MAX);
        let mut jti = [0u8; 16];
        ring::rand::SecureRandom::fill(&self.rng, &mut jti).map_err(|_| OidcError::Sign)?;
        let header = json!({"alg": "RS256", "typ": "JWT", "kid": key.kid});
        let payload = json!({
            "iss": self.url,
            "sub": claims.sub,
            "aud": claims.aud,
            "iat": iat,
            "nbf": iat,
            "exp": exp,
            "jti": B64.encode(jti),
        });
        let input = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(payload.to_string())
        );
        let mut signature = vec![0u8; key.pair.public().modulus_len()];
        key.pair
            .sign(
                &RSA_PKCS1_SHA256,
                &self.rng,
                input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| OidcError::Sign)?;
        Ok(format!("{input}.{}", B64.encode(signature)))
    }
}

#[cfg(test)]
mod tests;

/// Checks a token against a published JWKS the way a relying party does: the
/// header names `RS256` and a published key, the signature verifies with that
/// key, `iss` is `issuer` and `now` (milliseconds) is inside `nbf..exp`.
/// Returns the claims.
pub fn verify(token: &str, jwks: &Value, issuer: &str, now: i64) -> Result<Value, String> {
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts[..] else {
        return Err("a JWT has three parts".into());
    };
    let decode = |part: &str| -> Result<Value, String> {
        let bytes = B64.decode(part).map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    };
    let header = decode(header)?;
    if header["alg"] != "RS256" {
        return Err(format!("unexpected algorithm {}", header["alg"]));
    }
    let kid = header["kid"].as_str().ok_or("the header names no key")?;
    let key = jwks["keys"]
        .as_array()
        .and_then(|keys| keys.iter().find(|k| k["kid"] == kid))
        .ok_or_else(|| format!("key `{kid}` is not published"))?;
    let component = |name: &str| -> Result<Vec<u8>, String> {
        B64.decode(key[name].as_str().ok_or("key component missing")?)
            .map_err(|e| e.to_string())
    };
    let public = ring::signature::RsaPublicKeyComponents {
        n: component("n")?,
        e: component("e")?,
    };
    let signature = B64.decode(signature).map_err(|e| e.to_string())?;
    public
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &signature,
        )
        .map_err(|_| "the signature does not verify".to_string())?;
    let claims = decode(payload)?;
    if claims["iss"] != issuer {
        return Err(format!("unexpected issuer {}", claims["iss"]));
    }
    let secs = now.div_euclid(1000);
    let (Some(nbf), Some(exp)) = (claims["nbf"].as_i64(), claims["exp"].as_i64()) else {
        return Err("the token has no validity period".into());
    };
    if secs < nbf || secs >= exp {
        return Err("the token is not valid now".into());
    }
    Ok(claims)
}
