//! Deleting an environment: every owned resource is deleted in reverse
//! dependency order, then the environment; a failure keeps it for a retry.

use super::*;
use openporch_store::{Acceptance, AppDeletionRequest, DeploymentRecord, IdentityStore};

const ENV: &str = "pr-42";

fn teardown_snapshot() -> serde_json::Value {
    json!({
        "version": 1,
        "kind": "delete_env",
        "permits": ["permit-1"],
        "graph": {"nodes": {}, "layers": []},
        "definitions": {},
    })
}

/// The deletion the runner runs for `record`.
fn teardown_of(record: &DeploymentRecord) -> Deployment {
    let mut deployment = Deployment::from_snapshot(
        ORG.parse().unwrap(),
        "shop".parse().unwrap(),
        record.env.parse().unwrap(),
        "development".parse().unwrap(),
        record.id.clone(),
        String::new(),
        &teardown_snapshot(),
    )
    .unwrap();
    deployment.teardown = true;
    deployment
}

impl Fixture {
    /// Accepts the deletion of the fixture's environment, as the server does.
    async fn accept_teardown(&self) -> Result<Deployment, StoreError> {
        let env = self.store.env(ORG, "shop", self.env).await?.unwrap();
        let snapshot = teardown_snapshot();
        let accepted = self
            .store
            .accept_deployment(DeploymentRequest {
                permits: Default::default(),
                org: ORG.into(),
                app: "shop".into(),
                env: self.env.into(),
                actor: "ci".into(),
                expected_generation: env.generation,
                idempotency_key: None,
                fingerprint: format!("delete-{}", env.generation),
                kind: DeploymentKind::DeleteEnv,
                set_id: None,
                delta_id: None,
                delta_content: None,
                comment: String::new(),
                snapshot: snapshot.clone(),
                audit: audit("DELETE", "/orgs/acme/apps/shop/envs/pr-42"),
            })
            .await?;
        Ok(teardown_of(accepted.deployment()))
    }

    /// Accepts the deletion of the application, as the server does, and
    /// returns the deletions it started, by environment.
    async fn accept_app_teardown(&self) -> Result<Vec<(String, Deployment)>, StoreError> {
        let deletion = self
            .store
            .accept_app_deletion(AppDeletionRequest {
                permits: Default::default(),
                org: ORG.into(),
                app: "shop".into(),
                actor: "root".into(),
                comment: String::new(),
                snapshot: teardown_snapshot(),
                audit: AuditIntent {
                    actor: Some("root".into()),
                    ..audit("DELETE", "/orgs/acme/apps/shop")
                },
            })
            .await?;
        Ok(deletion
            .deletions
            .iter()
            .filter(|a| matches!(a, Acceptance::Created(_)))
            .map(|a| (a.deployment().env.clone(), teardown_of(a.deployment())))
            .collect())
    }

    /// The application's state, or `None` once it is gone.
    async fn app_state(&self) -> Option<String> {
        self.store.app(ORG, "shop").await.unwrap().map(|a| a.state)
    }

    /// The environment's state, or `None` once it is gone.
    async fn env_state(&self) -> Option<String> {
        self.store
            .env(ORG, "shop", self.env)
            .await
            .unwrap()
            .map(|e| e.state)
    }
}

