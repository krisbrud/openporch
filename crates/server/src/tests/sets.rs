//! HTTP tests for `deployment-sets-and-deltas`. Deploying a workload document is
//! verified with the CLI in task 6.3.

use axum::http::StatusCode;
use openporch_store::{AuditIntent, DeploymentRequest, Store};
use serde_json::{Value, json};

use super::{Server, assert_error, server};

fn web(page_size: &str) -> Value {
    json!({"class": "default", "externals": {}, "spec": {"containers": {"web": {
        "image": "shop/web:1", "variables": {"PAGE_SIZE": page_size}}}}})
}

impl Server {
    /// Applies `delta` to set `base` as `token` and returns the new set ID.
    async fn apply(&self, token: &str, base: &str, delta: Value) -> String {
        let (s, b) = self
            .post(&format!("/orgs/acme/apps/shop/sets/{base}"), token, delta)
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b["id"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
async fn add_to_an_empty_environment() {
    let s = server().await;
    s.app("shop").await;
    let (status, empty) = s.get("/orgs/acme/apps/shop/sets/0", &s.admin).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["modules"], json!({}));
    let empty_id = empty["id"].as_str().unwrap();
    let (status, same) = s
        .get(&format!("/orgs/acme/apps/shop/sets/{empty_id}"), &s.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{same}");

    let id = s
        .apply(
            &s.admin,
            "0",
            json!({"modules": {"add": {"web": web("10")}}}),
        )
        .await;
    let (status, set) = s
        .get(&format!("/orgs/acme/apps/shop/sets/{id}"), &s.admin)
        .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["id"], id.as_str());
    assert_eq!(set["modules"], json!({"web": web("10")}));
}

#[tokio::test]
async fn patch_a_variable() {
    let s = server().await;
    s.app("shop").await;
    let ten = s
        .apply(
            &s.admin,
            "0",
            json!({"modules": {"add": {"web": web("10")}}}),
        )
        .await;
    let twenty = s
        .apply(
            &s.admin,
            &ten,
            json!({"modules": {"update": {"web": [{"op": "replace",
                "path": "/spec/containers/web/variables/PAGE_SIZE", "value": "20"}]}}}),
        )
        .await;
    let (_, set) = s
        .get(&format!("/orgs/acme/apps/shop/sets/{twenty}"), &s.admin)
        .await;
    assert_eq!(set["modules"]["web"], web("20"));
    // The original set is unchanged.
    let (_, set) = s
        .get(&format!("/orgs/acme/apps/shop/sets/{ten}"), &s.admin)
        .await;
    assert_eq!(set["modules"]["web"], web("10"));

    // The diff transforms one set into the other.
    let (status, diff) = s
        .get(
            &format!("/orgs/acme/apps/shop/sets/{ten}/diff/{twenty}"),
            &s.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{diff}");
    assert_eq!(s.apply(&s.admin, &ten, diff).await, twenty);
    let (_, diff) = s
        .get(&format!("/orgs/acme/apps/shop/sets/0/diff/{ten}"), &s.admin)
        .await;
    assert_eq!(diff, json!({"modules": {"add": {"web": web("10")}}}));
}

#[tokio::test]
async fn identical_content_identical_id() {
    let s = server().await;
    s.app("shop").await;
    let ten = s
        .apply(
            &s.admin,
            "0",
            json!({"modules": {"add": {"web": web("10")}}}),
        )
        .await;
    // A different route to the same content yields the same ID.
    let other = s
        .apply(
            &s.admin,
            "0",
            json!({"modules": {"add": {"web": web("20")}}}),
        )
        .await;
    assert_ne!(other, ten);
    let back = s
        .apply(
            &s.admin,
            &other,
            json!({"modules": {"update": {"web": [{"op": "replace",
                "path": "/spec/containers/web/variables/PAGE_SIZE", "value": "10"}]}}}),
        )
        .await;
    assert_eq!(back, ten);
}

#[tokio::test]
async fn applying_names_the_workload_that_fails() {
    let s = server().await;
    s.app("shop").await;
    let id = s
        .apply(
            &s.admin,
            "0",
            json!({"modules": {"add": {"web": web("10")}}}),
        )
        .await;
    for (base, delta) in [
        ("0", json!({"modules": {"remove": ["api"]}})),
        (id.as_str(), json!({"modules": {"add": {"web": web("10")}}})),
        ("0", json!({"modules": {"update": {"api": []}}})),
        (
            id.as_str(),
            json!({"modules": {"update": {"web": [{"op": "remove", "path": "/spec/nothing"}]}}}),
        ),
    ] {
        let (status, b) = s
            .post(
                &format!("/orgs/acme/apps/shop/sets/{base}"),
                &s.admin,
                delta,
            )
            .await;
        assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
        let message = b["message"].as_str().unwrap();
        assert!(
            message.contains("`api`") || message.contains("`web`"),
            "{b}"
        );
    }
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/sets/0",
            &s.admin,
            json!({"modules": {"rename": {}}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    let (status, b) = s.get("/orgs/acme/apps/shop/sets/missing", &s.admin).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn sets_belong_to_their_application() {
    let s = server().await;
    s.app("shop").await;
    s.app("blog").await;
    let id = s
        .apply(
            &s.admin,
            "0",
            json!({"modules": {"add": {"web": web("10")}}}),
        )
        .await;
    let (status, b) = s
        .get(&format!("/orgs/acme/apps/blog/sets/{id}"), &s.admin)
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn deltas_are_mutable_until_deployed() {
    let s = server().await;
    s.app("shop").await;
    let (status, created) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": web("10")}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["frozen"], false);
    assert_eq!(created["created_by"], "bootstrap");
    assert_eq!(created["modules"], json!({"add": {"web": web("10")}}));

    let path = format!("/orgs/acme/apps/shop/deltas/{id}");
    let (status, replaced) = s
        .put(
            &path,
            &s.admin,
            json!({"modules": {"add": {"web": web("20")}}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["modules"]["add"]["web"], web("20"));
    let (_, listed) = s.get("/orgs/acme/apps/shop/deltas", &s.admin).await;
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed[0]["id"], id.as_str());

    // A deployment freezes the delta.
    let store = &s.state.store;
    let set_id = s
        .apply(
            &s.admin,
            "0",
            json!({"modules": {"add": {"web": web("20")}}}),
        )
        .await;
    store
        .accept_deployment(DeploymentRequest {
            org: "acme".into(),
            app: "shop".into(),
            env: "development".into(),
            actor: "bootstrap".into(),
            expected_generation: 0,
            idempotency_key: None,
            fingerprint: "body".into(),
            set_id,
            delta_id: Some(id.clone()),
            comment: String::new(),
            snapshot: json!({}),
            audit: AuditIntent::accepted("acme", "bootstrap", "POST", "/deploys", "deploy"),
        })
        .await
        .unwrap();
    let (status, b) = s
        .put(
            &path,
            &s.admin,
            json!({"modules": {"add": {"web": web("30")}}}),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (_, frozen) = s.get(&path, &s.admin).await;
    assert_eq!(frozen["frozen"], true);
    assert_eq!(frozen["modules"]["add"]["web"], web("20"));

    let (status, b) = s
        .put("/orgs/acme/apps/shop/deltas/missing", &s.admin, json!({}))
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/deltas",
            &s.admin,
            json!({"modules": {"add": {"web": {"image": "x"}}}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn viewers_read_and_developers_write() {
    let s = server().await;
    s.app("shop").await;
    let viewer = s.member("viewer", "member").await;
    s.grant_app("shop", "viewer", "viewer").await;
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    let outsider = s.member("outsider", "member").await;

    let delta = json!({"modules": {"add": {"web": web("10")}}});
    let id = s.apply(&dev, "0", delta.clone()).await;
    let (status, created) = s
        .post("/orgs/acme/apps/shop/deltas", &dev, delta.clone())
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let delta_id = created["id"].as_str().unwrap();

    for path in [
        format!("/orgs/acme/apps/shop/sets/{id}"),
        format!("/orgs/acme/apps/shop/sets/0/diff/{id}"),
        "/orgs/acme/apps/shop/deltas".to_owned(),
        format!("/orgs/acme/apps/shop/deltas/{delta_id}"),
    ] {
        let (status, b) = s.get(&path, &viewer).await;
        assert_eq!(status, StatusCode::OK, "{path}: {b}");
        let (status, b) = s.get(&path, &outsider).await;
        assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    }
    let (status, b) = s
        .post("/orgs/acme/apps/shop/sets/0", &viewer, delta.clone())
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, b) = s
        .post("/orgs/acme/apps/shop/deltas", &viewer, delta.clone())
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, b) = s
        .put(
            &format!("/orgs/acme/apps/shop/deltas/{delta_id}"),
            &viewer,
            delta,
        )
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
}
