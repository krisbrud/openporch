//! HTTP tests for `cloud-accounts` (create, delete-in-use) and the store, grant and
//! credential-graph scenarios of `secret-stores-and-references`.

use std::time::SystemTime;

use axum::http::{Method, StatusCode};
use openporch_engine::cloud::{AssumeRole, AwsCredentials, CallerIdentity, CloudError, Sts};
use openporch_store::secrets::{BoxFuture, SecretValue};
use serde_json::{Value, json};

use super::{Server, assert_error, server};

const ACCOUNTS: &str = "/orgs/acme/resources/accounts";
const STORES: &str = "/orgs/acme/secretstores";

impl Server {
    async fn send(&self, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(method, path, Some(&self.admin), Some(body)).await
    }

    async fn ok(&self, method: Method, path: &str, body: Value) -> Value {
        let (s, b) = self.send(method, path, body).await;
        assert!(s.is_success(), "{s} {b}");
        b
    }

    pub(super) async fn static_account(&self, id: &str) -> Value {
        self.ok(
            Method::POST,
            ACCOUNTS,
            json!({"id": id, "type": "aws", "config": {"access_key_id": "AKIAEXAMPLE"},
                   "secrets": {"secret_access_key": "wJalr-secret"}}),
        )
        .await
    }

    async fn aws_store(&self, id: &str, account: &str) -> Value {
        self.ok(
            Method::POST,
            STORES,
            json!({"id": id, "type": "aws-secrets-manager",
                   "config": {"region": "eu-north-1", "cloud_account": account}}),
        )
        .await
    }

    async fn secret_versions(&self, prefix: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM secret_versions WHERE ref LIKE ?")
            .bind(format!("{prefix}%"))
            .fetch_one(self.state.store.pool())
            .await
            .unwrap()
    }
}

