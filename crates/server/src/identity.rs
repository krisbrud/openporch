//! External identity providers. A provider verifies who a person is; [`crate::login`]
//! maps the verified identity to a member. Milestone 1 has GitHub with the device
//! flow; generic OIDC follows later behind the same trait.
//!
//! Identities are keyed by the provider's immutable subject (for GitHub, the numeric
//! user ID). Login names are display metadata only, so a renamed or reused login never
//! moves membership.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use openporch_store::ExternalIdentity;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::config::GithubConfig;

/// A boxed provider future.
pub type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>;

/// Provider failures. Messages never contain tokens.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The provider could not be reached or answered unexpectedly.
    #[error("the identity provider is unavailable: {0}")]
    Unavailable(String),
    /// The request was rejected because it is malformed.
    #[error("{0}")]
    Invalid(String),
}

/// A started device login: the person opens `verification_uri` and enters `user_code`,
/// while the client polls with `device_code`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLogin {
    /// The code the client polls with.
    pub device_code: String,
    /// The code the person enters.
    pub user_code: String,
    /// Where the person enters the code.
    pub verification_uri: String,
    /// Seconds until the codes expire.
    pub expires_in: u64,
    /// The minimum number of seconds between polls.
    pub interval: u64,
}

/// The state of a device login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePoll {
    /// The person has not approved yet; poll again after `interval` seconds.
    Pending {
        /// Seconds to wait before the next poll.
        interval: u64,
    },
    /// The person denied the request.
    Denied,
    /// The codes expired or are unknown.
    Expired,
    /// The person approved; this is who they are.
    Verified(ExternalIdentity),
}

/// An external identity provider.
pub trait IdentityProvider: Send + Sync + std::fmt::Debug {
    /// The provider ID used in identities, such as `github`.
    fn id(&self) -> &'static str;

    /// Starts a device login.
    fn start_device_login(&self) -> ProviderFuture<'_, DeviceLogin>;

    /// Polls a device login.
    fn poll_device_login<'a>(&'a self, device_code: &'a str) -> ProviderFuture<'a, DevicePoll>;

    /// Resolves a login name to the identity that holds it now, if any.
    fn resolve_login<'a>(&'a self, login: &'a str) -> ProviderFuture<'a, Option<ExternalIdentity>>;
}

/// Whether `login` is a well-formed GitHub login: 1 to 39 ASCII letters, digits or
/// single hyphens, not starting or ending with a hyphen.
pub fn is_github_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Device codes are opaque provider strings; bound their size and alphabet before
/// forwarding them.
fn is_device_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 256
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Checks a provider base URL: HTTPS, or plain HTTP only on a loopback host for local
/// development and tests.
pub fn check_base_url(name: &str, url: &str) -> Result<Url, String> {
    let mut parsed = Url::parse(url).map_err(|e| format!("`{name}` is not a URL: {e}"))?;
    let host = parsed.host_str().unwrap_or_default();
    let loopback = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => {
            return Err(format!(
                "`{name}` must use https; plain http is only allowed on loopback addresses"
            ));
        }
    }
    // Relative joins keep the whole base path.
    if !parsed.path().ends_with('/') {
        let path = format!("{}/", parsed.path());
        parsed.set_path(&path);
    }
    Ok(parsed)
}

/// GitHub through its OAuth device flow and REST API.
#[derive(Debug)]
pub struct Github {
    client: reqwest::Client,
    client_id: String,
    api: Url,
    web: Url,
}

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SCOPES: &str = "read:user user:email";
/// GitHub's largest page of email addresses, and a bound on how many pages are read.
const EMAIL_PAGE_SIZE: usize = 100;
const EMAIL_PAGES: usize = 10;

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct GithubUser {
    id: u64,
    login: String,
    name: Option<String>,
    /// `User`, `Organization` or `Bot`.
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

fn unavailable(e: impl std::fmt::Display) -> ProviderError {
    ProviderError::Unavailable(e.to_string())
}

impl Github {
    /// A GitHub provider for `config`.
    pub fn new(config: &GithubConfig) -> Result<Self, String> {
        let api = check_base_url("github.api_url", &config.api_url)?;
        let web = check_base_url("github.web_url", &config.web_url)?;
        // `ring` is the crypto provider; installing it twice is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .user_agent(concat!("openporch/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .https_only(api.scheme() == "https" && web.scheme() == "https")
            .build()
            .map_err(|e| format!("building the GitHub client: {e}"))?;
        Ok(Self {
            client,
            client_id: config.client_id.clone(),
            api,
            web,
        })
    }

    fn url(base: &Url, path: &str) -> Result<Url, ProviderError> {
        base.join(path).map_err(unavailable)
    }

    async fn api_get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> Result<Option<T>, ProviderError> {
        let mut req = self
            .client
            .get(Self::url(&self.api, path)?)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.map_err(unavailable)?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(unavailable(format!("GitHub answered {}", res.status())));
        }
        res.json().await.map(Some).map_err(unavailable)
    }

    async fn identity(&self, token: &str) -> Result<ExternalIdentity, ProviderError> {
        let user: GithubUser = self
            .api_get("user", Some(token))
            .await?
            .ok_or_else(|| unavailable("GitHub did not return the user"))?;
        // Every page of addresses, so an invited address on a later page still counts.
        let mut emails: Vec<GithubEmail> = Vec::new();
        for page in 1..=EMAIL_PAGES {
            let batch: Vec<GithubEmail> = self
                .api_get(
                    &format!("user/emails?per_page={EMAIL_PAGE_SIZE}&page={page}"),
                    Some(token),
                )
                .await?
                .unwrap_or_default();
            let last = batch.len() < EMAIL_PAGE_SIZE;
            emails.extend(batch);
            if last {
                break;
            }
        }
        // Only addresses GitHub has verified, primary first.
        let mut verified: Vec<GithubEmail> = emails.into_iter().filter(|e| e.verified).collect();
        verified.sort_by_key(|e| !e.primary);
        let verified_emails = verified.into_iter().map(|e| e.email).collect();
        Ok(ExternalIdentity {
            provider: "github".into(),
            subject: user.id.to_string(),
            name: user.name.unwrap_or_else(|| user.login.clone()),
            login: Some(user.login),
            verified_emails,
        })
    }
}

