//! HTTP tests for `oidc-provider`: discovery, the JWKS, tokens that validate
//! against it, key rotation, and `aws-identity` accounts exchanging the
//! issuer's tokens for role sessions.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use axum::http::{Method, StatusCode};
use openporch_core::model::CloudAccountType;
use openporch_engine::cloud::{
    AssumeRole, AssumeRoleWithWebIdentity, AwsCredentials, CallerIdentity, CloudError, Sts,
};
use openporch_engine::oidc::{self, Claims, Issuer, MAX_TOKEN_LIFETIME};
use openporch_store::secrets::{BoxFuture, SecretValue};
use openporch_store::{AccountStore, now_ms};
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

// --- aws-identity accounts ---------------------------------------------------

/// STS as AWS runs it for a role whose trust policy names this issuer and
/// `subject`: the token must verify against the issuer's published JWKS and
/// carry that subject and audience `sts.amazonaws.com`.
struct TrustingSts {
    issuer: OnceLock<Arc<Issuer>>,
    subject: String,
    tokens: Mutex<Vec<String>>,
}

impl Sts for TrustingSts {
    fn assume_role(&self, _: AssumeRole) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async { panic!("no host identity is used") })
    }

    fn assume_role_with_web_identity(
        &self,
        r: AssumeRoleWithWebIdentity,
    ) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async move {
            let issuer = self.issuer.get().expect("the issuer is on");
            let now = now_ms();
            let claims = oidc::verify(r.token.expose(), &issuer.jwks(now), issuer.url(), now)
                .map_err(|e| {
                    CloudError::Rejected(format!(
                        "STS AssumeRoleWithWebIdentity failed: InvalidIdentityToken: {e}"
                    ))
                })?;
            if claims["sub"] != self.subject.as_str() || claims["aud"] != "sts.amazonaws.com" {
                return Err(CloudError::Rejected(
                    "STS AssumeRoleWithWebIdentity failed: AccessDenied: Not authorized to \
                     perform sts:AssumeRoleWithWebIdentity"
                        .into(),
                ));
            }
            self.tokens
                .lock()
                .unwrap()
                .push(r.token.expose().to_owned());
            Ok(AwsCredentials {
                access_key_id: "ASIAWEBIDENTITY".into(),
                secret_access_key: SecretValue::new("web-identity-secret"),
                session_token: Some(SecretValue::new("web-identity-session")),
                region: None,
                expires_at: Some(SystemTime::now() + r.duration),
            })
        })
    }

    fn caller_identity<'a>(
        &'a self,
        c: &'a AwsCredentials,
    ) -> BoxFuture<'a, Result<CallerIdentity, CloudError>> {
        Box::pin(async move {
            assert_eq!(c.access_key_id, "ASIAWEBIDENTITY");
            Ok(CallerIdentity {
                arn: "arn:aws:sts::111122223333:assumed-role/openporch/openporch-aws-prod".into(),
                account: "111122223333".into(),
                user_id: "AROAEXAMPLE:openporch-aws-prod".into(),
            })
        })
    }
}

const ACCOUNTS: &str = "/orgs/acme/resources/accounts";
const ROLE: &str = "arn:aws:iam::111122223333:role/openporch";

