use std::collections::BTreeMap;

use openporch_core::ids::Descriptor;
use openporch_core::model::SecretRef;
use openporch_core::sets::DeploymentSet;
use serde_json::json;

use super::*;

const ORG: &str = "acme";

struct Fixture {
    dir: tempfile::TempDir,
    store: SqliteStore,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ownership = Ownership::acquire(dir.path()).unwrap();
        let store = SqliteStore::open(ownership, ORG).await.unwrap();
        for (id, role) in [("alice", "manager"), ("ci", "member")] {
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
                system("POST", "/orgs/acme/apps"),
            )
            .await
            .unwrap();
        // `ci` deploys to `shop/development`.
        for query in [
            "INSERT INTO app_roles (org_id, app_id, principal_id, role) \
             VALUES ('acme', 'shop', 'ci', 'developer')",
            "INSERT INTO env_type_roles (org_id, env_type_id, principal_id, role) \
             VALUES ('acme', 'development', 'ci', 'deployer')",
        ] {
            sqlx::query(query).execute(store.pool()).await.unwrap();
        }
        Self { dir, store }
    }

    /// Simulates a crash and restart: the pool and the lock are dropped without any
    /// cleanup, then the installation is opened again.
    async fn restart(self) -> Self {
        let Self { dir, store } = self.close().await;
        drop(store);
        let ownership = Ownership::acquire(dir.path()).unwrap();
        let store = SqliteStore::open(ownership, ORG).await.unwrap();
        Self { dir, store }
    }

    /// Closes the pool; dropping the returned store releases the ownership.
    async fn close(self) -> Self {
        self.store.pool().close().await;
        self
    }

    async fn generation(&self) -> i64 {
        self.store
            .env(ORG, "shop", "development")
            .await
            .unwrap()
            .unwrap()
            .generation
    }

    async fn set(&self, image: &str) -> String {
        let set: DeploymentSet = serde_json::from_value(json!({
            "modules": {"web": {"class": "default", "spec": {"containers": {"web": {"image": image}}}}}
        }))
        .unwrap();
        self.store.put_set(ORG, "shop", &set).await.unwrap()
    }

    fn request(&self, generation: i64, set_id: &str, key: Option<&str>) -> DeploymentRequest {
        DeploymentRequest {
            permits: Default::default(),
            org: ORG.into(),
            app: "shop".into(),
            env: "development".into(),
            actor: "ci".into(),
            expected_generation: generation,
            idempotency_key: key.map(Into::into),
            fingerprint: format!("body-{set_id}"),
            kind: DeploymentKind::Deploy,
            set_id: Some(set_id.into()),
            delta_id: None,
            delta_content: None,
            comment: String::new(),
            snapshot: json!({"definitions": {}}),
            audit: audit("POST", "/orgs/acme/apps/shop/envs/development/deploys"),
        }
    }

    async fn deploy(&self, image: &str) -> DeploymentRecord {
        let set = self.set(image).await;
        let generation = self.generation().await;
        self.store
            .accept_deployment(self.request(generation, &set, None))
            .await
            .unwrap()
            .deployment()
            .clone()
    }

    async fn start(&self, deployment: &str, descriptor: &str) -> Result<OperationStarted> {
        self.store
            .begin_operation(OperationStart {
                org: ORG.into(),
                app: "shop".into(),
                env: "development".into(),
                deployment_id: deployment.into(),
                descriptor: Descriptor::parse(descriptor).unwrap(),
                definition_id: Some("rds".into()),
                definition_version: Some(1),
                driver_type: Some("openporch/echo".into()),
                kind: OperationKind::Put,
                input_fingerprint: "inputs-1".into(),
                recovery: json!({"backend": "local"}),
            })
            .await
    }

    async fn resource(&self, descriptor: &str) -> Option<ActiveResource> {
        self.store
            .active_resource(ORG, "shop", "development", descriptor)
            .await
            .unwrap()
    }
}

