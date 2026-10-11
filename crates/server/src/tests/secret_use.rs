//! Secret-use grants in `secret-stores-and-references`: references are checked when
//! an object is assigned them and again when the executor resolves them for a
//! deployment ([`SecretUseStore::authorize_secret_use`]).

use axum::http::StatusCode;
use openporch_core::model::SecretRef;
use openporch_store::{SecretOwner, SecretUseStore, StoreError};
use serde_json::{Value, json};

use super::{Server, assert_error, server};

const SHOP: &str = "/orgs/acme/apps/shop/values";
const SHOP_PROD: &str = "/orgs/acme/apps/shop/envs/production/values";
const BILLING: &str = "/orgs/acme/apps/billing/values";
const DEFS: &str = "/orgs/acme/resources/defs";

fn reference(body: &Value) -> SecretRef {
    serde_json::from_value(body["secret_ref"].clone()).unwrap()
}

impl Server {
    async fn resolve(
        &self,
        app: &str,
        env: &str,
        owner: SecretOwner<'_>,
        r: &SecretRef,
    ) -> Result<(), StoreError> {
        self.state
            .store
            .authorize_secret_use("acme", app, env, owner, r)
            .await
    }

    /// `billing`'s secret `STRIPE_KEY`.
    async fn billing_secret(&self) -> SecretRef {
        self.app("billing").await;
        let b = self
            .value(
                BILLING,
                json!({"key": "STRIPE_KEY", "secret_ref": {"value": "sk_live_51H"}}),
            )
            .await;
        reference(&b)
    }

    /// A secret that only `shop/production` wrote.
    async fn production_secret(&self) -> SecretRef {
        self.value(SHOP, json!({"key": "DB_PASS", "value": "unused"}))
            .await;
        let b = self
            .value(
                SHOP_PROD,
                json!({"key": "DB_PASS", "secret_ref": {"value": "prod-pass"}}),
            )
            .await;
        reference(&b)
    }

