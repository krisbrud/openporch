//! HTTP tests for the issuer half of `oidc-provider`: discovery, the JWKS,
//! tokens that validate against it, and key rotation.

use axum::http::{Method, StatusCode};
use openporch_engine::oidc::{self, Claims, MAX_TOKEN_LIFETIME};
use openporch_store::now_ms;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

const PUBLIC_URL: &str = "https://porch.example.com/";
const ISSUER: &str = "https://porch.example.com/oidc";
const DISCOVERY: &str = "/oidc/.well-known/openid-configuration";
const ROTATE: &str = "/orgs/acme/oidc/keys/rotate";

fn issuer_config(dir: &TempDir) -> Config {
    let mut config = Config::new("acme", dir.path());
    config.images.pin_other_registries = false;
    config.public_url = Some(PUBLIC_URL.into());
    config
}

async fn issuing(dir: TempDir) -> Server {
    let config = issuer_config(&dir);
    server_with(dir, config).await
}

fn mint(s: &Server) -> String {
    let claims = Claims {
        sub: "acme/aws-prod".into(),
        aud: "sts.amazonaws.com".into(),
        lifetime: MAX_TOKEN_LIFETIME,
    };
    s.state
        .issuer
        .as_ref()
        .expect("the issuer is on")
        .mint(&claims, now_ms())
        .unwrap()
}

fn kids(jwks: &Value) -> Vec<String> {
    jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kid"].as_str().unwrap().to_owned())
        .collect()
}