fn audit(method: &str, path: &str) -> AuditIntent {
    AuditIntent::accepted(ORG, "ci", method, path, "test")
}

/// An intent for a change the server makes itself, outside any caller's authority.
fn system(method: &str, path: &str) -> AuditIntent {
    AuditIntent {
        actor: None,
        ..audit(method, path)
    }
}

fn provisioned(host: &str) -> CheckpointResult {
    CheckpointResult::Provisioned(Box::new(Provisioned {
        outputs: json!({"host": host, "port": 5432}),
        secret_outputs: BTreeMap::from([(
            "password".to_owned(),
            SecretRef {
                store: "openporch".into(),
                reference: "resources/x/password".into(),
                version: Some("1".into()),
            },
        )]),
        cookie: None,
        recovery: json!({}),
        inputs: json!({"values": {"size": "small"}}),
        dependencies: vec!["postgres-instance.default#modules.web.externals.db".into()],
        snapshot: json!({"definition_version": 1}),
        account: None,
        target: None,
        backend: None,
        external_ids: Vec::new(),
    }))
}

const DB: &str = "postgres.default#modules.web.externals.db";

#[tokio::test]
async fn second_owner_is_refused() {
    let f = Fixture::new().await;
    let e = Ownership::acquire(f.dir.path()).unwrap_err();
    assert!(matches!(e, StoreError::AlreadyOwned { .. }), "{e}");
    assert!(e.to_string().contains("owned by another openporch process"));
}

#[tokio::test]
async fn ownership_returns_after_the_owner_exits() {
    let f = Fixture::new().await;
    // Any clone of the store keeps the installation owned.
    let clone = f.store.clone();
    let Fixture { dir, store } = f;
    drop(store);
    assert!(Ownership::acquire(dir.path()).is_err());
    clone.pool().close().await;
    drop(clone);
    let ownership = Ownership::acquire(dir.path()).unwrap();
    let store = SqliteStore::open(ownership, ORG).await.unwrap();
    let owner: Option<String> = sqlx::query_scalar("SELECT owner_id FROM installation")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(owner.as_deref(), Some(store.ownership().id()));
}

#[tokio::test]
async fn installation_serves_one_organization() {
    let Fixture { dir, store } = Fixture::new().await.close().await;
    drop(store);
    let ownership = Ownership::acquire(dir.path()).unwrap();
    let e = SqliteStore::open(ownership, "other").await.unwrap_err();
    assert!(e.to_string().contains("serves organization `acme`"), "{e}");
}

#[tokio::test]
async fn newer_schema_is_refused_without_changes() {
    let f = Fixture::new().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, installed_on, success, checksum, \
         execution_time) VALUES (99990101000000, 'future', CURRENT_TIMESTAMP, 1, x'00', 0)",
    )
    .execute(f.store.pool())
    .await
    .unwrap();
    let Fixture { dir, store } = f.close().await;
    drop(store);
    let ownership = Ownership::acquire(dir.path()).unwrap();
    let e = SqliteStore::open(ownership, ORG).await.unwrap_err();
    assert!(
        matches!(
            e,
            StoreError::SchemaTooNew {
                found: 99990101000000,
                ..
            }
        ),
        "{e}"
    );
}

#[tokio::test]
async fn defaults_exist_after_first_start() {
    let f = Fixture::new().await;
    let env = f
        .store
        .env(ORG, "shop", "development")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(env.env_type, "development");
    assert_eq!(env.generation, 0);
    let primary: String = sqlx::query_scalar("SELECT id FROM secret_stores WHERE is_primary = 1")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(primary, "openporch");
    let owner: String =
        sqlx::query_scalar("SELECT role FROM app_roles WHERE principal_id = 'alice'")
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(owner, "owner");
}

