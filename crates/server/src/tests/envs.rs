//! HTTP tests for `applications-and-environments`, with a forbidden-role case per
//! endpoint.

use axum::http::{Method, StatusCode};
use openporch_core::sets::{Delta, DeploymentSet, apply_delta};
use openporch_store::{AuditIntent, DeploymentRecord, DeploymentRequest, NewEnv, Store};
use serde_json::json;

use super::{Server, assert_error, server};

impl Server {
    /// Deploys a one-workload set to `app/env` through the store and finishes it.
    async fn deployed(&self, app: &str, env: &str, image: &str, ok: bool) -> DeploymentRecord {
        let store = &self.state.store;
        let set: DeploymentSet = serde_json::from_value(json!({
            "modules": {"web": {"class": "default", "spec": {"containers": {"web": {"image": image}}}}}
        }))
        .unwrap();
        let set_id = store.put_set("acme", app, &set).await.unwrap();
        let generation = store
            .env("acme", app, env)
            .await
            .unwrap()
            .unwrap()
            .generation;
        let path = format!("/orgs/acme/apps/{app}/envs/{env}/deploys");
        let d = store
            .accept_deployment(DeploymentRequest {
                org: "acme".into(),
                app: app.into(),
                env: env.into(),
                actor: "bootstrap".into(),
                expected_generation: generation,
                idempotency_key: None,
                fingerprint: format!("body-{set_id}"),
                kind: openporch_store::DeploymentKind::Deploy,
                set_id: Some(set_id),
                delta_id: None,
                delta_content: None,
                comment: String::new(),
                snapshot: json!({}),
                audit: AuditIntent::accepted("acme", "bootstrap", "POST", &path, "deploy"),
            })
            .await
            .unwrap()
            .deployment()
            .clone();
        let error = (!ok).then(|| "postgres failed".to_owned());
        store
            .finish_deployment("acme", app, env, &d.id, error)
            .await
            .unwrap();
        d
    }
}

