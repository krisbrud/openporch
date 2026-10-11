//! HTTP tests for `cloud-accounts` (create, delete-in-use) and the store, grant and
//! credential-graph scenarios of `secret-stores-and-references`.

use axum::http::{Method, StatusCode};
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

    async fn static_account(&self, id: &str) -> Value {
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

    // References may now name the store.
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

    // Accounts and grants keep it too.
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
