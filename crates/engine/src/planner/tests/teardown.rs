//! Deleting an environment: every owned resource is deleted in reverse
//! dependency order, then the environment; a failure keeps it for a retry.

use super::*;

const ENV: &str = "pr-42";

impl Fixture {
    /// Accepts the deletion of the fixture's environment, as the server does.
    async fn accept_teardown(&self) -> Result<Deployment, StoreError> {
        let env = self.store.env(ORG, "shop", self.env).await?.unwrap();
        let snapshot = json!({
            "version": 1,
            "kind": "delete_env",
            "permits": ["permit-1"],
            "graph": {"nodes": {}, "layers": []},
            "definitions": {},
        });
        let accepted = self
            .store
            .accept_deployment(DeploymentRequest {
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
        let mut deployment = Deployment::from_snapshot(
            ORG.parse().unwrap(),
            "shop".parse().unwrap(),
            self.env.parse().unwrap(),
            "development".parse().unwrap(),
            accepted.deployment().id.clone(),
            String::new(),
            &snapshot,
        )
        .unwrap();
        deployment.teardown = true;
        Ok(deployment)
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
