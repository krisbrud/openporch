//! Container registries outside ECR, read through the OCI distribution API
//! without credentials: acceptance pins a public image's tag to the digest it
//! points at, and a deployment checks that a pinned digest still exists before
//! it changes anything. Images that need credentials are named by digest.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use openporch_store::secrets::BoxFuture;
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::cloud::{CloudError, pinned_image};

/// Docker Hub, the registry of images named without a host.
const DOCKER_HUB: &str = "docker.io";
/// The host Docker Hub's API answers on.
const DOCKER_HUB_API: &str = "registry-1.docker.io";
/// The manifest types a lookup accepts, image indexes first so that a
/// multi-platform image pins to its index.
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";
/// The largest manifest read to compute a digest a registry does not report.
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
/// The largest token service answer read.
const MAX_TOKEN_BYTES: usize = 64 * 1024;
/// The longest `Retry-After` honored.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// How long one registry request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What an image reference names in its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// A tag, which can move.
    Tag(String),
    /// A content digest, `sha256:<64 hex>`.
    Digest(String),
}

/// A container image in a registry, as `[<host>/]<repository>[:<tag>]` or
/// `...@sha256:<digest>`. A name without a host is on Docker Hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciImage {
    /// The image as named, without its tag or digest.
    pub name: String,
    /// The registry host, with its port if any.
    pub host: String,
    /// The repository within the registry.
    pub repository: String,
    /// The tag or digest.
    pub reference: Reference,
}

impl OciImage {
    /// `image` as a registry image; `None` when it is not a valid reference.
    pub fn parse(image: &str) -> Option<Self> {
        let (named, reference) = match image.split_once('@') {
            Some((named, digest)) => {
                if !pinned_image(image) {
                    return None;
                }
                (named, Reference::Digest(digest.to_ascii_lowercase()))
            }
            None => (image, Reference::Tag(String::new())),
        };
        // A tag follows the last `:` after the last `/`.
        let last = named.rsplit('/').next().unwrap_or(named);
        let (name, tag) = match last.rsplit_once(':') {
            Some((_, tag)) => (&named[..named.len() - tag.len() - 1], Some(tag)),
            None => (named, None),
        };
        let reference = match reference {
            Reference::Tag(_) => Reference::Tag(tag.unwrap_or("latest").to_owned()),
            digest => digest,
        };
        if let Reference::Tag(tag) = &reference {
            let valid = !tag.is_empty()
                && tag.len() <= 128
                && !tag.starts_with(['.', '-'])
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
            if !valid {
                return None;
            }
        }
        // The first component is a host when it looks like one.
        let (host, repository) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_ascii_lowercase(), rest.to_owned())
            }
            _ => (DOCKER_HUB.to_owned(), name.to_owned()),
        };
        let valid = !repository.is_empty()
            && repository.split('/').all(|part| {
                !part.is_empty()
                    && part.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                    })
                    && part.as_bytes()[0].is_ascii_alphanumeric()
            })
            && valid_host(&host);
        if !valid {
            return None;
        }
        let (host, repository) = match host.as_str() {
            DOCKER_HUB | "index.docker.io" | DOCKER_HUB_API => {
                let repository = if repository.contains('/') {
                    repository
                } else {
                    format!("library/{repository}")
                };
                (DOCKER_HUB.to_owned(), repository)
            }
            _ => (host, repository),
        };
        Some(Self {
            name: name.to_owned(),
            host,
            repository,
            reference,
        })
    }

    /// The image pinned to `digest`, keeping the name it was given.
    pub fn at(&self, digest: &str) -> String {
        format!("{}@{digest}", self.name)
    }

    /// The host the registry's API answers on.
    fn api_host(&self) -> &str {
        if self.host == DOCKER_HUB {
            DOCKER_HUB_API
        } else {
            &self.host
        }
    }
}

impl std::fmt::Display for OciImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reference {
            Reference::Tag(tag) => write!(f, "{}:{tag}", self.name),
            Reference::Digest(digest) => write!(f, "{}@{digest}", self.name),
        }
    }
}

/// What looking an image up found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The image exists; the digest of its manifest.
    Found(String),
    /// The repository, tag or digest does not exist.
    Missing,
    /// The registry does not show the image without credentials.
    Private,
}

