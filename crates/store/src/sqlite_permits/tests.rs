//! Plans, permits, their binding to a deployment and their use.

use openporch_core::sets::DeploymentSet;
use serde_json::json;

use super::*;
use crate::{Acceptance, DeploymentKind, DeploymentRequest, Ownership, Store};

const ORG: &str = "acme";
const DB: &str = "postgres.default#modules.web.externals.db";

struct Fixture {
    _dir: tempfile::TempDir,
    store: SqliteStore,
}

fn audit(actor: &str) -> AuditIntent {
    AuditIntent::accepted(
        ORG,
        actor,
        "POST",
        "/orgs/acme/apps/shop/envs/development",
        "test",
    )
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ownership = Ownership::acquire(dir.path()).unwrap();
        let store = SqliteStore::open(ownership, ORG).await.unwrap();
        for (id, role) in [
            ("alice", "manager"),
            ("ci", "member"),
            ("root", "administrator"),
        ] {
            sqlx::query(
                "INSERT INTO principals (org_id, id, type, name, org_role, created_at) \
                 VALUES (?, ?, 'user', ?, ?, 0)",
            )
            .bind(ORG)
            .bind(id)
            .bind(id)
            .bind(role)
            .execute(store.pool())
            .await
            .unwrap();
        }
        store
            .create_app(
                ORG,
                "shop",
                "Shop",
                "alice",
                AuditIntent {
                    actor: None,
                    ..audit("alice")
                },
            )
            .await
            .unwrap();
        for query in [
            "INSERT INTO app_roles (org_id, app_id, principal_id, role) \
             VALUES ('acme', 'shop', 'ci', 'developer')",
            "INSERT INTO env_type_roles (org_id, env_type_id, principal_id, role) \
             VALUES ('acme', 'development', 'ci', 'deployer')",
        ] {
            sqlx::query(query).execute(store.pool()).await.unwrap();
        }
        Self { _dir: dir, store }
    }

    async fn generation(&self) -> i64 {
        self.store
            .env(ORG, "shop", "development")
            .await
            .unwrap()
            .unwrap()
            .generation
    }

    async fn plan(&self, actor: &str) -> PlanRecord {
        self.store
            .put_plan(NewPlan {
                org: ORG.into(),
                app: "shop".into(),
                env: "development".into(),
                kind: "deploy".into(),
                base_generation: self.generation().await,
                set_id: None,
                body: json!({"entries": []}),
                audit: audit(actor),
            })
            .await
            .unwrap()
    }

    /// Issues an `action` permit for the database.
    async fn permit(&self, action: &str, fingerprint: Option<&str>) -> PermitRecord {
        let plan = self.plan("ci").await;
        let issuer = if action == "detach" { "root" } else { "ci" };
        self.store
            .issue_permits(
                ORG,
                "shop",
                "development",
                &plan.id,
                vec![NewPermit {
                    descriptor: DB.into(),
                    guresid: Some("g1".into()),
                    action: action.into(),
                    plan_fingerprint: fingerprint.map(Into::into),
                    config_digest: "digest-1".into(),
                }],
                audit(issuer),
            )
            .await
            .unwrap()
            .remove(0)
    }

    /// Accepts a deployment that deletes the database, presenting `permits`,
    /// with the database's configuration at `digest`.
    async fn deploy(&self, permits: &[&str], digest: &str) -> Result<String> {
        let set: DeploymentSet = serde_json::from_value(json!({"modules": {}})).unwrap();
        let set_id = self.store.put_set(ORG, "shop", &set).await.unwrap();
        let acceptance = self
            .store
            .accept_deployment(DeploymentRequest {
                org: ORG.into(),
                app: "shop".into(),
                env: "development".into(),
                actor: "ci".into(),
                expected_generation: self.generation().await,
                idempotency_key: None,
                fingerprint: "f".into(),
                kind: DeploymentKind::Deploy,
                set_id: Some(set_id),
                delta_id: None,
                delta_content: None,
                comment: String::new(),
                snapshot: json!({}),
                audit: audit("ci"),
                permits: PermitBinding {
                    presented: permits.iter().map(|p| (*p).to_owned()).collect(),
                    digests: BTreeMap::from([(DB.to_owned(), digest.to_owned())]),
                    required: vec![(DB.to_owned(), "delete".to_owned())],
                },
            })
            .await?;
        let Acceptance::Created(d) = acceptance else {
            panic!("replayed");
        };
        Ok(d.id)
    }

    async fn finish(&self, deployment: &str, error: Option<&str>) {
        self.store
            .finish_deployment(
                ORG,
                "shop",
                "development",
                deployment,
                error.map(Into::into),
            )
            .await
            .unwrap();
    }

    async fn use_in(&self, deployment: &str, fingerprint: Option<&str>) -> Result<()> {
        self.store
            .use_permit(PermitUse {
                org: ORG,
                app: "shop",
                env: "development",
                deployment: Some(deployment),
                presented: &[],
                descriptor: DB,
                guresid: Some("g1"),
                action: "delete",
                fingerprint,
            })
            .await
    }
}

#[tokio::test]
async fn a_bound_permit_authorizes_its_deployment_and_retries() {
    let f = Fixture::new().await;
    let permit = f.permit("delete", Some("fp-1")).await;
    assert!(permit.id.starts_with("prm-"));
    let d = f.deploy(&[&permit.id], "digest-1").await.unwrap();
    f.use_in(&d, Some("fp-1")).await.unwrap();
    // A retry of the same action on the same resource is accepted again.
    f.use_in(&d, Some("fp-1")).await.unwrap();
    // Another driver plan is not covered.
    let err = f.use_in(&d, Some("fp-2")).await.unwrap_err();
    assert!(matches!(err, StoreError::ApprovalRequired(_)), "{err:?}");
}