async fn trusting(subject: &str) -> (Server, Arc<TrustingSts>) {
    let dir = tempfile::tempdir().unwrap();
    let sts = Arc::new(TrustingSts {
        issuer: OnceLock::new(),
        subject: subject.into(),
        tokens: Mutex::new(Vec::new()),
    });
    let state = crate::start_with(issuer_config(&dir), None, sts.clone())
        .await
        .unwrap();
    sts.issuer
        .set(state.issuer.clone().expect("the issuer is on"))
        .unwrap();
    let s = server_from(dir, state);
    let (status, b) = s
        .post(
            ACCOUNTS,
            &s.admin,
            json!({"id": "aws-prod", "type": "aws-identity",
                   "config": {"role_arn": ROLE, "region": "eu-north-1"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    (s, sts)
}

#[tokio::test]
async fn role_assumption_without_standing_credentials() {
    let (s, sts) = trusting("acme/aws-prod").await;
    let (status, check) = s
        .post(&format!("{ACCOUNTS}/aws-prod/check"), &s.admin, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{check}");
    assert_eq!(check["account"], "111122223333");
    assert_eq!(check["setup"]["subject"], "acme/aws-prod");
    assert_eq!(check["setup"]["issuer"], ISSUER);
    // One token was exchanged; it never reaches the API.
    let tokens = sts.tokens.lock().unwrap().clone();
    assert_eq!(tokens.len(), 1);
    assert!(!check.to_string().contains(&tokens[0]));

    // Drivers receive the temporary credentials.
    let account = s
        .state
        .store
        .account("acme", "aws-prod")
        .await
        .unwrap()
        .unwrap()
        .account;
    let primary = crate::secret_inputs::primary(&s.state).unwrap();
    let credentials = s
        .state
        .cloud
        .resolve(
            &account,
            s.state.cloud.generation(),
            &primary,
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
    let inputs = credentials.driver_inputs();
    assert_eq!(inputs["AccessKeyId"].expose(), "ASIAWEBIDENTITY");
    assert_eq!(inputs["SessionToken"].expose(), "web-identity-session");
    assert_eq!(inputs["Region"].expose(), "eu-north-1");

    // openporch stores no AWS key for the account.
    assert_eq!(account.ty, CloudAccountType::AwsIdentity);
    assert!(account.secret_refs.is_empty());
    let (versions,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM secret_versions WHERE ref LIKE 'accounts/aws-prod/%'")
            .fetch_one(s.state.store.pool())
            .await
            .unwrap();
    assert_eq!(versions, 0);
    // Secrets cannot be given to it either.
    let (status, b) = s
        .call(
            Method::PUT,
            &format!("{ACCOUNTS}/aws-prod"),
            Some(&s.admin),
            Some(json!({"type": "aws-identity", "config": {"role_arn": ROLE},
                        "secrets": {"secret_access_key": "wJalr"}})),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
}

#[tokio::test]
async fn trust_scoped_to_another_account() {
    let (s, sts) = trusting("acme/aws-dev").await;
    let (status, b) = s
        .post(&format!("{ACCOUNTS}/aws-prod/check"), &s.admin, json!({}))
        .await;
    assert_error(status, &b, StatusCode::UNPROCESSABLE_ENTITY, "CLD-001");
    let message = b["message"].as_str().unwrap();
    assert!(
        message.contains("AssumeRoleWithWebIdentity failed: AccessDenied"),
        "{b}"
    );
    assert!(
        message.contains("porch cloud-accounts setup aws-identity aws-prod"),
        "{b}"
    );
    assert!(sts.tokens.lock().unwrap().is_empty());
}

#[tokio::test]
async fn setup_prints_the_trust_policy_scoped_to_the_account() {
    let (s, _) = trusting("acme/aws-prod").await;
    let member = logged_in(&s, "583231", "octocat", "member").await;
    let (status, setup) = s
        .get("/orgs/acme/oidc/aws-identity/aws-prod", &member)
        .await;
    assert_eq!(status, StatusCode::OK, "{setup}");
    assert_eq!(setup["provider_url"], ISSUER);
    assert_eq!(
        setup["provider_arn"],
        "arn:aws:iam::111122223333:oidc-provider/porch.example.com/oidc"
    );
    assert!(
        setup["thumbprint"]
            .as_str()
            .unwrap()
            .contains("certificate")
    );
    let statement = &setup["trust_policy"]["Statement"][0];
    assert_eq!(statement["Action"], "sts:AssumeRoleWithWebIdentity");
    assert_eq!(
        statement["Principal"]["Federated"],
        "arn:aws:iam::111122223333:oidc-provider/porch.example.com/oidc"
    );
    assert_eq!(
        statement["Condition"]["StringEquals"],
        json!({"porch.example.com/oidc:sub": "acme/aws-prod",
               "porch.example.com/oidc:aud": "sts.amazonaws.com"})
    );
    // An account that does not exist yet gets the same policy with a
    // placeholder for the AWS account.
    let (status, setup) = s.get("/orgs/acme/oidc/aws-identity/aws-new", &member).await;
    assert_eq!(status, StatusCode::OK, "{setup}");
    assert_eq!(
        setup["provider_arn"],
        "arn:aws:iam::<AWS_ACCOUNT_ID>:oidc-provider/porch.example.com/oidc"
    );
    assert_eq!(
        setup["trust_policy"]["Statement"][0]["Condition"]["StringEquals"]["porch.example.com/oidc:sub"],
        "acme/aws-new"
    );
    // Without the issuer there is nothing to trust.
    let s = server().await;
    let (status, b) = s
        .get("/orgs/acme/oidc/aws-identity/aws-new", &s.admin)
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
}

/// The opt-in check against real AWS (see `docs/cloud-accounts.md`): a server
/// whose issuer is exposed at `OPENPORCH_TEST_OIDC_PUBLIC_URL` checks an
/// `aws-identity` account for a role that trusts it, then checks that the same
/// role refuses another account's subject.
#[tokio::test]
#[ignore = "needs a publicly reachable issuer and an AWS role trusting it"]
async fn real_identity_account_check() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let public_url = var("OPENPORCH_TEST_OIDC_PUBLIC_URL");
    let listen: std::net::SocketAddr = var("OPENPORCH_TEST_OIDC_LISTEN").parse().unwrap();
    let role_arn = var("OPENPORCH_TEST_AWS_IDENTITY_ROLE_ARN");
    let dir = match std::env::var("OPENPORCH_TEST_OIDC_DATA_DIR") {
        Ok(path) => {
            std::fs::create_dir_all(&path).unwrap();
            tempfile::Builder::new().tempdir_in(&path).unwrap()
        }
        Err(_) => tempfile::tempdir().unwrap(),
    };
    let mut config = Config::new("acme", dir.path());
    config.images.pin_other_registries = false;
    config.public_url = Some(public_url);
    let s = server_with(dir, config).await;
    let listener = tokio::net::TcpListener::bind(listen).await.unwrap();
    let router = s.router.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    let (_, setup) = s
        .get("/orgs/acme/oidc/aws-identity/aws-prod", &s.admin)
        .await;
    println!("{}", serde_json::to_string_pretty(&setup).unwrap());
    for id in ["aws-prod", "aws-other"] {
        let (status, b) = s
            .post(
                ACCOUNTS,
                &s.admin,
                json!({"id": id, "type": "aws-identity", "config": {"role_arn": role_arn}}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{b}");
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let check = loop {
        let (status, b) = s
            .post(&format!("{ACCOUNTS}/aws-prod/check"), &s.admin, json!({}))
            .await;
        if status == StatusCode::OK || std::time::Instant::now() > deadline {
            break (status, b);
        }
        println!("waiting for the role to trust the issuer: {b}");
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    };
    assert_eq!(check.0, StatusCode::OK, "{}", check.1);
    assert!(
        check.1["arn"].as_str().unwrap().contains(":assumed-role/"),
        "{}",
        check.1
    );
    let (status, b) = s
        .post(&format!("{ACCOUNTS}/aws-other/check"), &s.admin, json!({}))
        .await;
    assert_error(status, &b, StatusCode::UNPROCESSABLE_ENTITY, "CLD-001");
    assert!(
        b["message"].as_str().unwrap().contains("AccessDenied"),
        "{b}"
    );
}
