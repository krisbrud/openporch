//! HTTP tests for `shared-values`. Resolving `${values.KEY}` in a deployment, pinning
//! a deployment's value set version and composite secret expressions are verified
//! with deployments (task 5).

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Server, assert_error, server};

const APP: &str = "/orgs/acme/apps/shop/values";
const PROD: &str = "/orgs/acme/apps/shop/envs/production/values";

impl Server {
    /// `shop` with environments `development` and `production`.
    pub(super) async fn shop(&self) {
        self.app("shop").await;
        self.env_type("production").await;
        let (s, b) = self
            .post(
                "/orgs/acme/apps/shop/envs",
                &self.admin,
                json!({"id": "production", "type": "production"}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }

    pub(super) async fn value(&self, path: &str, body: Value) -> Value {
        let (s, b) = self.post(path, &self.admin, body).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b
    }
}

#[tokio::test]
async fn environment_override() {
    let s = server().await;
    s.shop().await;
    let base = s
        .value(
            APP,
            json!({"key": "API_URL", "description": "API", "value": "https://dev.example"}),
        )
        .await;
    assert_eq!(base["value"], "https://dev.example");
    assert_eq!(base["is_secret"], false);
    s.value(
        PROD,
        json!({"key": "API_URL", "value": "https://api.example"}),
    )
    .await;

    let (_, prod) = s.get(PROD, &s.admin).await;
    assert_eq!(prod.as_array().unwrap().len(), 1, "{prod}");
    assert_eq!(prod[0]["value"], "https://api.example");
    let (_, dev) = s
        .get("/orgs/acme/apps/shop/envs/development/values", &s.admin)
        .await;
    assert_eq!(dev, json!([]));
    let (_, app) = s.get(&format!("{APP}/API_URL"), &s.admin).await;
    assert_eq!(app["value"], "https://dev.example");

    // Replacing keeps the key; deleting an overridden base is refused.
    let (status, b) = s
        .put(
            &format!("{PROD}/API_URL"),
            &s.admin,
            json!({"value": "https://api2.example"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["value"], "https://api2.example");
    let (status, b) = s.delete(&format!("{APP}/API_URL"), &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(
        b["message"].as_str().unwrap().contains("`production`"),
        "{b}"
    );
    let (status, _) = s.delete(&format!("{PROD}/API_URL"), &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = s.delete(&format!("{APP}/API_URL"), &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn override_without_base() {
    let s = server().await;
    s.shop().await;
    let (status, b) = s
        .post(PROD, &s.admin, json!({"key": "API_URL", "value": "x"}))
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");

    for body in [
        json!({"key": "API_URL"}),
        json!({"key": "API_URL", "value": "x", "secret_ref": {"value": "y"}}),
        json!({"key": "9LIVES", "value": "x"}),
        json!({"key": "versions", "value": "x"}),
    ] {
        let (status, b) = s.post(APP, &s.admin, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    }
    s.value(APP, json!({"key": "API_URL", "value": "x"})).await;
    let (status, b) = s
        .post(APP, &s.admin, json!({"key": "API_URL", "value": "x"}))
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .get("/orgs/acme/apps/shop/envs/missing/values", &s.admin)
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn secret_values_are_references() {
    let s = server().await;
    s.shop().await;
    let token = "s3cr3t-token-value";
    let created = s
        .value(
            APP,
            json!({"key": "API_TOKEN", "secret_ref": {"value": token}}),
        )
        .await;
    assert_eq!(created["is_secret"], true);
    assert_eq!(created["secret_ref"]["store"], "openporch");
    assert_eq!(created["secret_ref"]["ref"], "apps/shop/values/API_TOKEN");
    assert!(created["secret_ref"]["version"].is_string(), "{created}");
    assert!(created.get("value").is_none(), "{created}");

    // An environment override may reuse the application's secret.
    s.value(
        PROD,
        json!({"key": "API_TOKEN", "secret_ref": created["secret_ref"].clone()}),
    )
    .await;
    // A rejected change leaves no secret behind: `DB_PASSWORD` has no base.
    let (status, b) = s
        .post(
            PROD,
            &s.admin,
            json!({"key": "DB_PASSWORD", "secret_ref": {"value": "only-in-prod"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    use openporch_store::secrets::SecretStore as _;
    let primary = crate::secret_inputs::primary(&s.state).unwrap();
    let rejected = serde_json::from_value(
        json!({"store": "openporch", "ref": "apps/shop/envs/production/values/DB_PASSWORD"}),
    )
    .unwrap();
    assert!(primary.read(&rejected).await.is_err());
    // The accepted one is in the built-in store.
    let r = serde_json::from_value(created["secret_ref"].clone()).unwrap();
    assert_eq!(primary.read(&r).await.unwrap().expose(), token);

    let mut seen = String::new();
    for path in [
        APP.to_owned(),
        PROD.to_owned(),
        format!("{APP}/versions"),
        format!("{PROD}/versions"),
        "/orgs/acme/audit-logs".to_owned(),
    ] {
        seen.push_str(&s.get(&path, &s.admin).await.1.to_string());
    }
    assert!(
        !seen.contains(token) && !seen.contains("only-in-prod"),
        "{seen}"
    );
}

#[tokio::test]
async fn secret_references_need_a_grant() {
    let s = server().await;
    s.shop().await;
    s.app("billing").await;
    s.value(
        "/orgs/acme/apps/billing/values",
        json!({"key": "STRIPE_KEY", "secret_ref": {"value": "sk_live"}}),
    )
    .await;
    let foreign = json!({"store": "openporch", "ref": "apps/billing/values/STRIPE_KEY"});

    // Reference copied between applications.
    let (status, b) = s
        .post(
            APP,
            &s.admin,
            json!({"key": "STRIPE_KEY", "secret_ref": foreign}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    // Environment secret copied to application values.
    s.value(APP, json!({"key": "DB_PASSWORD", "value": "placeholder"}))
        .await;
    s.value(
        PROD,
        json!({"key": "DB_PASSWORD", "secret_ref": {"value": "prod-only"}}),
    )
    .await;
    let prod_ref =
        json!({"store": "openporch", "ref": "apps/shop/envs/production/values/DB_PASSWORD"});
    let (status, b) = s
        .put(
            &format!("{APP}/DB_PASSWORD"),
            &s.admin,
            json!({"secret_ref": prod_ref}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    // An administrator's grant to `shop` allows the reference.
    let (status, b) = s
        .post(
            "/orgs/acme/secretstores/openporch/grants",
            &s.admin,
            json!({"ref": "apps/billing/values/STRIPE_KEY", "app_id": "shop"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    s.value(APP, json!({"key": "STRIPE_KEY", "secret_ref": foreign}))
        .await;

    // Unregistered stores are rejected.
    let (status, b) = s
        .post(
            APP,
            &s.admin,
            json!({"key": "OTHER", "secret_ref": {"store": "vault", "ref": "x"}}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
}

#[tokio::test]
async fn every_change_creates_a_value_set_version() {
    let s = server().await;
    s.shop().await;
    s.value(APP, json!({"key": "A", "value": "1"})).await;
    s.value(APP, json!({"key": "B", "value": "2"})).await;
    let (status, _) = s
        .put(&format!("{APP}/A"), &s.admin, json!({"value": "3"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = s.delete(&format!("{APP}/B"), &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    s.value(PROD, json!({"key": "A", "value": "prod"})).await;

    let (status, versions) = s.get(&format!("{APP}/versions"), &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{versions}");
    let ids: Vec<&str> = versions
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["4", "3", "2", "1"]);
    assert_eq!(
        versions[0]["values"],
        json!({"A": {"description": "", "value": "3"}})
    );
    assert_eq!(versions[2]["values"]["B"]["value"], "2");
    assert_eq!(versions[0]["created_by"], "bootstrap");
    let (_, prod) = s.get(&format!("{PROD}/versions"), &s.admin).await;
    assert_eq!(prod.as_array().unwrap().len(), 1, "{prod}");
    assert_eq!(prod[0]["values"]["A"]["value"], "prod");
}

#[tokio::test]
async fn delete_environment() {
    let s = server().await;
    s.app("shop").await;
    s.env_type("preview").await;
    let create = json!({"id": "pr-42", "type": "preview"});
    let (status, b) = s
        .post("/orgs/acme/apps/shop/envs", &s.admin, create.clone())
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    s.value(APP, json!({"key": "DB_PASSWORD", "value": "base"}))
        .await;
    let env = "/orgs/acme/apps/shop/envs/pr-42/values";
    s.value(
        env,
        json!({"key": "DB_PASSWORD", "secret_ref": {"value": "preview-secret"}}),
    )
    .await;

    let (status, _) = s.delete("/orgs/acme/apps/shop/envs/pr-42", &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, app) = s.get(APP, &s.admin).await;
    assert_eq!(app[0]["value"], "base");
    // A new environment with the same ID starts without the overrides.
    let (status, _) = s.post("/orgs/acme/apps/shop/envs", &s.admin, create).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(s.get(env, &s.admin).await.1, json!([]));
    assert_eq!(
        s.get(&format!("{env}/versions"), &s.admin).await.1,
        json!([])
    );
}

#[tokio::test]
async fn development_user_changes_production_configuration() {
    let s = server().await;
    s.shop().await;
    s.value(APP, json!({"key": "API_URL", "value": "base"}))
        .await;
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    s.grant_deployer("development", "dev").await;
    let viewer = s.member("viewer", "member").await;
    s.grant_app("shop", "viewer", "viewer").await;

    // Development overrides are allowed.
    let dev_values = "/orgs/acme/apps/shop/envs/development/values";
    let (status, b) = s
        .post(dev_values, &dev, json!({"key": "API_URL", "value": "dev"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");

    // Production overrides and application values used by production are not.
    for (method, path, body) in [
        (
            Method::POST,
            PROD.to_owned(),
            json!({"key": "API_URL", "value": "x"}),
        ),
        (
            Method::POST,
            APP.to_owned(),
            json!({"key": "NEW", "value": "x"}),
        ),
        (Method::PUT, format!("{APP}/API_URL"), json!({"value": "x"})),
        (Method::DELETE, format!("{APP}/API_URL"), Value::Null),
    ] {
        let body = (!body.is_null()).then_some(body);
        let (status, b) = s.call(method, &path, Some(&dev), body).await;
        assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
        assert!(
            b["message"].as_str().unwrap().contains("`production`"),
            "{b}"
        );
    }
    // A secret is not written when the change is refused.
    let (status, _) = s
        .post(
            PROD,
            &dev,
            json!({"key": "API_URL", "secret_ref": {"value": "refused"}}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // With deployer on production, application values may change.
    s.grant_deployer("production", "dev").await;
    let (status, b) = s
        .put(&format!("{APP}/API_URL"), &dev, json!({"value": "new"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");

    // Viewers read but never write.
    let (status, _) = s.get(PROD, &viewer).await;
    assert_eq!(status, StatusCode::OK);
    let (status, b) = s
        .post(dev_values, &viewer, json!({"key": "X", "value": "x"}))
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let outsider = s.member("outsider", "member").await;
    let (status, b) = s.get(APP, &outsider).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn value_writes_recheck_authority_in_their_transaction() {
    use openporch_store::{AuditIntent, PutValue, StoreError, ValueContent, ValueStore};
    let s = server().await;
    s.shop().await;
    s.value(APP, json!({"key": "API_URL", "value": "base"}))
        .await;
    s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    s.grant_deployer("development", "dev").await;
    let store = &s.state.store;
    let audit = || AuditIntent::accepted("acme", "dev", "POST", "/values", "value.create");
    let put = |key: &str| PutValue {
        key: key.into(),
        description: String::new(),
        content: ValueContent::Value("x".into()),
        create: true,
    };
    for (env, key) in [(Some("production"), "API_URL"), (None, "NEW")] {
        let err = store
            .put_value("acme", "shop", env, put(key), audit())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Forbidden(_)), "{err}");
    }
    let err = store
        .delete_value("acme", "shop", None, "API_URL", audit())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Forbidden(_)), "{err}");
    store
        .put_value("acme", "shop", Some("development"), put("API_URL"), audit())
        .await
        .unwrap();
}