/// Reads images from registries.
pub trait Registry: Send + Sync {
    /// Looks `image` up: the digest its tag points at, or whether its digest
    /// exists.
    fn lookup<'a>(&'a self, image: &'a OciImage) -> BoxFuture<'a, Result<Lookup, CloudError>>;
}

/// A registry client that reads anonymously over HTTPS, or plain HTTP for
/// the hosts named insecure. Only registries named insecure or private are
/// read on loopback, private or link-local addresses, so an image name cannot
/// make the server probe its own network.
#[derive(Debug, Clone)]
pub struct HttpRegistry {
    /// Reaches public addresses only.
    client: Client,
    /// Also reaches the private addresses of the hosts in `private`; used only
    /// for exactly those `host[:port]`s.
    private_client: Client,
    insecure: BTreeSet<String>,
    private: BTreeSet<String>,
}

/// Resolves names to public addresses only, except for the hosts listed.
struct PublicOnly {
    /// Host names, without ports, that may resolve to private addresses.
    allowed: BTreeSet<String>,
}

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let any = self.allowed.contains(&host);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| any || is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!(
                    "`{host}` resolves to no public address; list it in \
                     `images.private_registries` to read it"
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether `ip` is a public address, rather than loopback, private,
/// link-local, shared, unspecified or otherwise not globally routed.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public(IpAddr::V4(v4)),
            // Global unicast only (2000::/3), less the documentation range,
            // the special-purpose 2001::/23, and 6to4 addresses that wrap a
            // non-public IPv4 address.
            None => {
                let [first, second, third, ..] = v6.segments();
                (first & 0xe000) == 0x2000
                    && !(first == 0x2001 && second < 0x0200)
                    && !(first == 0x2001 && second == 0x0db8)
                    && (first != 0x2002
                        || is_public(IpAddr::V4(std::net::Ipv4Addr::new(
                            (second >> 8) as u8,
                            second as u8,
                            (third >> 8) as u8,
                            third as u8,
                        ))))
            }
        },
    }
}

/// Whether `host` is `name[:port]` or `[ipv6][:port]`.
fn valid_host(host: &str) -> bool {
    let port_ok = |port: Option<&str>| {
        port.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    if let Some(rest) = host.strip_prefix('[') {
        let Some((address, after)) = rest.split_once(']') else {
            return false;
        };
        let port = match after {
            "" => None,
            after => match after.strip_prefix(':') {
                Some(port) => Some(port),
                None => return false,
            },
        };
        return address.parse::<std::net::Ipv6Addr>().is_ok() && port_ok(port);
    }
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        && port_ok(port)
}

/// The host of `host[:port]`, without its port or IPv6 brackets.
fn host_name(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_owned();
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name.to_owned(),
        _ => host,
    }
}

/// A bearer token from a registry's token service.
#[derive(Deserialize)]
struct Token {
    token: Option<String>,
    access_token: Option<String>,
}

impl HttpRegistry {
    /// A client that reaches the hosts in `insecure` (`host[:port]`) over
    /// plain HTTP and every other registry over HTTPS, and reads the hosts in
    /// `insecure` and `private` on private addresses too.
    pub fn new(
        insecure: impl IntoIterator<Item = String>,
        private: impl IntoIterator<Item = String>,
    ) -> Result<Self, CloudError> {
        let insecure: BTreeSet<String> = insecure
            .into_iter()
            .map(|h| h.to_ascii_lowercase())
            .collect();
        let private: BTreeSet<String> = private
            .into_iter()
            .map(|h| h.to_ascii_lowercase())
            .chain(insecure.iter().cloned())
            .collect();
        // `ring` is the crypto provider; installing it twice is harmless.
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Redirects are not followed: one could lead anywhere.
        let client = |allowed: BTreeSet<String>| {
            Client::builder()
                .user_agent(concat!("openporch/", env!("CARGO_PKG_VERSION")))
                .redirect(reqwest::redirect::Policy::none())
                .timeout(REQUEST_TIMEOUT)
                .dns_resolver(Arc::new(PublicOnly { allowed }))
                .build()
                .map_err(|e| CloudError::Invalid(format!("no registry client: {e}")))
        };
        Ok(Self {
            client: client(BTreeSet::new())?,
            private_client: client(private.iter().map(|h| host_name(h)).collect())?,
            insecure,
            private,
        })
    }