#[tokio::test]
async fn application_starts_with_an_undeployed_development_environment() {
    let s = server().await;
    let carol = s.member("carol", "manager").await;
    let (status, b) = s
        .post("/orgs/acme/apps", &carol, json!({"id": "shop"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let (status, envs) = s.get("/orgs/acme/apps/shop/envs", &carol).await;
    assert_eq!(status, StatusCode::OK, "{envs}");
    assert_eq!(envs.as_array().unwrap().len(), 1);
    let dev = &envs[0];
    assert_eq!(dev["id"], "development");
    assert_eq!(dev["type"], "development");
    assert_eq!(dev["generation"], 0);
    assert_eq!(dev["convergence"], "never_deployed");
    assert!(dev["latest_deployment"].is_null());
    let (_, users) = s.get("/orgs/acme/apps/shop/users", &carol).await;
    assert_eq!(users, json!([{"user": "carol", "role": "owner"}]));

    // Members without a role on the application see none of it.
    let bob = s.member("bob", "member").await;
    for path in [
        "/orgs/acme/apps/shop/envs",
        "/orgs/acme/apps/shop/envs/development",
    ] {
        let (status, b) = s.get(path, &bob).await;
        assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    }
    let (status, b) = s.get("/orgs/acme/apps/shop/envs/nope", &carol).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}

#[tokio::test]
async fn environments_need_developer_and_deployer() {
    let s = server().await;
    s.app("shop").await;
    s.env_type("staging").await;
    let viewer = s.member("viewer", "member").await;
    s.grant_app("shop", "viewer", "viewer").await;
    let dev = s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    let create = json!({"id": "staging", "type": "staging"});

    let (status, b) = s
        .post("/orgs/acme/apps/shop/envs", &viewer, create.clone())
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, b) = s
        .post("/orgs/acme/apps/shop/envs", &dev, create.clone())
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    assert!(b["message"].as_str().unwrap().contains("`deployer`"), "{b}");
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &dev,
            json!({"id": "xy", "type": "nope"}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");

    s.grant_deployer("staging", "dev").await;
    let (status, env) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &dev,
            json!({"id": "staging", "name": "Staging", "type": "staging"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{env}");
    assert_eq!(env["name"], "Staging");
    assert_eq!(env["type"], "staging");
    assert_eq!(env["convergence"], "never_deployed");
    assert!(env.get("delta_id").is_none());
    let (status, b) = s.post("/orgs/acme/apps/shop/envs", &dev, create).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    let (status, b) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &dev,
            json!({"id": "Bad Id", "type": "staging"}),
        )
        .await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-002");

    // Deletion: viewers and developers without deployer are forbidden.
    let (status, b) = s.delete("/orgs/acme/apps/shop/envs/staging", &viewer).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    let (status, b) = s
        .delete("/orgs/acme/apps/shop/envs/development", &dev)
        .await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");
    // `development` stays while the application exists, even for administrators.
    let (status, b) = s
        .remove_env("/orgs/acme/apps/shop/envs/development", &s.admin)
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    // The request needs the generation the client read.
    let (status, b) = s.delete("/orgs/acme/apps/shop/envs/staging", &dev).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    let (status, b) = s
        .call(
            Method::DELETE,
            "/orgs/acme/apps/shop/envs/staging",
            Some(&dev),
            Some(json!({"expected_generation": 7})),
        )
        .await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    // Without resources, it is deleted at once.
    let (status, b) = s
        .remove_env("/orgs/acme/apps/shop/envs/staging", &dev)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["deployment"]["kind"], "delete_env");
    assert_eq!(b["deployment"]["status"], "succeeded", "{b}");
    assert_eq!(b["retained"], json!([]));
    let (status, b) = s.get("/orgs/acme/apps/shop/envs/staging", &dev).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");

    let entries = s.get("/orgs/acme/audit-logs", &s.admin).await.1;
    let actions: Vec<&str> = entries["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["user_id"] == "dev")
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"env.create"), "{actions:?}");
    assert!(actions.contains(&"env.delete"), "{actions:?}");
}

#[tokio::test]
async fn environment_authority_is_rechecked_with_the_change() {
    let s = server().await;
    s.app("shop").await;
    s.env_type("staging").await;
    s.member("dev", "member").await;
    s.grant_app("shop", "dev", "developer").await;
    // The route check passed, but the deployer role is gone when the change runs.
    let audit = AuditIntent::accepted("acme", "dev", "POST", "/orgs/acme/apps/shop/envs", "x");
    let e = s
        .state
        .store
        .create_env(
            "acme",
            "shop",
            NewEnv {
                id: "staging".into(),
                name: "staging".into(),
                env_type: "staging".into(),
                draft: None,
            },
            audit,
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("`deployer`"), "{e}");
}

#[tokio::test]
async fn clone_an_environment_from_a_deployment() {
    let s = server().await;
    s.app("shop").await;
    s.app("billing").await;
    let source = s.deployed("shop", "development", "web:1", true).await;

    // The source environment's summary after a successful deployment.
    let (_, dev) = s
        .get("/orgs/acme/apps/shop/envs/development", &s.admin)
        .await;
    assert_eq!(dev["latest_deployment"]["id"], source.id.as_str());
    assert_eq!(dev["latest_deployment"]["status"], "succeeded");
    assert_eq!(dev["last_successful_deploy_id"], source.id.as_str());
    assert_eq!(dev["desired_set_id"], dev["last_successful_set_id"]);
    assert_eq!(dev["convergence"], "converged");
    assert_eq!(dev["generation"], 1);

    let (status, env) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &s.admin,
            json!({"id": "pr-42", "type": "development", "from_deploy_id": source.id}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{env}");
    assert_eq!(env["convergence"], "never_deployed");
    assert_eq!(env["generation"], 0);
    let delta_id = env["delta_id"].as_str().unwrap();
    let delta = s
        .state
        .store
        .delta("acme", "shop", delta_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delta.env.as_deref(), Some("pr-42"));
    assert!(!delta.frozen);
    let delta: Delta = serde_json::from_value(delta.content).unwrap();
    let rebuilt = apply_delta(&DeploymentSet::default(), &delta).unwrap();
    let source_set = s
        .state
        .store
        .set("acme", "shop", source.set_id.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rebuilt, source_set);
    assert!(
        s.state
            .store
            .latest_deployment("acme", "shop", "pr-42")
            .await
            .unwrap()
            .is_none()
    );

    // A deployment of another application is not found, and nothing is created.
    let (status, b) = s
        .post(
            "/orgs/acme/apps/billing/envs",
            &s.admin,
            json!({"id": "copy", "type": "development", "from_deploy_id": source.id}),
        )
        .await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
    assert!(!b.to_string().contains("web:1"));
    let (status, _) = s.get("/orgs/acme/apps/billing/envs/copy", &s.admin).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Deleting a deployed environment needs the generation the client read.
    s.deployed("shop", "pr-42", "web:2", false).await;
    let (_, pr) = s.get("/orgs/acme/apps/shop/envs/pr-42", &s.admin).await;
    assert_eq!(pr["latest_deployment"]["status"], "failed");
    assert_eq!(pr["convergence"], "partial");
    assert!(pr["last_successful_deploy_id"].is_null());
    let (status, b) = s.delete("/orgs/acme/apps/shop/envs/pr-42", &s.admin).await;
    assert_error(status, &b, StatusCode::BAD_REQUEST, "API-003");
    assert!(
        b["message"]
            .as_str()
            .unwrap()
            .contains("expected_generation")
    );
}

#[tokio::test]
async fn environment_types_in_use_are_kept() {
    let s = server().await;
    s.app("shop").await;
    s.env_type("staging").await;
    s.member("dev", "member").await;
    s.grant_deployer("staging", "dev").await;
    let manager = s.member("mgr", "manager").await;
    let (status, b) = s.delete("/orgs/acme/env-types/staging", &manager).await;
    assert_error(status, &b, StatusCode::FORBIDDEN, "AUTH-002");

    let (status, _) = s
        .post(
            "/orgs/acme/apps/shop/envs",
            &s.admin,
            json!({"id": "staging", "type": "staging"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, b) = s.delete("/orgs/acme/env-types/staging", &s.admin).await;
    assert_error(status, &b, StatusCode::CONFLICT, "API-004");
    assert!(
        b["message"].as_str().unwrap().contains("shop/staging"),
        "{b}"
    );
    assert_eq!(
        s.get("/orgs/acme/env-types/staging", &s.admin).await.0,
        StatusCode::OK
    );

    s.remove_env("/orgs/acme/apps/shop/envs/staging", &s.admin)
        .await;
    let (status, _) = s.delete("/orgs/acme/env-types/staging", &s.admin).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        s.get("/orgs/acme/env-types/staging", &s.admin).await.0,
        StatusCode::NOT_FOUND
    );
    // The deployer role went with the type and does not come back with a new one.
    s.env_type("staging").await;
    let (_, users) = s.get("/orgs/acme/env-types/staging/users", &s.admin).await;
    assert_eq!(users, json!([]));
    let (status, b) = s.delete("/orgs/acme/env-types/nope", &s.admin).await;
    assert_error(status, &b, StatusCode::NOT_FOUND, "API-001");
}