fn message(b: &Value) -> &str {
    b["message"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn create_a_static_account() {
    let s = server().await;
    let created = s.static_account("aws-dev").await;
    let (status, read) = s
        .get(
            &format!("{ACCOUNTS}/aws-dev"),
            &s.member("mia", "member").await,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    for body in [&created, &read] {
        assert!(!body.to_string().contains("wJalr-secret"), "{body}");
        assert_eq!(body["type"], "aws");
        assert_eq!(body["config"]["access_key_id"], "AKIAEXAMPLE");
        assert_eq!(
            body["secret_refs"]["secret_access_key"],
            json!({"store": "openporch", "ref": "accounts/aws-dev/secret_access_key", "version": "1"})
        );
        assert!(body.get("secrets").is_none(), "{body}");
    }

    // The value is in the primary store.
    use openporch_store::secrets::SecretStore as _;
    let primary = crate::secret_inputs::primary(&s.state).unwrap();
    let r = serde_json::from_value(read["secret_refs"]["secret_access_key"].clone()).unwrap();
    assert_eq!(primary.read(&r).await.unwrap().expose(), "wJalr-secret");

    let (_, list) = s.get(ACCOUNTS, &s.admin).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (status, b) = s
        .send(
            Method::POST,
            ACCOUNTS,
            json!({"id": "aws-dev", "type": "aws", "config": {"access_key_id": "A"},
                   "secrets": {"secret_access_key": "other"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // Updating writes a new version; the type is fixed.
    let b = s
        .ok(
            Method::PUT,
            &format!("{ACCOUNTS}/aws-dev"),
            json!({"type": "aws", "name": "Dev", "config": {"access_key_id": "AKIANEW"},
                   "secret_refs": {"secret_access_key": {"value": "rotated-secret"}}}),
        )
        .await;
    assert_eq!(b["name"], "Dev");
    assert_eq!(b["secret_refs"]["secret_access_key"]["version"], "2");
    assert!(!b.to_string().contains("rotated-secret"));
    let (status, b) = s
        .send(
            Method::PUT,
            &format!("{ACCOUNTS}/aws-dev"),
            json!({"type": "aws-role", "config": {"role_arn": "arn:aws:iam::1:role/x", "external_id": "e"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) = s
        .send(
            Method::PUT,
            &format!("{ACCOUNTS}/nope"),
            json!({"type": "aws", "config": {"access_key_id": "A"},
                   "secrets": {"secret_access_key": "x"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");

    s.ok(Method::DELETE, &format!("{ACCOUNTS}/aws-dev"), Value::Null)
        .await;
    let (status, b) = s.get(&format!("{ACCOUNTS}/aws-dev"), &s.admin).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn create_a_role_account() {
    let s = server().await;
    let b = s
        .ok(
            Method::POST,
            ACCOUNTS,
            json!({"id": "aws-prod", "type": "aws-role",
                   "config": {"role_arn": "arn:aws:iam::123456789012:role/openporch",
                              "external_id": "ext-123", "session_duration": 3600}}),
        )
        .await;
    assert_eq!(b["type"], "aws-role");
    assert_eq!(b["config"]["external_id"], "ext-123");
    assert!(b.get("secret_refs").is_none(), "{b}");

    // Invalid bodies are rejected with the key named.
    for (body, named) in [
        (
            json!({"id": "x1", "type": "aws-role", "config": {"role_arn": "arn:aws:iam::1:role/r"}}),
            "external_id",
        ),
        (
            json!({"id": "x1", "type": "aws-role", "config": {"role_arn": "role/r", "external_id": "e"}}),
            "role_arn",
        ),
        (
            json!({"id": "x1", "type": "aws-role", "config": {"role_arn": "arn:r", "external_id": "e", "session_duration": 60}}),
            "session_duration",
        ),
        (
            json!({"id": "x1", "type": "aws-role", "config": {"role_arn": "arn:r", "external_id": "e", "colour": "red"}}),
            "colour",
        ),
        (
            json!({"id": "x1", "type": "aws-role", "config": {"role_arn": "arn:r", "external_id": "e"},
                "secrets": {"secret_access_key": "hunter2"}}),
            "secret_access_key",
        ),
        (
            json!({"id": "x1", "type": "aws", "config": {"access_key_id": "A"}}),
            "secret_access_key",
        ),
        (
            json!({"id": "x1", "type": "aws", "config": {"access_key_id": "A"},
                "secrets": {"secret_access_key": "hunter2"},
                "secret_refs": {"secret_access_key": {"store": "openporch", "ref": "k"}}}),
            "secret_access_key",
        ),
        (
            json!({"id": "x1", "type": "aws", "config": {"access_key_id": "A"},
                "secret_refs": {"secret_access_key": {"store": "openporch", "ref": "${context.env.id}"}}}),
            "placeholders",
        ),
        (
            json!({"id": "x1", "type": "aws", "config": {"access_key_id": "A"},
                "secret_refs": {"secret_access_key": {"store": "prod-sm", "ref": "k"}}}),
            "prod-sm",
        ),
        (json!({"type": "aws"}), "id"),
    ] {
        let (status, b) = s.send(Method::POST, ACCOUNTS, body).await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        assert!(message(&b).contains(named), "{named}: {b}");
        assert!(!b.to_string().contains("hunter2"), "{b}");
    }
    let (status, b) = s
        .send(
            Method::POST,
            ACCOUNTS,
            json!({"id": "x", "type": "aws-role", "config": {"role_arn": "arn:r", "external_id": "e"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-002");
    assert_eq!(s.secret_versions("accounts/x1/").await, 0);
}

#[tokio::test]
async fn unavailable_account_types_name_the_missing_capability() {
    let s = server().await;
    let (status, b) = s
        .send(
            Method::POST,
            ACCOUNTS,
            json!({"id": "aws-oidc", "type": "aws-identity", "config": {}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(message(&b).contains("OIDC provider"), "{b}");
}

#[tokio::test]
async fn only_administrators_change_accounts_and_stores() {
    let s = server().await;
    s.static_account("aws-dev").await;
    for role in ["manager", "member"] {
        let token = s.member(&format!("u-{role}"), role).await;
        for (method, path, body) in [
            (
                Method::POST,
                ACCOUNTS.to_owned(),
                json!({"id": "aws-x", "type": "aws"}),
            ),
            (
                Method::PUT,
                format!("{ACCOUNTS}/aws-dev"),
                json!({"type": "aws"}),
            ),
            (Method::DELETE, format!("{ACCOUNTS}/aws-dev"), Value::Null),
            (
                Method::POST,
                STORES.to_owned(),
                json!({"id": "sm", "type": "aws-ssm"}),
            ),
            (Method::PATCH, format!("{STORES}/openporch"), json!({})),
            (Method::DELETE, format!("{STORES}/openporch"), Value::Null),
            (
                Method::POST,
                format!("{STORES}/openporch/grants"),
                json!({"ref": "k", "org_wide": true}),
            ),
        ] {
            let body = (!body.is_null()).then_some(body);
            let (status, b) = s.call(method, &path, Some(&token), body).await;
            assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
        }
        // Members read accounts and stores, but not grants.
        for path in [ACCOUNTS, STORES] {
            assert_eq!(s.get(path, &token).await.0, StatusCode::OK);
        }
        let (status, b) = s.get(&format!("{STORES}/openporch/grants"), &token).await;
        assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    }
}

#[tokio::test]
async fn delete_an_account_in_use() {
    let s = server().await;
    s.static_account("aws-prod").await;
    s.ok(
        Method::POST,
        "/orgs/acme/resources/defs",
        json!({"id": "prod-postgres", "type": "postgres", "driver_type": "openporch/echo",
               "driver_account": "aws-prod"}),
    )
    .await;
    let path = format!("{ACCOUNTS}/aws-prod");
    let (status, b) = s.delete(&path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(message(&b).contains("prod-postgres"), "{b}");

    s.ok(
        Method::DELETE,
        "/orgs/acme/resources/defs/prod-postgres",
        Value::Null,
    )
    .await;
    s.aws_store("prod-sm", "aws-prod").await;
    let (status, b) = s.delete(&path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(message(&b).contains("prod-sm"), "{b}");

    s.ok(Method::DELETE, &format!("{STORES}/prod-sm"), Value::Null)
        .await;
    s.ok(Method::DELETE, &path, Value::Null).await;
}

#[tokio::test]
async fn register_an_aws_store() {
    let s = server().await;
    let (_, list) = s.get(STORES, &s.admin).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], "openporch");
    assert_eq!(list[0]["primary"], true);

    s.static_account("aws-prod").await;
    let b = s.aws_store("prod-sm", "aws-prod").await;
    assert_eq!(b["type"], "aws-secrets-manager");
    assert_eq!(b["primary"], false);
    assert_eq!(b["config"]["cloud_account"], "aws-prod");

    // References may now name the store, once granted.
    s.grant_secret("prod-sm", "shop/db-user").await;
    s.ok(
        Method::POST,
        "/orgs/acme/resources/defs",
        json!({"id": "two-stores", "type": "postgres", "driver_type": "openporch/echo",
        "driver_inputs": {"secret_refs": {"variables": {
            "username": {"store": "prod-sm", "ref": "shop/db-user"},
            "password": {"value": "hunter2"},
        }}}}),
    )
    .await;

    for (body, status, code) in [
        (
            json!({"id": "openporch", "type": "openporch"}),
            StatusCode::CONFLICT,
            "API-004",
        ),
        (
            json!({"id": "prod-sm", "type": "aws-ssm", "config": {"region": "r", "cloud_account": "aws-prod"}}),
            StatusCode::CONFLICT,
            "API-004",
        ),
        (
            json!({"id": "vault", "type": "vault", "config": {}}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"id": "ssm", "type": "aws-ssm", "config": {"region": "r"}}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"id": "ssm", "type": "aws-ssm", "config": {"region": "r", "cloud_account": "aws-nope"}}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"id": "ssm", "type": "aws-ssm", "config": {"region": "r", "cloud_account": "aws-prod", "x": 1}}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
    ] {
        let (s2, b) = s.send(Method::POST, STORES, body).await;
        assert_error(s2, &b, status, code);
    }

    // Configuration changes; the built-in store has none.
    let b = s
        .ok(
            Method::PATCH,
            &format!("{STORES}/prod-sm"),
            json!({"config": {"region": "eu-west-1", "cloud_account": "aws-prod"}}),
        )
        .await;
    assert_eq!(b["config"]["region"], "eu-west-1");
    let (status, b) = s
        .send(
            Method::PATCH,
            &format!("{STORES}/openporch"),
            json!({"config": {"region": "r", "cloud_account": "aws-prod"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");

    // External stores cannot become primary until their adapters exist.
    let (status, b) = s
        .send(
            Method::PATCH,
            &format!("{STORES}/prod-sm"),
            json!({"primary": true}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) = s
        .send(
            Method::PATCH,
            &format!("{STORES}/openporch"),
            json!({"primary": false}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (_, b) = s.get(&format!("{STORES}/openporch"), &s.admin).await;
    assert_eq!(b["primary"], true);
    let (status, b) = s.get(&format!("{STORES}/nope"), &s.admin).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn delete_a_store_in_use() {
    let s = server().await;
    s.static_account("aws-prod").await;
    s.aws_store("prod-sm", "aws-prod").await;
    let db_grant = s.grant_secret("prod-sm", "db").await;
    s.ok(
        Method::POST,
        "/orgs/acme/resources/defs",
        json!({"id": "prod-postgres", "type": "postgres", "driver_type": "openporch/echo",
               "driver_inputs": {"secret_refs": {"password": {"store": "prod-sm", "ref": "db"}}}}),
    )
    .await;
    // Plain values shaped like a reference do not keep the store.
    s.ok(
        Method::POST,
        "/orgs/acme/resources/defs",
        json!({"id": "lookalike", "type": "postgres", "driver_type": "openporch/echo",
               "driver_inputs": {"values": {"source": {"store": "prod-sm", "ref": "branch"}}}}),
    )
    .await;
    let path = format!("{STORES}/prod-sm");
    let (status, b) = s.delete(&path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(message(&b).contains("prod-postgres"), "{b}");
    assert!(!message(&b).contains("lookalike"), "{b}");
    s.ok(
        Method::DELETE,
        "/orgs/acme/resources/defs/prod-postgres",
        Value::Null,
    )
    .await;
    s.ok(Method::DELETE, &db_grant, Value::Null).await;

    // Accounts and grants keep it too.
    let key_grant = s.grant_secret("prod-sm", "keys/other").await;
    s.ok(
        Method::POST,
        ACCOUNTS,
        json!({"id": "aws-other", "type": "aws", "config": {"access_key_id": "A"},
               "secret_refs": {"secret_access_key": {"store": "prod-sm", "ref": "keys/other"}}}),
    )
    .await;
    let (status, b) = s.delete(&path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(message(&b).contains("aws-other"), "{b}");
    s.ok(
        Method::DELETE,
        &format!("{ACCOUNTS}/aws-other"),
        Value::Null,
    )
    .await;
    s.ok(Method::DELETE, &key_grant, Value::Null).await;
    let grant = s
        .ok(
            Method::POST,
            &format!("{path}/grants"),
            json!({"ref": "shop/db", "org_wide": true}),
        )
        .await;
    let (status, b) = s.delete(&path, &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    s.ok(
        Method::DELETE,
        &format!("{path}/grants/{}", grant["id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    s.ok(Method::DELETE, &path, Value::Null).await;

    // The built-in, primary store is always kept.
    let (status, b) = s.delete(&format!("{STORES}/openporch"), &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
}

#[tokio::test]
async fn store_depends_on_itself() {
    let s = server().await;
    s.static_account("aws-a").await;
    s.aws_store("sm-a", "aws-a").await;
    // The account's key would now live only in the store it unlocks.
    let (status, b) = s
        .send(
            Method::PUT,
            &format!("{ACCOUNTS}/aws-a"),
            json!({"type": "aws", "config": {"access_key_id": "A"},
                   "secret_refs": {"secret_access_key": {"store": "sm-a", "ref": "keys/a"}}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(message(&b).contains("credential cycle"), "{b}");

    // A longer cycle through a second store, closed by a store update.
    s.grant_secret("sm-a", "keys/b").await;
    s.ok(
        Method::POST,
        ACCOUNTS,
        json!({"id": "aws-b", "type": "aws", "config": {"access_key_id": "B"},
               "secret_refs": {"secret_access_key": {"store": "sm-a", "ref": "keys/b"}}}),
    )
    .await;
    s.aws_store("sm-b", "aws-b").await;
    let (status, b) = s
        .send(
            Method::PATCH,
            &format!("{STORES}/sm-a"),
            json!({"config": {"region": "r", "cloud_account": "aws-b"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(message(&b).contains("credential cycle"), "{b}");

    // Role accounts use the host identity and end the chain.
    s.ok(
        Method::POST,
        ACCOUNTS,
        json!({"id": "aws-role", "type": "aws-role",
               "config": {"role_arn": "arn:aws:iam::1:role/r", "external_id": "e"}}),
    )
    .await;
    s.ok(
        Method::PATCH,
        &format!("{STORES}/sm-a"),
        json!({"config": {"region": "r", "cloud_account": "aws-role"}}),
    )
    .await;
}

#[tokio::test]
async fn secret_use_grants() {
    let s = server().await;
    s.app("shop").await;
    let grants = format!("{STORES}/openporch/grants");
    let b = s
        .ok(
            Method::POST,
            &grants,
            json!({"ref": "shop/db", "app_id": "shop", "env_id": "development"}),
        )
        .await;
    assert_eq!(b["store"], "openporch");
    assert_eq!(b["ref"], "shop/db");
    assert_eq!(b["app_id"], "shop");
    assert_eq!(b["env_id"], "development");
    assert_eq!(b["org_wide"], false);
    assert_eq!(b["created_by"], "bootstrap");
    let id = b["id"].as_str().unwrap().to_owned();
    s.ok(
        Method::POST,
        &grants,
        json!({"ref": "platform/key", "org_wide": true}),
    )
    .await;
    let (status, list) = s.get(&grants, &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 2);

    for (body, status, code) in [
        (
            json!({"ref": "shop/db", "app_id": "shop", "env_id": "development"}),
            StatusCode::CONFLICT,
            "API-004",
        ),
        (
            json!({"ref": "shop/db"}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"ref": "shop/db", "env_id": "development"}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"ref": "shop/db", "app_id": "shop", "org_wide": true}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"ref": "${context.app.id}/db", "app_id": "shop"}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"ref": "shop/db", "app_id": "nope"}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
        (
            json!({"ref": "shop/db", "app_id": "shop", "env_id": "nope"}),
            StatusCode::BAD_REQUEST,
            "API-003",
        ),
    ] {
        let (s2, b) = s.send(Method::POST, &grants, body.clone()).await;
        assert_error(s2, &b, status, code);
    }
    let (status, b) = s
        .send(
            Method::POST,
            &format!("{STORES}/nope/grants"),
            json!({"ref": "k", "org_wide": true}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");

    s.ok(Method::DELETE, &format!("{grants}/{id}"), Value::Null)
        .await;
    let (status, b) = s.delete(&format!("{grants}/{id}"), &s.admin).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let (_, list) = s.get(&grants, &s.admin).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn account_changes_are_audited() {
    let s = server().await;
    s.static_account("aws-dev").await;
    s.ok(Method::DELETE, &format!("{ACCOUNTS}/aws-dev"), Value::Null)
        .await;
    let (status, b) = s
        .get(
            "/orgs/acme/audit-logs?path_prefix=/orgs/acme/resources/accounts",
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let text = b.to_string();
    assert!(text.contains("cloud_account.create"), "{text}");
    assert!(text.contains("cloud_account.delete"), "{text}");
    assert!(!text.contains("wJalr-secret"), "{text}");
}

/// STS for the account check: role sessions need external ID `ext-123`, and role
/// `arn:aws:iam::1:role/offline` cannot be reached.
struct FakeSts;

impl Sts for FakeSts {
    fn assume_role(&self, r: AssumeRole) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async move {
            if r.role_arn.ends_with("/offline") {
                return Err(CloudError::Unavailable(
                    "STS AssumeRole failed: dispatch failure".into(),
                ));
            }
            if r.external_id != "ext-123" {
                return Err(CloudError::Rejected(
                    "STS AssumeRole failed: AccessDenied: not authorized to perform \
                     sts:AssumeRole"
                        .into(),
                ));
            }
            Ok(AwsCredentials {
                access_key_id: "ASIASESSION".into(),
                secret_access_key: SecretValue::new("session-secret"),
                session_token: Some(SecretValue::new("session-token")),
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
            let arn = if c.session_token.is_some() {
                "arn:aws:sts::111122223333:assumed-role/openporch/openporch-aws-prod".to_owned()
            } else {
                format!("arn:aws:iam::111122223333:user/{}", c.access_key_id)
            };
            Ok(CallerIdentity {
                arn,
                account: "111122223333".into(),
                user_id: "AIDAEXAMPLE".into(),
            })
        })
    }
}

#[tokio::test]
async fn account_check_returns_the_caller_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::Config::new("acme", dir.path());
    let state = crate::start_with(config, None, std::sync::Arc::new(FakeSts))
        .await
        .unwrap();
    let s = super::server_from(dir, state);
    s.static_account("aws-dev").await;
    let b = s
        .ok(
            Method::POST,
            &format!("{ACCOUNTS}/aws-dev/check"),
            json!({}),
        )
        .await;
    assert_eq!(b["arn"], "arn:aws:iam::111122223333:user/AKIAEXAMPLE");
    assert_eq!(b["account"], "111122223333");

    let role = |id: &str, arn: &str, external_id: &str| {
        json!({"id": id, "type": "aws-role",
               "config": {"role_arn": arn, "external_id": external_id}})
    };
    s.ok(
        Method::POST,
        ACCOUNTS,
        role("aws-prod", "arn:aws:iam::1:role/openporch", "ext-123"),
    )
    .await;
    let b = s
        .ok(
            Method::POST,
            &format!("{ACCOUNTS}/aws-prod/check"),
            json!({}),
        )
        .await;
    assert!(b["arn"].as_str().unwrap().contains(":assumed-role/"), "{b}");
    let text = b.to_string();
    for secret in [
        "wJalr-secret",
        "session-secret",
        "session-token",
        "ASIASESSION",
    ] {
        assert!(!text.contains(secret), "{text}");
    }

    // Wrong external ID: the STS failure message is returned.
    s.ok(
        Method::POST,
        ACCOUNTS,
        role("aws-wrong", "arn:aws:iam::1:role/openporch", "nope"),
    )
    .await;
    let (status, b) = s
        .send(
            Method::POST,
            &format!("{ACCOUNTS}/aws-wrong/check"),
            json!({}),
        )
        .await;
    assert_error(status, &b, StatusCode::UNPROCESSABLE_ENTITY, "CLD-001");
    assert!(message(&b).contains("AccessDenied"), "{b}");

    s.ok(
        Method::POST,
        ACCOUNTS,
        role("aws-offline", "arn:aws:iam::1:role/offline", "ext-123"),
    )
    .await;
    let (status, b) = s
        .send(
            Method::POST,
            &format!("{ACCOUNTS}/aws-offline/check"),
            json!({}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_GATEWAY, "CLD-002");

    let (status, b) = s
        .send(Method::POST, &format!("{ACCOUNTS}/nope/check"), json!({}))
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let member = s.member("mia", "member").await;
    let (status, b) = s
        .call(
            Method::POST,
            &format!("{ACCOUNTS}/aws-dev/check"),
            Some(&member),
            None,
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
}

#[tokio::test]
async fn delete_a_store_used_by_shared_values() {
    let s = server().await;
    s.static_account("aws-prod").await;
    s.aws_store("vault", "aws-prod").await;
    s.app("shop").await;
    let grant = s
        .ok(
            Method::POST,
            &format!("{STORES}/vault/grants"),
            json!({"ref": "shop/db", "app_id": "shop"}),
        )
        .await;
    let value = "/orgs/acme/apps/shop/values";
    let (status, b) = s
        .post(
            value,
            &s.admin,
            json!({"key": "DB", "secret_ref": {"store": "vault", "ref": "shop/db"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    s.ok(
        Method::DELETE,
        &format!("{STORES}/vault/grants/{}", grant["id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    let (status, b) = s.delete(&format!("{STORES}/vault"), &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(message(&b).contains("shared values shop"), "{b}");

    // Value set versions keep the reference after the value is gone.
    let (status, _) = s.delete(&format!("{value}/DB"), &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, b) = s.delete(&format!("{STORES}/vault"), &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
}