fn header_kid(token: &str) -> String {
    use base64::Engine as _;
    let header = token.split('.').next().unwrap();
    let header: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header)
            .unwrap(),
    )
    .unwrap();
    header["kid"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn discovery_and_jwks_are_public_and_validate_issued_tokens() {
    let s = issuing(tempfile::tempdir().unwrap()).await;
    let (status, doc) = s.call(Method::GET, DISCOVERY, None, None).await;
    assert_eq!(status, StatusCode::OK, "{doc}");
    assert_eq!(doc["issuer"], ISSUER);
    assert_eq!(doc["jwks_uri"], format!("{ISSUER}/jwks"));
    assert_eq!(
        doc["id_token_signing_alg_values_supported"],
        json!(["RS256"])
    );
    assert!(
        !doc["response_types_supported"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let (status, jwks) = s.call(Method::GET, "/oidc/jwks", None, None).await;
    assert_eq!(status, StatusCode::OK, "{jwks}");
    assert_eq!(kids(&jwks).len(), 1, "{jwks}");
    // Only public key components are published.
    for key in jwks["keys"].as_array().unwrap() {
        for private in ["d", "p", "q", "dp", "dq", "qi"] {
            assert!(key.get(private).is_none(), "{key}");
        }
        assert_eq!(key["kty"], "RSA");
        assert_eq!(key["alg"], "RS256");
    }
    let token = mint(&s);
    let claims = oidc::verify(&token, &jwks, ISSUER, now_ms()).unwrap();
    assert_eq!(claims["sub"], "acme/aws-prod");
    assert_eq!(claims["aud"], "sts.amazonaws.com");
    // Credentials are not needed, and a bad one changes nothing.
    let (status, _) = s
        .call(Method::GET, "/oidc/jwks", Some("opt_wrong"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn without_a_public_url_the_issuer_is_off() {
    let s = server().await;
    assert!(s.state.issuer.is_none());
    for path in [DISCOVERY, "/oidc/jwks"] {
        let (status, body) = s.call(Method::GET, path, None, None).await;
        assert_error(status, &body, StatusCode::NOT_FOUND, "API-001");
        assert!(
            body["message"].as_str().unwrap().contains("public_url"),
            "{body}"
        );
    }
    let (status, body) = s.post(ROTATE, &s.admin, json!({})).await;
    assert_error(status, &body, StatusCode::NOT_FOUND, "API-001");
    // No key is generated for an issuer that is off.
    assert!(
        s.state
            .store
            .signing_keys(crate::state::OIDC_KEY_PURPOSE)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn rotation_signs_with_the_new_key_and_keeps_the_old_one_published_for_its_grace() {
    let s = issuing(tempfile::tempdir().unwrap()).await;
    let (_, jwks) = s.call(Method::GET, "/oidc/jwks", None, None).await;
    let old = kids(&jwks).remove(0);
    let before = mint(&s);
    assert_eq!(header_kid(&before), old);

    // Only administrators rotate.
    let member = logged_in(&s, "583231", "octocat", "member").await;
    let manager = logged_in(&s, "583232", "hubot", "manager").await;
    for token in [&member, &manager] {
        let (status, body) = s.post(ROTATE, token, json!({})).await;
        assert_error(status, &body, StatusCode::FORBIDDEN, "AUTH-002");
    }
    let (_, still) = s.call(Method::GET, "/oidc/jwks", None, None).await;
    assert_eq!(kids(&still), std::slice::from_ref(&old));

    let started = now_ms();
    let (status, rotated) = s.post(ROTATE, &s.admin, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let new = rotated["signing_kid"].as_str().unwrap().to_owned();
    assert_ne!(new, old);
    // The old key is unpublished after the configured grace period, a day.
    let keys = rotated["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2, "{rotated}");
    assert_eq!(keys[0]["kid"], new.as_str());
    assert!(keys[0].get("retires_at").is_none(), "{rotated}");
    assert_eq!(keys[1]["kid"], old.as_str());
    let retires_at = keys[1]["retires_at"].as_i64().unwrap();
    let day = 24 * 60 * 60 * 1000;
    assert!((started + day..=now_ms() + day).contains(&retires_at));

    // New tokens use the new key; the JWKS lists both, so tokens signed
    // before the rotation still validate.
    let after = mint(&s);
    assert_eq!(header_kid(&after), new);
    let (_, jwks) = s.call(Method::GET, "/oidc/jwks", None, None).await;
    assert_eq!(kids(&jwks), [new.clone(), old.clone()]);
    for token in [&before, &after] {
        oidc::verify(token, &jwks, ISSUER, now_ms()).unwrap();
    }

    // Rotation is audited.
    let (_, audit) = s
        .get(
            "/orgs/acme/audit-logs?path_prefix=/orgs/acme/oidc",
            &s.admin,
        )
        .await;
    let entries = audit["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["action"] == "oidc.key.rotate" && e["outcome"] == "succeeded"),
        "{audit}"
    );

    // The keys survive a restart, sealed in the database: the new key still
    // signs and the old one is still published.
    let Server {
        dir, state, router, ..
    } = s;
    drop((state, router));
    let s = issuing(dir).await;
    assert_eq!(header_kid(&mint(&s)), new);
    let (_, jwks) = s.call(Method::GET, "/oidc/jwks", None, None).await;
    assert_eq!(kids(&jwks), [new.clone(), old.clone()]);

    // Once the grace period has ended the old key is no longer published,
    // and the next rotation removes it.
    sqlx::query("UPDATE signing_keys SET retired_at = ? WHERE retired_at IS NOT NULL")
        .bind(now_ms() - 1)
        .execute(s.state.store.pool())
        .await
        .unwrap();
    let Server {
        dir, state, router, ..
    } = s;
    drop((state, router));
    let s = issuing(dir).await;
    let (_, jwks) = s.call(Method::GET, "/oidc/jwks", None, None).await;
    assert_eq!(kids(&jwks), std::slice::from_ref(&new));
    assert!(oidc::verify(&before, &jwks, ISSUER, now_ms()).is_err());
    let (status, rotated) = s.post(ROTATE, &s.admin, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM signing_keys WHERE purpose = 'oidc'")
            .fetch_one(s.state.store.pool())
            .await
            .unwrap();
    assert_eq!(
        count, 2,
        "the expired key is removed, the replaced one retires"
    );
}

#[tokio::test]
async fn the_public_url_must_be_https_and_the_grace_at_least_a_token_lifetime() {
    let dir = tempfile::tempdir().unwrap();
    for (url, expected) in [
        ("http://porch.example.com", "https"),
        ("http://127.0.0.1:8080", "https"),
        ("https://user:pw@porch.example.com", "credentials"),
        ("https://porch.example.com/?a=b", "query"),
        ("https://porch.example.com/#x", "fragment"),
        ("porch.example.com", "not a URL"),
        ("https://porch.example.com/openporch", "path"),
        ("https://porch.example.com:8443", "port"),
    ] {
        let mut config = issuer_config(&dir);
        config.public_url = Some(url.into());
        let e = config.validate().unwrap_err().to_string();
        assert!(e.contains(expected), "{url}: {e}");
    }
    let mut config = issuer_config(&dir);
    // The default port is the same URL.
    config.public_url = Some("https://porch.example.com:443/".into());
    config.validate().unwrap();
    assert_eq!(
        config.oidc_issuer().as_deref(),
        Some("https://porch.example.com/oidc")
    );
    for minutes in [14, 30 * 24 * 60 + 1] {
        let mut config = issuer_config(&dir);
        config.oidc.key_grace_minutes = minutes;
        let e = config.validate().unwrap_err().to_string();
        assert!(e.contains("oidc.key_grace_minutes"), "{e}");
    }
}