#[tokio::test]
async fn a_known_deletion_without_a_permit_is_refused_at_acceptance() {
    let f = Fixture::new().await;
    let before = f.generation().await;
    let err = f.deploy(&[], "digest-1").await.unwrap_err();
    assert!(matches!(err, StoreError::ApprovalRequired(_)), "{err:?}");
    // Nothing was accepted.
    assert_eq!(f.generation().await, before);
}

#[tokio::test]
async fn stale_permits_do_not_bind() {
    let f = Fixture::new().await;

    // Unknown, or issued for another environment.
    let err = f.deploy(&["prm-nope"], "digest-1").await.unwrap_err();
    assert!(matches!(err, StoreError::PermitInvalid(_)), "{err:?}");

    // The resource's configuration changed since the preview.
    let permit = f.permit("delete", None).await;
    let err = f.deploy(&[&permit.id], "digest-2").await.unwrap_err();
    assert!(
        err.to_string().contains("configuration has changed"),
        "{err}"
    );

    // Expired.
    sqlx::query("UPDATE permits SET expires_at = 0 WHERE id = ?")
        .bind(&permit.id)
        .execute(f.store.pool())
        .await
        .unwrap();
    let err = f.deploy(&[&permit.id], "digest-1").await.unwrap_err();
    assert!(err.to_string().contains("expired"), "{err}");

    // Issued before the environment moved on.
    let permit = f.permit("delete", None).await;
    let other = f.permit("delete", None).await;
    let d = f.deploy(&[&other.id], "digest-1").await.unwrap();
    f.finish(&d, Some("failed")).await;
    let err = f.deploy(&[&permit.id], "digest-1").await.unwrap_err();
    assert!(err.to_string().contains("generation"), "{err}");

    // Bound to another deployment already, even one that failed.
    let err = f.deploy(&[&other.id], "digest-1").await.unwrap_err();
    assert!(err.to_string().contains("already bound"), "{err}");
}

#[tokio::test]
async fn a_permit_is_only_as_good_as_its_issuer() {
    let f = Fixture::new().await;
    let permit = f.permit("delete", None).await;
    let d = f.deploy(&[&permit.id], "digest-1").await.unwrap();
    sqlx::query("DELETE FROM env_type_roles WHERE principal_id = 'ci'")
        .execute(f.store.pool())
        .await
        .unwrap();
    let err = f.use_in(&d, None).await.unwrap_err();
    assert!(err.to_string().contains("no longer holds"), "{err}");
}

#[tokio::test]
async fn issuing_needs_an_unexpired_plan_and_the_authority() {
    let f = Fixture::new().await;
    let plan = f.plan("ci").await;
    let permit = |action: &str| NewPermit {
        descriptor: DB.into(),
        guresid: None,
        action: action.into(),
        plan_fingerprint: None,
        config_digest: "digest-1".into(),
    };
    let issue = |action: &'static str, actor: &'static str| {
        f.store.issue_permits(
            ORG,
            "shop",
            "development",
            &plan.id,
            vec![permit(action)],
            audit(actor),
        )
    };
    // Detaching needs an administrator.
    let err = issue("detach", "ci").await.unwrap_err();
    assert!(matches!(err, StoreError::Forbidden(_)), "{err:?}");
    // Alice manages the organization but cannot deploy here.
    let err = issue("delete", "alice").await.unwrap_err();
    assert!(matches!(err, StoreError::Forbidden(_)), "{err:?}");

    sqlx::query("UPDATE plans SET expires_at = 0 WHERE id = ?")
        .bind(&plan.id)
        .execute(f.store.pool())
        .await
        .unwrap();
    let err = issue("delete", "ci").await.unwrap_err();
    assert!(matches!(err, StoreError::PermitInvalid(_)), "{err:?}");

    let stored = f.store.plan(ORG, "shop", "development", &plan.id).await;
    assert_eq!(stored.unwrap().unwrap().actor, "ci");
}

#[tokio::test]
async fn a_detach_uses_its_presented_permit_once() {
    let f = Fixture::new().await;
    let permit = f.permit("detach", None).await;
    let detach = |presented| PermitUse {
        org: ORG,
        app: "shop",
        env: "development",
        deployment: None,
        presented,
        descriptor: DB,
        guresid: Some("g1"),
        action: "detach",
        fingerprint: None,
    };
    let presented = [permit.id.clone()];
    f.store.use_permit(detach(&presented)).await.unwrap();
    let err = f.store.use_permit(detach(&presented)).await.unwrap_err();
    assert!(err.to_string().contains("already used"), "{err}");
    let err = f.store.use_permit(detach(&[])).await.unwrap_err();
    assert!(matches!(err, StoreError::ApprovalRequired(_)), "{err:?}");
}

#[tokio::test]
async fn approval_requests_keep_the_latest_plan_per_resource() {
    let f = Fixture::new().await;
    let request = |fingerprint: &str| ApprovalRequest {
        descriptor: DB.into(),
        guresid: "g1".into(),
        action: "replace".into(),
        fingerprint: fingerprint.into(),
        actions: vec![("replace".into(), "aws_db_instance.main".into())],
        deployment_id: "d1".into(),
    };
    for fp in ["fp-1", "fp-2"] {
        f.store
            .record_approval_request(ORG, "shop", "development", &request(fp))
            .await
            .unwrap();
    }
    let stored = f.store.approval_requests(ORG, "shop", "development").await;
    assert_eq!(stored.unwrap(), vec![request("fp-2")]);
}