#[tokio::test]
async fn successful_deletion_goes_in_reverse_dependency_order() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    let deployed = f.accept().await;
    deleting(&f, &fake, &checks).run(deployed.clone()).await;
    assert_eq!(f.status(&deployed).await.0, DeploymentStatus::Succeeded);
    let active = f.store.active_resources(ORG, "shop", ENV).await.unwrap();
    assert_eq!(active.len(), 5, "{active:?}");

    let teardown = f.accept_teardown().await.unwrap();
    assert_eq!(f.env_state().await.as_deref(), Some("deleting"));
    let outcomes = deleting(&f, &fake, &checks).run(teardown).await;
    for r in &active {
        assert_eq!(
            outcomes[&d(&r.descriptor)],
            Outcome::Deleted,
            "{}",
            r.descriptor
        );
    }
    // Everything that used a resource was deleted before it: the workload
    // before its database, and the database before the environment's base.
    let order = fake.deletions();
    let at = |descriptor: &str| order.iter().position(|d| d == descriptor).unwrap();
    assert_eq!(order.len(), active.len(), "{order:?}");
    assert!(at(WORKLOAD) < at(DB), "{order:?}");
    for r in &active {
        for dep in &r.dependencies {
            assert!(
                at(&r.descriptor) < at(dep),
                "{} before {dep}: {order:?}",
                r.descriptor
            );
        }
    }
    // Every deletion was checked against the deletion's permits.
    let asked = checks.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), active.len());
    assert!(asked.iter().all(
        |(_, action, permits)| *action == DestructiveAction::Delete && permits == &["permit-1"]
    ));

    // The environment and its records are gone; the tombstones stay.
    assert_eq!(f.env_state().await, None);
    let (deployments,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM deployments WHERE env_id = ?")
            .bind(ENV)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(deployments, 0);
    let tombstones = f.tombstones().await;
    assert_eq!(tombstones.len(), active.len());
    assert!(tombstones.iter().all(|(_, _, reason)| reason == "deleted"));
    // The default environment is untouched.
    assert!(
        f.store
            .env(ORG, "shop", "development")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_failed_deletion_keeps_the_environment_for_a_retry() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    let db_guresid = f.active(DB).await.unwrap().guresid;
    fake.deletions
        .lock()
        .unwrap()
        .insert("postgres".into(), Behavior::Permanent);

    let teardown = f.accept_teardown().await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(teardown.clone()).await;
    assert_eq!(outcomes[&d(WORKLOAD)], Outcome::Deleted);
    assert!(
        matches!(&outcomes[&d(DB)], Outcome::Failed(e) if e.contains("deletion protection")),
        "{outcomes:?}"
    );
    let (status, error) = f.status(&teardown).await;
    assert_eq!(status, DeploymentStatus::Failed);
    assert!(error.unwrap().contains(DB));

    // The environment stays with the failed resource, its error and what it
    // still uses.
    assert_eq!(f.env_state().await.as_deref(), Some("deletion_failed"));
    let db = f.active(DB).await.unwrap();
    assert_eq!(db.status, "deletion_failed");
    let runs = f.resources(&teardown).await;
    assert_eq!(runs[DB], "failed");
    let (db_error,): (Option<String>,) = sqlx::query_as(
        "SELECT error FROM deployment_resources WHERE deployment_id = ? AND descriptor = ?",
    )
    .bind(&teardown.id)
    .bind(DB)
    .fetch_one(f.store.pool())
    .await
    .unwrap();
    assert!(db_error.unwrap().contains("deletion protection"));
    for dep in &db.dependencies {
        assert!(
            f.active(dep).await.is_some(),
            "{dep} was deleted under {DB}"
        );
    }
    assert!(f.active(WORKLOAD).await.is_none());

    // It cannot be deployed while it waits for the deletion to be retried.
    assert!(matches!(
        f.try_accept().await,
        Err(StoreError::Conflict(e)) if e.contains("deletion_failed")
    ));

    // The retry deletes what is left with the same identities, then the
    // environment.
    fake.deletions.lock().unwrap().clear();
    let retry = f.accept_teardown().await.unwrap();
    let before = fake.deletions().len();
    let outcomes = deleting(&f, &fake, &checks).run(retry).await;
    assert_eq!(outcomes[&d(DB)], Outcome::Deleted);
    assert!(!outcomes.contains_key(&d(WORKLOAD)), "{outcomes:?}");
    let db_jobs: Vec<_> = fake
        .deleted
        .lock()
        .unwrap()
        .iter()
        .filter(|j| j.job.descriptor == d(DB))
        .map(|j| j.job.guresid.to_string())
        .collect();
    assert_eq!(db_jobs, [db_guresid.to_string(), db_guresid.to_string()]);
    assert!(fake.deletions().len() > before);
    assert_eq!(f.env_state().await, None);
}

#[tokio::test]
async fn adopted_and_external_resources_are_released_not_deleted() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // The DNS zone is managed elsewhere, and the base environment was adopted.
    for (descriptor, ownership) in [(DNS, "external"), (BASE, "adopted")] {
        sqlx::query("UPDATE active_resources SET ownership = ? WHERE descriptor = ?")
            .bind(ownership)
            .bind(descriptor)
            .execute(f.store.pool())
            .await
            .unwrap();
    }

    let teardown = f.accept_teardown().await.unwrap();
    // The deletion succeeds, so the environment and its deployments are gone.
    let outcomes = deleting(&f, &fake, &checks).run(teardown).await;
    assert!(!outcomes.contains_key(&d(DNS)));
    assert!(!outcomes.contains_key(&d(BASE)));
    let deleted = fake.deletions();
    assert!(!deleted.contains(&DNS.to_owned()), "{deleted:?}");
    assert!(!deleted.contains(&BASE.to_owned()), "{deleted:?}");
    assert_eq!(f.env_state().await, None);
    let tombstones = f.tombstones().await;
    for descriptor in [DNS, BASE] {
        assert!(
            tombstones
                .iter()
                .any(|(d, _, reason)| d == descriptor && reason == "released"),
            "{tombstones:?}"
        );
    }
}