#[tokio::test]
async fn acceptance_records_job_desired_set_and_audit_together() {
    let f = Fixture::new().await;
    let set = f.set("nginx:1").await;
    f.store
        .put_delta(
            ORG,
            "shop",
            "d1",
            &json!({"modules": {}}),
            // The application's owner; deltas need `developer` or `owner`.
            AuditIntent::accepted(
                ORG,
                "alice",
                "PUT",
                "/orgs/acme/apps/shop/deltas/d1",
                "test",
            ),
        )
        .await
        .unwrap();
    let mut req = f.request(0, &set, Some("key-1"));
    req.delta_id = Some("d1".into());
    let d = match f.store.accept_deployment(req).await.unwrap() {
        Acceptance::Created(d) => d,
        other => panic!("{other:?}"),
    };
    assert_eq!(d.status, DeploymentStatus::InProgress);
    assert_eq!((d.seq, d.generation), (1, 1));

    let env = f
        .store
        .env(ORG, "shop", "development")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(env.generation, 1);
    assert_eq!(env.desired_set_id.as_deref(), Some(set.as_str()));
    assert_eq!(env.last_successful_set_id, None);

    // The delta is frozen.
    let e = f
        .store
        .put_delta(
            ORG,
            "shop",
            "d1",
            &json!({"modules": {"remove": ["web"]}}),
            AuditIntent::accepted(
                ORG,
                "alice",
                "PUT",
                "/orgs/acme/apps/shop/deltas/d1",
                "test",
            ),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("can no longer change"), "{e}");
    let delta_audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE method = 'PUT' AND path LIKE '%/deltas/d1'",
    )
    .fetch_one(f.store.pool())
    .await
    .unwrap();
    assert_eq!(delta_audits, 1, "the rejected change must not be audited");

    let entries = f.store.audit_entries(ORG, 10).await.unwrap();
    let accepted = &entries[0];
    assert_eq!(accepted.outcome, "accepted");
    assert_eq!(accepted.operation_id.as_deref(), Some(d.id.as_str()));
    assert_eq!(accepted.actor_id.as_deref(), Some("ci"));
    assert_eq!(accepted.target.as_deref(), Some("shop/development"));
}

#[tokio::test]
async fn stale_generation_and_running_deployments_conflict_without_changes() {
    let f = Fixture::new().await;
    let first = f.deploy("nginx:1").await;
    let set = f.set("nginx:2").await;

    let e = f
        .store
        .accept_deployment(f.request(0, &set, None))
        .await
        .unwrap_err();
    assert!(
        matches!(
            e,
            StoreError::StaleGeneration {
                expected: 0,
                actual: 1
            }
        ),
        "{e}"
    );
    let e = f
        .store
        .accept_deployment(f.request(1, &set, None))
        .await
        .unwrap_err();
    assert!(matches!(&e, StoreError::Busy(id) if *id == first.id), "{e}");

    let env = f
        .store
        .env(ORG, "shop", "development")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(env.generation, 1);
    assert_ne!(env.desired_set_id.as_deref(), Some(set.as_str()));
}

#[tokio::test]
async fn idempotent_retry_returns_the_original_deployment() {
    let f = Fixture::new().await;
    let set = f.set("nginx:1").await;
    let first = f
        .store
        .accept_deployment(f.request(0, &set, Some("k")))
        .await
        .unwrap();
    // The generation has moved on, but the retry still gets the original.
    let retry = f
        .store
        .accept_deployment(f.request(0, &set, Some("k")))
        .await
        .unwrap();
    assert!(matches!(retry, Acceptance::Replayed(_)));
    assert_eq!(retry.deployment(), first.deployment());
    assert_eq!(f.generation().await, 1);

    let other = f.set("nginx:2").await;
    let e = f
        .store
        .accept_deployment(f.request(1, &other, Some("k")))
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::IdempotencyMismatch(_)), "{e}");
}

