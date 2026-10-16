use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::*;

#[test]
fn image_references_are_parsed_like_docker_does() {
    let hex = "Ab".repeat(32);
    for (image, host, repository, reference) in [
        (
            "nginx",
            "docker.io",
            "library/nginx",
            Reference::Tag("latest".into()),
        ),
        (
            "nginx:1.27",
            "docker.io",
            "library/nginx",
            Reference::Tag("1.27".into()),
        ),
        (
            "acme/web:1",
            "docker.io",
            "acme/web",
            Reference::Tag("1".into()),
        ),
        (
            "ghcr.io/acme/web:abc123",
            "ghcr.io",
            "acme/web",
            Reference::Tag("abc123".into()),
        ),
        (
            "localhost:5000/web",
            "localhost:5000",
            "web",
            Reference::Tag("latest".into()),
        ),
        (
            "[::1]:5000/web:1",
            "[::1]:5000",
            "web",
            Reference::Tag("1".into()),
        ),
        (
            "index.docker.io/nginx:1",
            "docker.io",
            "library/nginx",
            Reference::Tag("1".into()),
        ),
    ] {
        let parsed = OciImage::parse(image).unwrap_or_else(|| panic!("{image}"));
        assert_eq!(
            (
                parsed.host.as_str(),
                parsed.repository.as_str(),
                parsed.reference
            ),
            (host, repository, reference),
            "{image}"
        );
    }
    // A digest is kept with the name it was given, lower-cased; a tag before
    // it is dropped.
    let pinned = OciImage::parse(&format!("ghcr.io/acme/web:1@sha256:{hex}")).unwrap();
    assert_eq!(
        pinned.reference,
        Reference::Digest(format!("sha256:{}", hex.to_ascii_lowercase()))
    );
    assert_eq!(pinned.name, "ghcr.io/acme/web");
    let tagged = OciImage::parse("nginx:1").unwrap();
    let digest = format!("sha256:{}", "0".repeat(64));
    assert_eq!(tagged.at(&digest), format!("nginx@{digest}"));
    assert_eq!(tagged.tagged_at(&digest), format!("nginx:1@{digest}"));
    assert!(pinned_from_tag(&tagged.tagged_at(&digest)));
    assert!(!pinned_from_tag(&tagged.at(&digest)));
    assert!(!pinned_from_tag(&format!("localhost:5000/web@{digest}")));
    assert_eq!(tagged.to_string(), "nginx:1");

    for invalid in [
        "",
        ".",
        "Web:1",
        "web:",
        "web:-1",
        "ghcr.io/",
        "ghcr.io//web",
        "${resources.registry.outputs.host}/web:1",
        "web@sha256:abc",
        "web@md5:0123",
        "[::1/web:1",
        "[nope]:5000/web:1",
        "[::1]x/web:1",
        "[::1]:/web:1",
        "host:port/web:1",
        "host:70000/web:1",
    ] {
        assert_eq!(OciImage::parse(invalid), None, "{invalid}");
    }
}

#[test]
fn bearer_challenges_are_read() {
    assert_eq!(
        bearer_params(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#
        )
        .unwrap(),
        [
            ("realm".to_owned(), "https://auth.docker.io/token".to_owned()),
            ("service".to_owned(), "registry.docker.io".to_owned()),
            (
                "scope".to_owned(),
                "repository:library/nginx:pull".to_owned()
            ),
        ]
    );
    assert_eq!(
        bearer_params("bearer realm=https://r/token, service=r").unwrap(),
        [
            ("realm".to_owned(), "https://r/token".to_owned()),
            ("service".to_owned(), "r".to_owned()),
        ]
    );
    assert_eq!(bearer_params(r#"Basic realm="r""#), None);
}

/// The digest the fake registry reports for `web:1`.
fn web_digest() -> String {
    format!("sha256:{}", "1".repeat(64))
}

/// A registry that wants an anonymous token for every repository: `web` has
/// tag `1` (reported by digest) and tag `quiet` (whose digest is not
/// reported), `private` gives no anonymous token, `busy` is rate limited
/// and the token for `huge` is too large to read.
async fn fake_registry() -> String {
    async fn token(Query(q): Query<Vec<(String, String)>>) -> Response {
        let scope = q
            .iter()
            .find(|(k, _)| k == "scope")
            .map(|(_, v)| v.as_str());
        match scope {
            Some("repository:private:pull") => StatusCode::UNAUTHORIZED.into_response(),
            Some("repository:huge:pull") => {
                let token = "x".repeat(2 * MAX_TOKEN_BYTES);
                axum::Json(serde_json::json!({ "token": token })).into_response()
            }
            Some(_) => axum::Json(serde_json::json!({"token": "anonymous"})).into_response(),
            None => StatusCode::BAD_REQUEST.into_response(),
        }
    }
    async fn manifest(
        State(host): State<Arc<String>>,
        Path((repository, reference)): Path<(String, String)>,
        method: axum::http::Method,
        headers: HeaderMap,
    ) -> Response {
        if headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some("Bearer anonymous")
        {
            let challenge = format!(r#"Bearer realm="http://{host}/token",service="fake""#);
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, challenge)],
            )
                .into_response();
        }
        if !headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("application/vnd.oci.image.index.v1+json"))
        {
            return StatusCode::NOT_ACCEPTABLE.into_response();
        }
        let digest = web_digest();
        match (repository.as_str(), reference.as_str()) {
            ("web", "1") => ([("docker-content-digest", digest)], "{}").into_response(),
            ("web", r) if r == digest => {
                ([("docker-content-digest", digest)], "{}").into_response()
            }
            ("web", "quiet") => "quiet manifest".into_response(),
            ("slow", _) if method == axum::http::Method::HEAD => "slow manifest".into_response(),
            ("slow", _) => {
                (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "9")]).into_response()
            }
            ("busy", _) => {
                (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "7")]).into_response()
            }
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let app = Router::new()
        .route("/token", get(token))
        .route("/v2/{repository}/manifests/{reference}", get(manifest))
        .with_state(Arc::new(host.clone()));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    host
}