    async fn grant(&self, r: &SecretRef, body: Value) -> String {
        let mut body = body;
        body["ref"] = json!(r.reference);
        let (s, b) = self
            .post(
                &format!("/orgs/acme/secretstores/{}/grants", r.store),
                &self.admin,
                body,
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        format!(
            "/orgs/acme/secretstores/{}/grants/{}",
            r.store,
            b["id"].as_str().unwrap()
        )
    }
}

fn forbidden(result: Result<(), StoreError>) {
    assert!(
        matches!(result, Err(StoreError::Forbidden(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn reference_copied_between_applications() {
    let s = server().await;
    s.shop().await;
    let r = s.billing_secret().await;
    let copied = json!({"store": r.store, "ref": r.reference});

    // Assignment is denied, to shared values and to a definition.
    let (status, b) = s
        .post(
            SHOP,
            &s.admin,
            json!({"key": "STRIPE_KEY", "secret_ref": copied}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, b) = s
        .post(
            DEFS,
            &s.admin,
            json!({"id": "stripe", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secret_refs": {"key": copied}}}),
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    assert!(!b.to_string().contains("sk_live"), "{b}");

    // The executor refuses it too, whoever holds it, although the store can read it.
    forbidden(
        s.resolve("shop", "development", SecretOwner::Values, &r)
            .await,
    );
    forbidden(
        s.resolve("shop", "production", SecretOwner::Definition("stripe"), &r)
            .await,
    );
    s.resolve("billing", "development", SecretOwner::Values, &r)
        .await
        .unwrap();
}

#[tokio::test]
async fn production_reference_in_development() {
    let s = server().await;
    s.shop().await;
    let r = s.production_secret().await;
    s.resolve("shop", "production", SecretOwner::Values, &r)
        .await
        .unwrap();
    forbidden(
        s.resolve("shop", "development", SecretOwner::Values, &r)
            .await,
    );

    // A definition granted the secret for production only.
    s.grant(&r, json!({"app_id": "shop", "env_id": "production"}))
        .await;
    let (status, b) = s
        .post(
            DEFS,
            &s.admin,
            json!({"id": "prod-db", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secret_refs": {"password": {"store": r.store, "ref": r.reference}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    s.resolve("shop", "production", SecretOwner::Definition("prod-db"), &r)
        .await
        .unwrap();
    forbidden(
        s.resolve(
            "shop",
            "development",
            SecretOwner::Definition("prod-db"),
            &r,
        )
        .await,
    );
}

#[tokio::test]
async fn environment_secret_copied_to_app_values() {
    let s = server().await;
    s.shop().await;
    let r = s.production_secret().await;
    let promote = json!({"key": "PROMOTED", "secret_ref": {"store": r.store, "ref": r.reference}});
    let (status, b) = s.post(SHOP, &s.admin, promote.clone()).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    // An environment grant is not an application grant either.
    s.grant(&r, json!({"app_id": "shop", "env_id": "production"}))
        .await;
    let (status, b) = s.post(SHOP, &s.admin, promote.clone()).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    // An explicit application-wide grant allows it.
    s.grant(&r, json!({"app_id": "shop"})).await;
    let (status, b) = s.post(SHOP, &s.admin, promote).await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn grant_removal_blocks_later_resolution() {
    let s = server().await;
    s.shop().await;
    let r = s.billing_secret().await;
    let grant = s.grant(&r, json!({"app_id": "shop"})).await;
    s.value(
        SHOP,
        json!({"key": "STRIPE_KEY", "secret_ref": {"store": r.store, "ref": r.reference}}),
    )
    .await;
    s.resolve("shop", "development", SecretOwner::Values, &r)
        .await
        .unwrap();

    let (status, _) = s.delete(&grant, &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The value still holds the reference, but replaying it resolves nothing.
    forbidden(
        s.resolve("shop", "development", SecretOwner::Values, &r)
            .await,
    );
}

#[tokio::test]
async fn platform_objects_use_their_own_secrets_or_granted_ones() {
    let s = server().await;
    s.shop().await;
    let r = s.billing_secret().await;
    let foreign = json!({"store": r.store, "ref": r.reference});

    // Without a grant, no platform object may be assigned another object's secret.
    for (path, body) in [
        (
            DEFS,
            json!({"id": "pg", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secret_refs": {"nested": {"password": foreign}}}}),
        ),
        (
            "/orgs/acme/resources/accounts",
            json!({"id": "aws-x", "type": "aws", "config": {"access_key_id": "A"},
                   "secret_refs": {"secret_access_key": foreign}}),
        ),
        (
            "/orgs/acme/resources/drivers",
            json!({"id": "acme/xdriver", "target": "https://driver.internal/x", "token": foreign}),
        ),
    ] {
        let (status, b) = s.post(path, &s.admin, body).await;
        assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    }

    // A definition's own `{value}` secret serves whatever its criteria match, and
    // no other definition.
    let (status, def) = s
        .post(
            DEFS,
            &s.admin,
            json!({"id": "pg", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secret_refs": {"password": {"value": "hunter2"}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{def}");
    let own: SecretRef =
        serde_json::from_value(def["driver_inputs"]["secret_refs"]["password"].clone()).unwrap();
    s.resolve("shop", "development", SecretOwner::Definition("pg"), &own)
        .await
        .unwrap();
    forbidden(
        s.resolve("shop", "development", SecretOwner::Definition("pg2"), &own)
            .await,
    );
    forbidden(
        s.resolve("shop", "development", SecretOwner::Values, &own)
            .await,
    );

    // An organization-wide grant serves every application.
    s.grant(&r, json!({"org_wide": true})).await;
    let (status, b) = s
        .post(
            DEFS,
            &s.admin,
            json!({"id": "shared", "type": "postgres", "driver_type": "openporch/echo",
                   "driver_inputs": {"secret_refs": {"key": foreign}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    s.resolve("shop", "production", SecretOwner::Definition("shared"), &r)
        .await
        .unwrap();
}