#[tokio::test]
async fn failed_audit_write_rejects_the_mutation() {
    let f = Fixture::new().await;
    let set = f.set("nginx:1").await;
    sqlx::query(
        "CREATE TRIGGER audit_full BEFORE INSERT ON audit_log \
         BEGIN SELECT RAISE(ABORT, 'audit storage unavailable'); END",
    )
    .execute(f.store.pool())
    .await
    .unwrap();

    let e = f
        .store
        .accept_deployment(f.request(0, &set, None))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("audit storage unavailable"), "{e}");
    let env = f
        .store
        .env(ORG, "shop", "development")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(env.generation, 0);
    assert_eq!(env.desired_set_id, None);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM deployments")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);

    let e = f
        .store
        .create_env_type(ORG, "staging", "", system("POST", "/orgs/acme/env-types"))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("audit storage unavailable"), "{e}");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM env_types WHERE id = 'staging'")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn guresid_is_allocated_once_and_kept_across_deployments() {
    let f = Fixture::new().await;
    let d1 = f.deploy("nginx:1").await;
    let op = f.start(&d1.id, DB).await.unwrap();
    assert_eq!(op.attempt, 1);
    // The record exists before the driver is called.
    let r = f.resource(DB).await.unwrap();
    assert_eq!(r.guresid, op.guresid.as_str());
    assert_eq!(r.status, "provisioning");

    f.store
        .checkpoint_operation(&op.operation_id, provisioned("db1"))
        .await
        .unwrap();
    f.store
        .finish_deployment(ORG, "shop", "development", &d1.id, None)
        .await
        .unwrap();

    let d2 = f.deploy("nginx:2").await;
    let op2 = f.start(&d2.id, DB).await.unwrap();
    assert_eq!(op2.guresid, op.guresid);
    assert_ne!(op2.operation_id, op.operation_id);
}

#[tokio::test]
async fn a_driver_cannot_take_over_what_a_failed_attempt_left_behind() {
    let f = Fixture::new().await;
    let d1 = f.deploy("nginx:1").await;
    let op = f.start(&d1.id, DB).await.unwrap();
    f.store
        .checkpoint_operation(
            &op.operation_id,
            CheckpointResult::Failed {
                error: "apply failed".into(),
            },
        )
        .await
        .unwrap();
    f.store
        .finish_deployment(ORG, "shop", "development", &d1.id, Some("failed".into()))
        .await
        .unwrap();
    let other = |deployment: &str| OperationStart {
        org: ORG.into(),
        app: "shop".into(),
        env: "development".into(),
        deployment_id: deployment.into(),
        descriptor: Descriptor::parse(DB).unwrap(),
        definition_id: Some("rds".into()),
        definition_version: Some(1),
        driver_type: Some("openporch/template".into()),
        kind: OperationKind::Put,
        input_fingerprint: "inputs-1".into(),
        recovery: json!({}),
    };

    // The first attempt left a target behind, so its driver keeps the resource.
    sqlx::query("UPDATE active_resources SET target = 'aws:111122223333' WHERE descriptor = ?")
        .bind(DB)
        .execute(f.store.pool())
        .await
        .unwrap();
    let d2 = f.deploy("nginx:2").await;
    let err = f.store.begin_operation(other(&d2.id)).await.unwrap_err();
    assert!(
        matches!(err, StoreError::MigrationRequired { .. }),
        "{err:?}"
    );
    f.store
        .finish_deployment(ORG, "shop", "development", &d2.id, Some("failed".into()))
        .await
        .unwrap();

    // An attempt that left nothing behind may be retried with another driver.
    sqlx::query("UPDATE active_resources SET target = NULL WHERE descriptor = ?")
        .bind(DB)
        .execute(f.store.pool())
        .await
        .unwrap();
    let d3 = f.deploy("nginx:3").await;
    f.store.begin_operation(other(&d3.id)).await.unwrap();
}