#[tokio::test]
async fn a_deletion_without_permits_keeps_everything() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    deleting(&f, &fake, &Arc::default())
        .run(f.accept().await)
        .await;
    let teardown = f.accept_teardown().await.unwrap();
    // The planner's default refuses every destructive action.
    let outcomes = planner(&f, &fake, fast()).run(teardown.clone()).await;
    assert!(
        matches!(&outcomes[&d(WORKLOAD)], Outcome::Failed(e) if e.starts_with("approval_required")),
        "{outcomes:?}"
    );
    assert!(fake.deletions().is_empty());
    assert_eq!(f.status(&teardown).await.0, DeploymentStatus::Failed);
    assert_eq!(f.env_state().await.as_deref(), Some("deletion_failed"));
    assert_eq!(
        f.store
            .active_resources(ORG, "shop", ENV)
            .await
            .unwrap()
            .len(),
        5
    );
}

#[tokio::test]
async fn the_development_environment_is_not_deleted() {
    let f = Fixture::new().await;
    assert!(matches!(
        f.accept_teardown().await,
        Err(StoreError::Conflict(e)) if e.contains("development")
    ));
}

#[tokio::test]
async fn a_running_deletion_is_not_started_again_and_a_restart_fails_it() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    deleting(&f, &fake, &Arc::default())
        .run(f.accept().await)
        .await;
    let teardown = f.accept_teardown().await.unwrap();
    assert!(matches!(
        f.accept_teardown().await,
        Err(StoreError::Conflict(e)) if e.contains("already being deleted")
    ));
    // A restart fails the interrupted deletion; the environment can be
    // deleted again.
    let report = f.store.recover_interrupted().await.unwrap();
    assert!(report.failed_deployments.contains(&teardown.id));
    assert_eq!(f.env_state().await.as_deref(), Some("deletion_failed"));
    f.accept_teardown().await.unwrap();
}

#[tokio::test]
async fn a_scheduled_adopted_resource_must_be_deleted_before_the_environment_goes() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // The base environment was adopted and scheduled for deletion, but the
    // external DNS zone, which stays, uses it.
    for (descriptor, ownership) in [(BASE, "adopted"), (DNS, "external")] {
        sqlx::query("UPDATE active_resources SET ownership = ? WHERE descriptor = ?")
            .bind(ownership)
            .bind(descriptor)
            .execute(f.store.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE active_resources SET dependencies = ? WHERE descriptor = ?")
        .bind(json!([BASE]).to_string())
        .bind(DNS)
        .execute(f.store.pool())
        .await
        .unwrap();
    f.schedule(BASE).await;

    let teardown = f.accept_teardown().await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(teardown.clone()).await;
    assert!(
        matches!(&outcomes[&d(BASE)], Outcome::Pending(r) if r.contains(DNS)),
        "{outcomes:?}"
    );
    assert_eq!(f.status(&teardown).await.0, DeploymentStatus::Failed);
    assert_eq!(f.env_state().await.as_deref(), Some("deletion_failed"));
    assert!(f.active(BASE).await.is_some());
}

#[tokio::test]
async fn the_runner_deletes_an_environment_without_resources() {
    // The server finishes such a deletion itself, and leaves it to the
    // runner when it cannot.
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let teardown = f.accept_teardown().await.unwrap();
    let outcomes = deleting(&f, &fake, &Arc::default()).run(teardown).await;
    assert!(outcomes.is_empty(), "{outcomes:?}");
    assert!(fake.deletions().is_empty());
    assert_eq!(f.env_state().await, None);
}

#[tokio::test]
async fn deleting_an_application_deletes_every_environment_then_the_application() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // The DNS zone is managed elsewhere.
    sqlx::query("UPDATE active_resources SET ownership = 'external' WHERE descriptor = ?")
        .bind(DNS)
        .execute(f.store.pool())
        .await
        .unwrap();
    let owned: Vec<String> = f
        .store
        .active_resources(ORG, "shop", ENV)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.ownership == "owned")
        .map(|r| r.descriptor)
        .collect();

    let teardowns = f.accept_app_teardown().await.unwrap();
    let envs: Vec<_> = teardowns.iter().map(|(env, _)| env.as_str()).collect();
    assert_eq!(envs, [ENV, "development"]);
    assert_eq!(f.app_state().await.as_deref(), Some("deleting"));
    // It takes no new environments while it is being deleted.
    let created = f
        .store
        .create_env(
            ORG,
            "shop",
            NewEnv {
                id: "pr-43".into(),
                name: "pr-43".into(),
                env_type: "development".into(),
                draft: None,
            },
            audit("POST", "/orgs/acme/apps/shop/envs"),
        )
        .await;
    assert!(
        matches!(&created, Err(StoreError::Conflict(e)) if e.contains("being deleted")),
        "{created:?}"
    );

    let mut teardowns = teardowns.into_iter();
    let (_, resources) = teardowns.next().unwrap();
    deleting(&f, &fake, &checks).run(resources).await;
    // The environment is gone, and the application waits for `development`.
    assert_eq!(f.env_state().await, None);
    assert_eq!(f.app_state().await.as_deref(), Some("deleting"));
    let (_, development) = teardowns.next().unwrap();
    deleting(&f, &fake, &checks).run(development).await;
    assert_eq!(f.app_state().await, None);

    // Owned resources were deleted; the external DNS zone stays and is
    // released with a tombstone.
    let mut deleted = fake.deletions();
    deleted.sort();
    let mut expected = owned;
    expected.sort();
    assert_eq!(deleted, expected);
    assert!(
        f.tombstones()
            .await
            .iter()
            .any(|(d, _, reason)| d == DNS && reason == "released")
    );
}