#[tokio::test]
async fn a_public_image_is_looked_up_with_an_anonymous_token() {
    let host = fake_registry().await;
    let registry = HttpRegistry::new([host.clone()], []).unwrap();
    let lookup = async |image: String| registry.lookup(&OciImage::parse(&image).unwrap()).await;

    assert_eq!(
        lookup(format!("{host}/web:1")).await.unwrap(),
        Lookup::Found(web_digest())
    );
    // A digest the registry does not report is the manifest's SHA-256.
    let quiet: String = Sha256::digest(b"quiet manifest")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        lookup(format!("{host}/web:quiet")).await.unwrap(),
        Lookup::Found(format!("sha256:{quiet}"))
    );
    assert_eq!(
        lookup(format!("{host}/web@{}", web_digest()))
            .await
            .unwrap(),
        Lookup::Found(web_digest())
    );
    assert_eq!(
        lookup(format!("{host}/web:2")).await.unwrap(),
        Lookup::Missing
    );
    assert_eq!(
        lookup(format!("{host}/web@sha256:{}", "2".repeat(64)))
            .await
            .unwrap(),
        Lookup::Missing
    );
    assert_eq!(
        lookup(format!("{host}/private:1")).await.unwrap(),
        Lookup::Private
    );
    // A rate limit says how long to wait.
    assert_eq!(
        lookup(format!("{host}/busy:1")).await.unwrap_err(),
        CloudError::Throttled {
            message: format!("registry `{host}` answered 429 for `{host}/busy:1`"),
            retry_after: Some(Duration::from_secs(7)),
        }
    );
    // So does one on reading a manifest whose digest is not reported.
    assert_eq!(
        lookup(format!("{host}/slow:1")).await.unwrap_err(),
        CloudError::Throttled {
            message: format!("registry `{host}` answered 429 reading `{host}/slow:1`"),
            retry_after: Some(Duration::from_secs(9)),
        }
    );
    // A token service answer is read only up to a limit.
    let e = lookup(format!("{host}/huge:1")).await.unwrap_err();
    assert!(
        matches!(&e, CloudError::Rejected(m) if m.contains("more than")),
        "{e:?}"
    );

    // Named private, the registry is reached on its loopback address, but
    // only over HTTPS, which it does not speak.
    let private = HttpRegistry::new([], [host.clone()]).unwrap();
    let e = private
        .lookup(&OciImage::parse(&format!("{host}/web:1")).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(e, CloudError::Unavailable(_)), "{e:?}");
    // Named neither, it is not reached at all, by address or by a name that
    // resolves to it.
    let strict = HttpRegistry::new([], []).unwrap();
    let e = strict
        .lookup(&OciImage::parse(&format!("{host}/web:1")).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&e, CloudError::Rejected(m) if m.contains("private address")),
        "{e:?}"
    );
    let port = host.rsplit_once(':').unwrap().1;
    let e = strict
        .lookup(&OciImage::parse(&format!("localhost:{port}/web:1")).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&e, CloudError::Unavailable(m) if m.contains("no public address")),
        "{e:?}"
    );
    // Listed with one port, a name reaches its private address on that port
    // only.
    let named = format!("localhost:{port}");
    // (The token service is named by address, so it is listed too.)
    let listed = HttpRegistry::new([named.clone(), host.clone()], []).unwrap();
    assert_eq!(
        listed
            .lookup(&OciImage::parse(&format!("{named}/web:1")).unwrap())
            .await
            .unwrap(),
        Lookup::Found(web_digest())
    );
    let other = HttpRegistry::new(["localhost:1".to_owned()], []).unwrap();
    let e = other
        .lookup(&OciImage::parse(&format!("{named}/web:1")).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&e, CloudError::Unavailable(m) if m.contains("no public address")),
        "{e:?}"
    );
}

#[test]
fn only_public_addresses_are_public() {
    for public in [
        "8.8.8.8",
        "140.82.112.33",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
        "2a00:1450:4001::1",
        "2002:0808:0808::1",
    ] {
        assert!(is_public(public.parse().unwrap()), "{public}");
    }
    for private in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "255.255.255.255",
        "::1",
        "::",
        "fd00::1",
        "fe80::1",
        "::ffff:169.254.169.254",
        "fec0::1",
        "2001:db8::1",
        "3fff::1",
        "2001::1",
        "2002:0a00:0001::1",
        "64:ff9b::a00:1",
        "ff02::1",
        "192.0.0.8",
        "192.88.99.1",
        "224.0.0.251",
    ] {
        assert!(!is_public(private.parse().unwrap()), "{private}");
    }
    assert_eq!(host_name("Registry.local:5000"), "registry.local");
    assert_eq!(host_name("[::1]:5000"), "::1");
    assert_eq!(host_name("ghcr.io"), "ghcr.io");
}