#[tokio::test]
async fn checkpoints_survive_partial_failure() {
    let f = Fixture::new().await;
    let d1 = f.deploy("nginx:1").await;
    let op = f.start(&d1.id, DB).await.unwrap();
    f.store
        .checkpoint_operation(&op.operation_id, provisioned("db1"))
        .await
        .unwrap();
    f.store
        .finish_deployment(ORG, "shop", "development", &d1.id, None)
        .await
        .unwrap();
    let env = f
        .store
        .env(ORG, "shop", "development")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        env.last_successful_deploy_id.as_deref(),
        Some(d1.id.as_str())
    );

    // The second deployment updates the database, then the workload fails.
    let d2 = f.deploy("nginx:2").await;
    let db = f.start(&d2.id, DB).await.unwrap();
    f.store
        .checkpoint_operation(&db.operation_id, provisioned("db2"))
        .await
        .unwrap();
    let wl = f
        .start(&d2.id, "workload.default#modules.web")
        .await
        .unwrap();
    f.store
        .checkpoint_operation(
            &wl.operation_id,
            CheckpointResult::Failed {
                error: "image not found".into(),
            },
        )
        .await
        .unwrap();
    f.store
        .finish_deployment(
            ORG,
            "shop",
            "development",
            &d2.id,
            Some("workload failed".into()),
        )
        .await
        .unwrap();

    let r = f.resource(DB).await.unwrap();
    assert_eq!(r.outputs["host"], "db2");
    assert_eq!(r.last_deploy_id.as_deref(), Some(d2.id.as_str()));
    assert_eq!(r.secret_outputs["password"].version.as_deref(), Some("1"));
    assert_eq!(
        f.resource("workload.default#modules.web")
            .await
            .unwrap()
            .status,
        "failed"
    );
    let env = f
        .store
        .env(ORG, "shop", "development")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        env.last_successful_deploy_id.as_deref(),
        Some(d1.id.as_str())
    );
    assert_ne!(env.desired_set_id, env.last_successful_set_id);
    let d2 = f
        .store
        .deployment(ORG, "shop", "development", &d2.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d2.status, DeploymentStatus::Failed);

    // The outcome is correlated with the accepting entry.
    let entries = f.store.audit_entries(ORG, 20).await.unwrap();
    let outcome = entries
        .iter()
        .find(|e| e.operation_id.as_deref() == Some(d2.id.as_str()) && e.outcome == "failed")
        .unwrap();
    let accepted = entries
        .iter()
        .find(|e| e.operation_id.as_deref() == Some(d2.id.as_str()) && e.outcome == "accepted")
        .unwrap();
    assert_eq!(outcome.correlates.as_deref(), Some(accepted.id.as_str()));
    assert_eq!(outcome.actor_id.as_deref(), Some("ci"));
}

#[tokio::test]
async fn restart_recovery_leaves_a_finished_log_alone() {
    let f = Fixture::new().await;
    let d1 = f.deploy("nginx:1").await;
    f.start(&d1.id, DB).await.unwrap();
    // The deployment finished, as after a planner panic, with its operation
    // still open.
    f.store
        .finish_deployment(ORG, "shop", "development", &d1.id, Some("stopped".into()))
        .await
        .unwrap();
    let f = f.restart().await;
    let report = f.store.recover_interrupted().await.unwrap();
    assert_eq!(report.uncertain.len(), 1);
    let lines = f
        .store
        .deployment_logs(ORG, "shop", "development", &d1.id, None, 100)
        .await
        .unwrap();
    let messages: Vec<_> = lines.iter().map(|l| l.message.as_str()).collect();
    assert_eq!(messages, ["deployment failed: stopped"]);
}