impl IdentityProvider for Github {
    fn id(&self) -> &'static str {
        "github"
    }

    fn start_device_login(&self) -> ProviderFuture<'_, DeviceLogin> {
        Box::pin(async move {
            let res = self
                .client
                .post(Self::url(&self.web, "login/device/code")?)
                .header("accept", "application/json")
                .form(&[("client_id", self.client_id.as_str()), ("scope", SCOPES)])
                .send()
                .await
                .map_err(unavailable)?;
            if !res.status().is_success() {
                return Err(unavailable(format!("GitHub answered {}", res.status())));
            }
            res.json().await.map_err(unavailable)
        })
    }

    fn poll_device_login<'a>(&'a self, device_code: &'a str) -> ProviderFuture<'a, DevicePoll> {
        Box::pin(async move {
            if !is_device_code(device_code) {
                return Err(ProviderError::Invalid("`device_code` is malformed".into()));
            }
            let res = self
                .client
                .post(Self::url(&self.web, "login/oauth/access_token")?)
                .header("accept", "application/json")
                .form(&[
                    ("client_id", self.client_id.as_str()),
                    ("device_code", device_code),
                    ("grant_type", DEVICE_GRANT),
                ])
                .send()
                .await
                .map_err(unavailable)?;
            if !res.status().is_success() {
                return Err(unavailable(format!("GitHub answered {}", res.status())));
            }
            let body: TokenResponse = res.json().await.map_err(unavailable)?;
            // The GitHub token only proves who the person is; it is dropped (and
            // zeroed) once the identity is read and never stored or returned.
            if let Some(token) = body.access_token.map(Zeroizing::new) {
                return Ok(DevicePoll::Verified(self.identity(&token).await?));
            }
            match body.error.as_deref() {
                Some("authorization_pending") => Ok(DevicePoll::Pending {
                    interval: body.interval.unwrap_or(5),
                }),
                Some("slow_down") => Ok(DevicePoll::Pending {
                    interval: body.interval.unwrap_or(10),
                }),
                Some("access_denied") => Ok(DevicePoll::Denied),
                Some("expired_token" | "incorrect_device_code" | "bad_verification_code") => {
                    Ok(DevicePoll::Expired)
                }
                Some(other) => Err(unavailable(format!("GitHub reported `{other}`"))),
                None => Err(unavailable("GitHub returned neither a token nor an error")),
            }
        })
    }

    fn resolve_login<'a>(&'a self, login: &'a str) -> ProviderFuture<'a, Option<ExternalIdentity>> {
        Box::pin(async move {
            if !is_github_login(login) {
                return Err(ProviderError::Invalid(format!(
                    "`{login}` is not a valid GitHub login"
                )));
            }
            let user: Option<GithubUser> = self.api_get(&format!("users/{login}"), None).await?;
            // Organizations and bots cannot log in, so they cannot be invited.
            let user = user.filter(|u| u.kind.as_deref().is_none_or(|k| k == "User"));
            Ok(user.map(|u| ExternalIdentity {
                provider: "github".into(),
                subject: u.id.to_string(),
                name: u.name.unwrap_or_else(|| u.login.clone()),
                login: Some(u.login),
                verified_emails: Vec::new(),
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_logins() {
        for ok in ["octocat", "a", "a-b-c", "A1", &"x".repeat(39)] {
            assert!(is_github_login(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            "a--b",
            "a/b",
            "../x",
            "a b",
            "é",
            &"x".repeat(40),
        ] {
            assert!(!is_github_login(bad), "{bad}");
        }
    }

    #[test]
    fn base_urls_need_tls_off_loopback() {
        assert!(check_base_url("u", "https://api.github.com").is_ok());
        assert!(check_base_url("u", "http://127.0.0.1:9000").is_ok());
        assert!(check_base_url("u", "http://localhost:9000").is_ok());
        assert!(check_base_url("u", "http://api.github.com").is_err());
        assert!(check_base_url("u", "ftp://x").is_err());
        assert!(check_base_url("u", "http://[::1]:9000").is_ok());
        let base = check_base_url("u", "https://ghe.example.com/api/v3").unwrap();
        assert_eq!(
            base.join("users/octocat").unwrap().as_str(),
            "https://ghe.example.com/api/v3/users/octocat"
        );
    }
}
