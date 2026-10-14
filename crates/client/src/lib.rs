//! Typed client for the openporch HTTP API.
//!
//! The client only talks to servers over verified TLS. Plain HTTP is accepted for
//! loopback addresses, for local development, and nowhere else. Tokens are kept in
//! [`Token`], which never prints its value, and are never part of an [`Error`].

use std::fmt;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use zeroize::Zeroize as _;

pub use reqwest::Method;

#[cfg(test)]
mod tests;

/// An API token or session token. Its value is shown only through
/// [`Token::expose`]; `Debug` and `Display` print a placeholder.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// A token from its value, trimmed of surrounding whitespace.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let mut value: String = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            value.zeroize();
            return None;
        }
        let token = Self(trimmed.to_owned());
        value.zeroize();
        Some(token)
    }

    /// The token's value, for sending or storing it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for Token {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("empty token"))
    }
}

/// A failed API call. No variant carries a token.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server URL cannot be used.
    #[error("invalid server URL: {0}")]
    InvalidUrl(String),
    /// The request did not reach the server or its response was lost.
    #[error("cannot reach the server: {0}")]
    Transport(String),
    /// The server answered with an error.
    #[error("{message} ({code}, HTTP {status})")]
    Api {
        /// The HTTP status.
        status: u16,
        /// The error code, such as `AUTH-001`.
        code: String,
        /// The error message.
        message: String,
    },
    /// The server's response did not have the expected shape.
    #[error("unexpected response from the server: {0}")]
    Decode(String),
}

impl Error {
    /// The HTTP status of an API error.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Whether `url` names this machine, where plain HTTP is allowed.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Parses and checks a server URL: `https`, or `http` on a loopback address. The
/// result has no trailing slash, query, fragment or credentials.
pub fn server_url(url: &str) -> Result<Url, Error> {
    let parsed = Url::parse(url.trim()).map_err(|e| Error::InvalidUrl(format!("{url}: {e}")))?;
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback(&parsed) => {}
        "http" => {
            return Err(Error::InvalidUrl(format!(
                "{url}: plain HTTP is only allowed for loopback addresses; use https"
            )));
        }
        other => {
            return Err(Error::InvalidUrl(format!(
                "{url}: unsupported scheme `{other}`"
            )));
        }
    }
    if parsed.host().is_none() {
        return Err(Error::InvalidUrl(format!("{url}: no host")));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::InvalidUrl(
            "the server URL must not contain credentials".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::InvalidUrl(format!(
            "{url}: no query or fragment allowed"
        )));
    }
    let mut parsed = parsed;
    let path = parsed.path().trim_end_matches('/').to_owned();
    parsed.set_path(&path);
    Ok(parsed)
}

/// The text of a URL as [`server_url`] normalizes it, without a trailing slash.
pub fn url_key(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_owned()
}

/// An application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    /// ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Creation time in Unix milliseconds.
    #[serde(default)]
    pub created_at: i64,
    /// The creating principal.
    #[serde(default)]
    pub created_by: Option<String>,
    /// `active`, or `deleting`.
    #[serde(default)]
    pub state: String,
}

/// A started device login.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

/// A session issued by a login.
#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    /// The session token.
    pub token: Token,
    /// The member the session belongs to.
    pub user: String,
    /// When the session expires, in Unix milliseconds.
    pub expires_at: i64,
}

/// The state of a polled device login.
#[derive(Debug, Clone)]
pub enum DevicePoll {
    /// The person has not decided yet; poll again after `interval` seconds.
    Pending {
        /// Seconds to wait.
        interval: u64,
    },
    /// The person approved and the session is ready.
    Session(Session),
}

/// A client for one server and organization.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    org: String,
    token: Option<Token>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_str())
            .field("org", &self.org)
            .field("token", &self.token)
            .finish()
    }
}

impl Client {
    /// A client for `url` (see [`server_url`]) and organization `org`, sending
    /// `token` on every request when given.
    pub fn new(url: &str, org: &str, token: Option<Token>) -> Result<Self, Error> {
        let base = server_url(url)?;
        // `ring` is the crypto provider; installing it twice is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("porch/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            // A redirect could carry the token to another host.
            .redirect(reqwest::redirect::Policy::none())
            .https_only(base.scheme() == "https")
            .build()
            .map_err(|e| Error::Transport(chain(&e)))?;
        Ok(Self {
            http,
            base,
            org: org.to_owned(),
            token,
        })
    }