#[tokio::test]
async fn restart_fails_interrupted_work_and_blocks_uncertain_resources() {
    let f = Fixture::new().await;
    let d1 = f.deploy("nginx:1").await;
    let op = f.start(&d1.id, DB).await.unwrap();
    // Crash before the result is checkpointed.
    let f = f.restart().await;

    let report = f.store.recover_interrupted().await.unwrap();
    assert_eq!(report.failed_deployments, vec![d1.id.clone()]);
    assert_eq!(
        report.uncertain,
        [UncertainOperation {
            operation_id: op.operation_id.clone(),
            deployment_id: d1.id.clone(),
            guresid: op.guresid.to_string(),
        }]
    );
    let d1 = f
        .store
        .deployment(ORG, "shop", "development", &d1.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(d1.status, DeploymentStatus::Failed);
    assert_eq!(f.resource(DB).await.unwrap().status, "unknown");
    // The log ends with what the restart did.
    let lines = f
        .store
        .deployment_logs(ORG, "shop", "development", &d1.id, None, 100)
        .await
        .unwrap();
    let summary: Vec<_> = lines
        .iter()
        .map(|l| {
            (
                l.descriptor.as_deref(),
                l.operation_id.as_deref(),
                l.level.as_str(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (Some(DB), Some(op.operation_id.as_str()), "error"),
            (None, None, "error"),
        ]
    );
    assert!(lines[0].message.starts_with("outcome unknown: interrupted"));
    assert_eq!(lines[0].guresid.as_deref(), Some(op.guresid.as_str()));
    assert_eq!(
        lines[1].message,
        "deployment failed: interrupted by a server restart; 1 resource(s) may have changed \
         and need recovery before the environment is deployed again"
    );
    // A finished deployment's log takes no more lines.
    let late = LogLine {
        seq: 0,
        operation_id: None,
        descriptor: None,
        guresid: None,
        at: 0,
        level: "info".into(),
        message: "late".into(),
    };
    f.store
        .append_logs(ORG, "shop", "development", &d1.id, vec![late])
        .await
        .unwrap();
    let after = f
        .store
        .deployment_logs(ORG, "shop", "development", &d1.id, None, 100)
        .await
        .unwrap();
    assert_eq!(after, lines);

    // New deployments are refused until the outcome is resolved.
    let set = f.set("nginx:2").await;
    let e = f
        .store
        .accept_deployment(f.request(f.generation().await, &set, None))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, StoreError::RecoveryRequired(d) if d == DB),
        "{e}"
    );

    // A guess or a result that does not fit the operation does not resolve it.
    for result in [
        CheckpointResult::Failed {
            error: "gave up".into(),
        },
        CheckpointResult::Unknown {
            error: "still unsure".into(),
        },
        CheckpointResult::Deleted,
    ] {
        let e = f
            .store
            .checkpoint_operation(&op.operation_id, result)
            .await
            .unwrap_err();
        assert!(matches!(e, StoreError::Conflict(_)), "{e}");
    }
    assert_eq!(f.resource(DB).await.unwrap().status, "unknown");

    // Resolving the operation (here: observed as provisioned) unblocks them, and the
    // same GUResID is kept.
    f.store
        .checkpoint_operation(&op.operation_id, provisioned("db1"))
        .await
        .unwrap();
    let d2 = f
        .store
        .accept_deployment(f.request(f.generation().await, &set, None))
        .await
        .unwrap();
    let again = f.start(&d2.deployment().id, DB).await.unwrap();
    assert_eq!(again.guresid, op.guresid);
}

#[tokio::test]
async fn operations_need_a_running_deployment_and_one_at_a_time() {
    let f = Fixture::new().await;
    let d1 = f.deploy("nginx:1").await;
    f.start(&d1.id, DB).await.unwrap();
    let e = f.start(&d1.id, DB).await.unwrap_err();
    assert!(e.to_string().contains("has not finished"), "{e}");

    f.store
        .finish_deployment(ORG, "shop", "development", &d1.id, Some("x".into()))
        .await
        .unwrap();
    let e = f.start(&d1.id, DB).await.unwrap_err();
    assert!(e.to_string().contains("is failed"), "{e}");
}

#[tokio::test]
async fn deletion_leaves_a_tombstone() {
    let f = Fixture::new().await;
    let d1 = f.deploy("nginx:1").await;
    let op = f.start(&d1.id, DB).await.unwrap();
    f.store
        .checkpoint_operation(&op.operation_id, provisioned("db1"))
        .await
        .unwrap();
    let del = f
        .store
        .begin_operation(OperationStart {
            kind: OperationKind::Delete,
            ..OperationStart {
                org: ORG.into(),
                app: "shop".into(),
                env: "development".into(),
                deployment_id: d1.id.clone(),
                descriptor: Descriptor::parse(DB).unwrap(),
                definition_id: None,
                definition_version: None,
                driver_type: None,
                kind: OperationKind::Put,
                input_fingerprint: "x".into(),
                recovery: json!({}),
            }
        })
        .await
        .unwrap();
    assert_eq!(f.resource(DB).await.unwrap().status, "deleting");
    f.store
        .checkpoint_operation(
            &del.operation_id,
            CheckpointResult::Failed {
                error: "still in use".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(f.resource(DB).await.unwrap().status, "deletion_failed");
    let del = f
        .store
        .begin_operation(OperationStart {
            kind: OperationKind::Delete,
            ..OperationStart {
                org: ORG.into(),
                app: "shop".into(),
                env: "development".into(),
                deployment_id: d1.id.clone(),
                descriptor: Descriptor::parse(DB).unwrap(),
                definition_id: None,
                definition_version: None,
                driver_type: None,
                kind: OperationKind::Put,
                input_fingerprint: "x".into(),
                recovery: json!({}),
            }
        })
        .await
        .unwrap();
    assert_eq!(del.attempt, 3);
    // A delete is never checkpointed as provisioned.
    let e = f
        .store
        .checkpoint_operation(&del.operation_id, provisioned("db1"))
        .await
        .unwrap_err();
    assert!(matches!(e, StoreError::Conflict(_)), "{e}");
    f.store
        .checkpoint_operation(&del.operation_id, CheckpointResult::Deleted)
        .await
        .unwrap();
    assert!(f.resource(DB).await.is_none());
    let run_status: String = sqlx::query_scalar(
        "SELECT status FROM deployment_resources WHERE deployment_id = ? AND descriptor = ?",
    )
    .bind(&d1.id)
    .bind(DB)
    .fetch_one(f.store.pool())
    .await
    .unwrap();
    assert_eq!(run_status, "deleted");
    let (guresid, reason): (String, String) =
        sqlx::query_as("SELECT guresid, reason FROM tombstones")
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(guresid, op.guresid.as_str());
    assert_eq!(reason, "deleted");
}

#[tokio::test]
async fn denied_requests_are_recorded() {
    let f = Fixture::new().await;
    let mut intent = audit("DELETE", "/orgs/acme/apps/shop");
    intent.outcome = AuditOutcome::Denied;
    intent.status_code = Some(403);
    intent.reason = Some("missing_role".into());
    let id = f.store.record_audit(intent).await.unwrap();
    let e = &f.store.audit_entries(ORG, 1).await.unwrap()[0];
    assert_eq!(e.id, id);
    assert_eq!((e.outcome.as_str(), e.status_code), ("denied", Some(403)));
}

#[tokio::test]
async fn concurrent_acceptances_admit_one() {
    let f = Fixture::new().await;
    let a = f.set("nginx:1").await;
    let b = f.set("nginx:2").await;
    let (ra, rb) = tokio::join!(
        f.store.accept_deployment(f.request(0, &a, None)),
        f.store.accept_deployment(f.request(0, &b, None)),
    );
    assert_eq!(ra.is_ok() as u8 + rb.is_ok() as u8, 1, "{ra:?} {rb:?}");
    assert_eq!(f.generation().await, 1);
}