    /// Refuses `url` when its host is an address literal that is not public,
    /// unless the registry `host` is allowed private addresses. Names are
    /// checked as they resolve.
    fn reachable(&self, url: &reqwest::Url, host: &str) -> Result<(), CloudError> {
        let literal = match url.host() {
            Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        match literal {
            Some(ip) if !is_public(ip) && !self.private.contains(host) => {
                Err(CloudError::Rejected(format!(
                    "registry `{host}` is on a private address; list it in \
                     `images.private_registries` to read it"
                )))
            }
            _ => Ok(()),
        }
    }

    /// The client for `host[:port]`: one that reaches private addresses only
    /// for a host listed with exactly that port.
    fn client(&self, host: &str) -> &Client {
        if self.private.contains(host) {
            &self.private_client
        } else {
            &self.client
        }
    }

    fn scheme(&self, host: &str) -> &'static str {
        if self.insecure.contains(host) {
            "http"
        } else {
            "https"
        }
    }

    async fn lookup_image(&self, image: &OciImage) -> Result<Lookup, CloudError> {
        let host = image.api_host();
        let reference = match &image.reference {
            Reference::Tag(tag) => tag,
            Reference::Digest(digest) => digest,
        };
        let url = format!(
            "{}://{host}/v2/{}/manifests/{reference}",
            self.scheme(host),
            image.repository
        );
        let parsed = reqwest::Url::parse(&url)
            .map_err(|_| CloudError::Rejected(format!("`{image}` names no valid registry")))?;
        self.reachable(&parsed, host)?;
        let unavailable = |e: reqwest::Error| unreachable_registry(host, &e);
        let request = |method: reqwest::Method, token: Option<&str>| {
            let mut r = self
                .client(host)
                .request(method, &url)
                .header(header::ACCEPT, MANIFEST_TYPES);
            if let Some(token) = token {
                r = r.bearer_auth(token);
            }
            r
        };
        let mut response = request(reqwest::Method::HEAD, None)
            .send()
            .await
            .map_err(unavailable)?;
        let mut token = None;
        if response.status() == StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let Some(t) = self.anonymous_token(image, challenge.as_deref()).await? else {
                return Ok(Lookup::Private);
            };
            response = request(reqwest::Method::HEAD, Some(&t))
                .send()
                .await
                .map_err(unavailable)?;
            token = Some(t);
        }
        match response.status() {
            s if s.is_success() => {}
            StatusCode::NOT_FOUND => return Ok(Lookup::Missing),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Ok(Lookup::Private),
            StatusCode::TOO_MANY_REQUESTS => {
                return Err(throttled(
                    format!("registry `{host}` answered 429 for `{image}`"),
                    &response,
                ));
            }
            s if s.is_server_error() => {
                return Err(CloudError::Unavailable(format!(
                    "registry `{host}` answered {s} for `{image}`"
                )));
            }
            s => {
                return Err(CloudError::Rejected(format!(
                    "registry `{host}` answered {s} for `{image}`"
                )));
            }
        }
        let reported = response
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase);
        let digest = match reported {
            Some(d) => d,
            // Not every registry reports the digest; it is the manifest's
            // SHA-256.
            None => {
                let mut response = request(reqwest::Method::GET, token.as_deref())
                    .send()
                    .await
                    .map_err(unavailable)?;
                if !response.status().is_success() {
                    return Err(CloudError::Unavailable(format!(
                        "registry `{host}` answered {} reading `{image}`",
                        response.status()
                    )));
                }
                let mut hasher = Sha256::new();
                let mut read = 0;
                while let Some(chunk) = response.chunk().await.map_err(unavailable)? {
                    read += chunk.len();
                    if read > MAX_MANIFEST_BYTES {
                        return Err(CloudError::Rejected(format!(
                            "the manifest of `{image}` is larger than {MAX_MANIFEST_BYTES} bytes"
                        )));
                    }
                    hasher.update(&chunk);
                }
                let hex: String = hasher
                    .finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                format!("sha256:{hex}")
            }
        };
        if let Reference::Digest(asked) = &image.reference
            && *asked != digest
        {
            return Err(CloudError::Rejected(format!(
                "registry `{host}` answered `{image}` with digest `{digest}`"
            )));
        }
        Ok(Lookup::Found(digest))
    }

    /// An anonymous pull token from the token service a `Bearer` challenge
    /// names; `None` when the registry offers none.
    async fn anonymous_token(
        &self,
        image: &OciImage,
        challenge: Option<&str>,
    ) -> Result<Option<String>, CloudError> {
        let Some(params) = challenge.and_then(bearer_params) else {
            return Ok(None);
        };
        let Some(realm) = params.iter().find(|(k, _)| k == "realm").map(|(_, v)| v) else {
            return Ok(None);
        };
        let Ok(mut realm) = reqwest::Url::parse(realm) else {
            return Ok(None);
        };
        // A token is fetched over HTTPS, unless the registry itself is reached
        // over plain HTTP.
        let insecure = realm
            .host_str()
            .is_some_and(|h| self.insecure.contains(&realm_host(h, realm.port())));
        if realm.scheme() != "https" && !insecure {
            return Ok(None);
        }
        let realm_host = realm_host(realm.host_str().unwrap_or_default(), realm.port());
        self.reachable(&realm, &realm_host)?;
        {
            let mut query = realm.query_pairs_mut();
            if let Some((_, service)) = params.iter().find(|(k, _)| k == "service") {
                query.append_pair("service", service);
            }
            query.append_pair("scope", &format!("repository:{}:pull", image.repository));
        }
        let response = self
            .client(&realm_host)
            .get(realm)
            .send()
            .await
            .map_err(|e| unreachable_registry(&image.host, &e))?;
        match response.status() {
            s if s.is_success() => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Ok(None),
            StatusCode::TOO_MANY_REQUESTS => {
                return Err(throttled(
                    format!(
                        "the token service of registry `{}` answered 429",
                        image.host
                    ),
                    &response,
                ));
            }
            s => {
                return Err(CloudError::Unavailable(format!(
                    "the token service of registry `{}` answered {s}",
                    image.host
                )));
            }
        }
        let unavailable = |e: reqwest::Error| unreachable_registry(&image.host, &e);
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unavailable)? {
            if body.len() + chunk.len() > MAX_TOKEN_BYTES {
                return Err(CloudError::Rejected(format!(
                    "the token service of registry `{}` answered with more than \
                     {MAX_TOKEN_BYTES} bytes",
                    image.host
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let token: Token = serde_json::from_slice(&body).map_err(|e| {
            CloudError::Unavailable(format!(
                "the token service of registry `{}` answered unreadably: {e}",
                image.host
            ))
        })?;
        Ok(token.token.or(token.access_token))
    }
}

/// A request that failed, with the causes reqwest leaves out of its message.
fn unreachable_registry(host: &str, e: &reqwest::Error) -> CloudError {
    let mut message = format!("registry `{host}`: {e}");
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    CloudError::Unavailable(message)
}

/// A `429`, with the wait its `Retry-After` names in seconds.
fn throttled(message: String, response: &reqwest::Response) -> CloudError {
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .map(|secs| Duration::from_secs(secs).min(MAX_RETRY_AFTER));
    CloudError::Throttled {
        message,
        retry_after,
    }
}

/// `host[:port]` as named in the insecure list.
fn realm_host(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("{host}:{port}").to_ascii_lowercase(),
        None => host.to_ascii_lowercase(),
    }
}

/// The parameters of a `Bearer` challenge: `Bearer realm="...",service="..."`.
fn bearer_params(challenge: &str) -> Option<Vec<(String, String)>> {
    let (scheme, rest) = challenge.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut params = Vec::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let after = after.trim_start();
        let (value, after) = match after.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted.find('"')?;
                (&quoted[..end], &quoted[end + 1..])
            }
            None => match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            },
        };
        params.push((key.trim().to_ascii_lowercase(), value.to_owned()));
        rest = after.trim_start().trim_start_matches(',').trim_start();
    }
    Some(params)
}

impl Registry for HttpRegistry {
    fn lookup<'a>(&'a self, image: &'a OciImage) -> BoxFuture<'a, Result<Lookup, CloudError>> {
        Box::pin(self.lookup_image(image))
    }
}

#[cfg(test)]
mod tests;