    /// The server URL.
    pub fn url(&self) -> &Url {
        &self.base
    }

    /// The organization.
    pub fn org(&self) -> &str {
        &self.org
    }

    /// `/orgs/{org}` followed by `suffix`.
    pub fn org_path(&self, suffix: &str) -> String {
        format!("/orgs/{}{suffix}", self.org)
    }

    /// Sends a request to `path` (starting with `/`) and returns the response's
    /// status and JSON body (`null` when empty).
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> Result<(StatusCode, Value), Error> {
        if !path.starts_with('/') {
            return Err(Error::InvalidUrl(format!(
                "path `{path}` must start with `/`"
            )));
        }
        let url = Url::parse(&format!("{}{path}", url_key(&self.base)))
            .map_err(|e| Error::InvalidUrl(format!("{path}: {e}")))?;
        let mut req = self.http.request(method, url);
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        if let Some(token) = &self.token {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
                .map_err(|_| Error::InvalidUrl("the token contains invalid characters".into()))?;
            value.set_sensitive(true);
            req = req.header(AUTHORIZATION, value);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req.send().await.map_err(|e| Error::Transport(chain(&e)))?;
        let status = res.status();
        let bytes = res.bytes().await.map_err(|e| Error::Transport(chain(&e)))?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(_) if !status.is_success() => {
                    return Err(Error::Api {
                        status: status.as_u16(),
                        code: "HTTP".into(),
                        message: status
                            .canonical_reason()
                            .unwrap_or("request failed")
                            .to_owned(),
                    });
                }
                Err(e) => return Err(Error::Decode(e.to_string())),
            }
        };
        if !status.is_success() {
            return Err(api_error(status, &value));
        }
        Ok((status, value))
    }

    /// Sends a request and returns its JSON body.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, Error> {
        Ok(self.send(method, path, body, &[]).await?.1)
    }

    async fn typed<T: DeserializeOwned>(&self, method: Method, path: &str) -> Result<T, Error> {
        let value = self.request(method, path, None).await?;
        serde_json::from_value(value).map_err(|e| Error::Decode(e.to_string()))
    }

    /// The applications the caller can see.
    pub async fn apps(&self) -> Result<Vec<App>, Error> {
        self.typed(Method::GET, &self.org_path("/apps")).await
    }

    /// The caller: ID, type, name and roles.
    pub async fn current_user(&self) -> Result<Value, Error> {
        self.request(Method::GET, &self.org_path("/current-user"), None)
            .await
    }

    /// Ends the current session, when the token is a session token.
    pub async fn end_session(&self) -> Result<(), Error> {
        self.request(Method::DELETE, &self.org_path("/sessions/current"), None)
            .await
            .map(|_| ())
    }

    /// Starts a GitHub device login.
    pub async fn start_device_login(&self) -> Result<DeviceLogin, Error> {
        let value = self
            .request(
                Method::POST,
                "/auth/github/device",
                Some(&Value::Object(Default::default())),
            )
            .await?;
        serde_json::from_value(value).map_err(|e| Error::Decode(e.to_string()))
    }

    /// Polls a device login once.
    pub async fn poll_device_login(
        &self,
        device_code: &str,
        invitation_code: Option<&str>,
    ) -> Result<DevicePoll, Error> {
        let mut body = serde_json::json!({"device_code": device_code});
        if let Some(code) = invitation_code {
            body["invitation_code"] = Value::String(code.to_owned());
        }
        let (status, value) = self
            .send(Method::POST, "/auth/github/token", Some(&body), &[])
            .await?;
        if status == StatusCode::ACCEPTED {
            let interval = value["interval"].as_u64().unwrap_or(5);
            return Ok(DevicePoll::Pending { interval });
        }
        serde_json::from_value(value)
            .map(DevicePoll::Session)
            .map_err(|e| Error::Decode(e.to_string()))
    }
}

fn api_error(status: StatusCode, body: &Value) -> Error {
    let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
    Error::Api {
        status: status.as_u16(),
        code: text("error").unwrap_or_else(|| "HTTP".into()),
        message: text("message").unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("request failed")
                .to_owned()
        }),
    }
}

/// An error and its sources, so TLS failures name their cause.
fn chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let next = s.to_string();
        if !text.contains(&next) {
            text.push_str(": ");
            text.push_str(&next);
        }
        source = s.source();
    }
    text
}