#[tokio::test]
async fn a_failed_environment_keeps_the_application_for_a_retry() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    fake.deletions
        .lock()
        .unwrap()
        .insert("postgres".into(), Behavior::Permanent);

    for (_, teardown) in f.accept_app_teardown().await.unwrap() {
        deleting(&f, &fake, &checks).run(teardown).await;
    }
    // `development` is gone; the failed environment and the application stay.
    assert!(
        f.store
            .env(ORG, "shop", "development")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.env_state().await.as_deref(), Some("deletion_failed"));
    assert_eq!(f.app_state().await.as_deref(), Some("deleting"));
    assert!(f.active(DB).await.is_some());
    // Nothing can be deployed to it.
    assert!(matches!(f.try_accept().await, Err(StoreError::Conflict(_))));

    // Repeating the deletion retries what is left, and then the application
    // goes.
    fake.deletions.lock().unwrap().clear();
    let retry = f.accept_app_teardown().await.unwrap();
    assert_eq!(retry.len(), 1);
    assert_eq!(retry[0].0, ENV);
    for (_, teardown) in retry {
        deleting(&f, &fake, &checks).run(teardown).await;
    }
    assert_eq!(f.env_state().await, None);
    assert_eq!(f.app_state().await, None);
}

#[tokio::test]
async fn an_application_deletion_includes_a_running_environment_deletion() {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    deleting(&f, &fake, &Arc::default())
        .run(f.accept().await)
        .await;
    let running = f.accept_teardown().await.unwrap();
    // Only `development` gets a new deletion; the running one is part of it.
    let teardowns = f.accept_app_teardown().await.unwrap();
    let envs: Vec<_> = teardowns.iter().map(|(env, _)| env.as_str()).collect();
    assert_eq!(envs, ["development"]);
    let checks = Arc::new(AllowAll::default());
    for (_, teardown) in teardowns {
        deleting(&f, &fake, &checks).run(teardown).await;
    }
    assert_eq!(f.app_state().await.as_deref(), Some("deleting"));
    deleting(&f, &fake, &checks).run(running).await;
    assert_eq!(f.app_state().await, None);
}

#[tokio::test]
async fn what_a_detached_resource_uses_blocks_the_deletion() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    detach_as_root(&f, WORKLOAD).await.unwrap();

    // The detached workload keeps running and uses the database, which can
    // only be deleted once that object is removed outside openporch.
    for (_, teardown) in f.accept_app_teardown().await.unwrap() {
        let outcomes = deleting(&f, &fake, &checks).run(teardown).await;
        assert!(
            matches!(&outcomes[&d(DB)], Outcome::Pending(r)
                if r.contains("no longer manages") && r.contains("detach this resource")),
            "{outcomes:?}"
        );
    }
    assert_eq!(f.env_state().await.as_deref(), Some("deletion_failed"));
    assert_eq!(f.app_state().await.as_deref(), Some("deleting"));
    assert!(f.active(DB).await.is_some());

    // Detaching what each message names leaves it in place, and the retries
    // remove the rest and then the application.
    detach_as_root(&f, DB).await.unwrap();
    for _ in 0..5 {
        if f.app_state().await.is_none() {
            break;
        }
        for (_, teardown) in f.accept_app_teardown().await.unwrap() {
            let outcomes = deleting(&f, &fake, &checks).run(teardown).await;
            for (descriptor, outcome) in outcomes {
                if matches!(&outcome, Outcome::Pending(r) if r.contains("detach this resource")) {
                    detach_as_root(&f, &descriptor.to_string()).await.unwrap();
                }
            }
        }
    }
    assert!(!fake.deletions().contains(&DB.to_owned()));
    assert_eq!(f.app_state().await, None);
    let (removed,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM audit_log WHERE action = 'app.delete' AND outcome = 'succeeded'",
    )
    .fetch_one(f.store.pool())
    .await
    .unwrap();
    assert_eq!(removed, 1);
}
