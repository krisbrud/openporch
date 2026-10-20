//! Crash injection: a deployment stopped before a driver runs, after the cloud
//! object exists, before the result is recorded, and in the middle of a
//! deletion. Each keeps its GUResIDs and state, creates no duplicate objects,
//! keeps what other branches provisioned, and leaves an outcome nobody knows
//! for observation or an administrator to resolve.

use std::future::Future;

use openporch_store::{RecoveryReport, Resolution};

use super::*;

const RECOVER: &str = "/orgs/acme/apps/shop/envs/development/resources/recover";

/// Runs `deployment` until `ready` holds, then stops it the way a crash does:
/// the run is dropped mid-flight, and the store recovers as after a restart.
async fn crash<F, Fut>(
    f: &Fixture,
    planner: &Arc<Planner<SqliteStore, Fake>>,
    deployment: Deployment,
    ready: F,
) -> RecoveryReport
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let run = tokio::spawn({
        let planner = planner.clone();
        async move { planner.run(deployment).await }
    });
    let waited = tokio::time::timeout(Duration::from_secs(30), async {
        while !ready().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the crash point was never reached");
    assert!(
        !run.is_finished(),
        "the deployment finished before the crash"
    );
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    f.store.recover_interrupted().await.unwrap()
}

impl Fixture {
    async fn guresid(&self, descriptor: &str) -> String {
        self.active(descriptor).await.unwrap().guresid
    }

    async fn provisioned(&self, descriptors: &[&str]) -> bool {
        for descriptor in descriptors {
            match self.active(descriptor).await {
                Some(r) if r.status == "provisioned" => {}
                _ => return false,
            }
        }
        true
    }

    async fn recover(
        &self,
        planner: &Planner<SqliteStore, Fake>,
        descriptor: &str,
    ) -> Result<Recovered, RecoverError> {
        planner
            .recover(ORG, "shop", self.env, descriptor, audit("POST", RECOVER))
            .await
    }
}

/// Every descriptor has exactly one object, and the object of each provisioned
/// resource is the one its GUResID names.
async fn no_duplicates(f: &Fixture, fake: &Fake) {
    for r in f.store.active_resources(ORG, "shop", f.env).await.unwrap() {
        assert!(fake.creations(&r.descriptor) <= 1, "{}", r.descriptor);
        if r.status == "provisioned" {
            let objects = fake.objects.lock().unwrap();
            assert_eq!(
                objects.get(&r.guresid),
                Some(&r.descriptor),
                "{}",
                r.descriptor
            );
        }
    }
}

#[tokio::test]
async fn a_crash_before_the_driver_runs_is_repeated_with_the_same_identity() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let deployment = f.accept().await;
    // The operation is recorded, but the crash comes before its inputs are,
    // which happens right before the driver runs.
    let started = f
        .store
        .begin_operation(OperationStart {
            org: ORG.into(),
            app: "shop".into(),
            env: "development".into(),
            deployment_id: deployment.id.clone(),
            descriptor: d(DB),
            definition_id: Some("pg".into()),
            definition_version: Some(1),
            driver_type: Some("openporch/echo".into()),
            kind: OperationKind::Put,
            input_fingerprint: "inputs".into(),
            recovery: json!({"definition_id": "pg"}),
        })
        .await
        .unwrap();

    let report = f.store.recover_interrupted().await.unwrap();
    assert!(report.uncertain.is_empty(), "{report:?}");
    assert_eq!(
        report.failed_deployments,
        std::slice::from_ref(&deployment.id)
    );
    let db = f.active(DB).await.unwrap();
    assert_eq!(db.status, "failed");
    assert_eq!(db.guresid, started.guresid.as_str());

    // Nothing blocks the next deployment, which provisions it once, under the
    // identity allocated before the crash.
    let deployment = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    assert_eq!(f.guresid(DB).await, started.guresid.as_str());
    assert_eq!(fake.creations(DB), 1);
    assert!(fake.observed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_crash_after_creation_is_recovered_by_observation() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("postgres", Behavior::Crash)]);
    let planner = Arc::new(planner(&f, &fake, fast()));
    let report = crash(&f, &planner, f.accept().await, || async {
        fake.objects.lock().unwrap().values().any(|d| d == DB)
            && f.provisioned(&[BASE, DNS, ROUTE]).await
    })
    .await;
    let db = f.guresid(DB).await;
    assert_eq!(report.uncertain.len(), 1, "{report:?}");
    assert_eq!(report.uncertain[0].descriptor, DB);
    assert_eq!(report.uncertain[0].guresid, db);
    assert_eq!(f.active(DB).await.unwrap().status, "unknown");
    // The branches that finished keep their results.
    let before: BTreeMap<String, String> = f
        .store
        .active_resources(ORG, "shop", "development")
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.descriptor, r.guresid))
        .collect();
    assert!(f.provisioned(&[BASE, DNS, ROUTE]).await);
    assert!(!f.provisioned(&[WORKLOAD]).await);

    // Nothing changes the database until its outcome is known.
    assert!(matches!(
        f.try_accept().await,
        Err(StoreError::RecoveryRequired(e)) if e.contains(DB)
    ));
    assert_eq!(
        f.recover(&planner, DB).await.unwrap(),
        Recovered::Provisioned
    );
    let recovered = f.active(DB).await.unwrap();
    assert_eq!(recovered.status, "provisioned");
    assert_eq!(recovered.guresid, db);
    assert_eq!(recovered.outputs["host"], "postgres.internal");
    assert!(recovered.cookie.is_some());
    assert_eq!(recovered.snapshot["definition"]["id"], "pg");
    // A second recovery has nothing left to do.
    assert!(matches!(
        f.recover(&planner, DB).await,
        Err(RecoverError::NothingToRecover(_))
    ));

    // The next deployment converges without creating anything twice.
    fake.behavior.lock().unwrap().clear();
    let deployment = f.accept().await;
    let outcomes = planner.run(deployment.clone()).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
    for (descriptor, guresid) in &before {
        assert_eq!(&f.guresid(descriptor).await, guresid, "{descriptor}");
    }
    no_duplicates(&f, &fake).await;
    for descriptor in [BASE, DB, DNS, ROUTE, WORKLOAD] {
        assert_eq!(fake.creations(descriptor), 1, "{descriptor}");
    }
}

#[tokio::test]
async fn a_recovered_creation_is_checked_against_the_type_its_deployment_recorded() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("postgres", Behavior::Crash)]);
    let planner = Arc::new(planner(&f, &fake, fast()));
    let deployment = f.accept().await;
    let id = deployment.id.clone();
    crash(&f, &planner, deployment, || async {
        fake.objects.lock().unwrap().values().any(|d| d == DB)
    })
    .await;
    // The deployment recorded a postgres type of its own, as one with a custom
    // type does: recovery checks the outputs against it, not the built-in.
    let mut recorded = builtin_type(&"postgres".parse().unwrap()).unwrap();
    recorded.name = "Recorded postgres".into();
    sqlx::query(
        "UPDATE deployments SET snapshot = json_set(snapshot, '$.types', json(?)) WHERE id = ?",
    )
    .bind(json!({"postgres": recorded}).to_string())
    .bind(&id)
    .execute(f.store.pool())
    .await
    .unwrap();
    assert_eq!(
        f.recover(&planner, DB).await.unwrap(),
        Recovered::Provisioned
    );
    assert_eq!(*fake.observed_schemas.lock().unwrap(), vec![Some(recorded)]);
}

#[tokio::test]
async fn a_crash_before_the_result_is_recorded_is_recovered_by_observation() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    // The database provisions, but recording that fails, as when the server
    // stops between the driver's answer and the checkpoint.
    sqlx::query(&format!(
        "CREATE TRIGGER crash BEFORE UPDATE OF status ON active_resources \
         WHEN NEW.status = 'provisioned' AND NEW.descriptor = '{DB}' \
         BEGIN SELECT RAISE(ABORT, 'injected crash'); END"
    ))
    .execute(f.store.pool())
    .await
    .unwrap();
    let planner = planner(&f, &fake, fast());
    let deployment = f.accept().await;
    let outcomes = planner.run(deployment.clone()).await;
    assert!(
        matches!(&outcomes[&d(DB)], Outcome::Unknown(e) if e.contains("could not be recorded")),
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes[&d(WORKLOAD)],
        Outcome::Pending(format!("waiting for `{DB}`"))
    );
    for descriptor in [BASE, DNS, ROUTE] {
        assert_eq!(outcomes[&d(descriptor)], Outcome::Provisioned);
    }
    assert_eq!(f.active(DB).await.unwrap().status, "unknown");
    assert_eq!(fake.creations(DB), 1);
    sqlx::query("DROP TRIGGER crash")
        .execute(f.store.pool())
        .await
        .unwrap();

    let db = f.guresid(DB).await;
    // An observation that panics establishes nothing.
    fake.observe_panics
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        f.recover(&planner, DB).await,
        Err(RecoverError::Observation(e)) if e.contains("stopped unexpectedly")
    ),);
    assert_eq!(f.active(DB).await.unwrap().status, "unknown");
    fake.observe_panics
        .store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        f.recover(&planner, DB).await.unwrap(),
        Recovered::Provisioned
    );
    let deployment = f.accept().await;
    let outcomes = planner.run(deployment).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    assert_eq!(f.guresid(DB).await, db);
    no_duplicates(&f, &fake).await;
    assert_eq!(fake.creations(DB), 1);
}

#[tokio::test]
async fn nothing_found_is_provisioned_again_with_the_same_identity() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("postgres", Behavior::Crash)]);
    let planner = Arc::new(planner(&f, &fake, fast()));
    crash(&f, &planner, f.accept().await, || async {
        fake.objects.lock().unwrap().values().any(|d| d == DB)
            && f.provisioned(&[BASE, DNS, ROUTE]).await
    })
    .await;
    let db = f.guresid(DB).await;
    // The creation never took hold.
    fake.objects.lock().unwrap().remove(&db);
    let recovered = f.recover(&planner, DB).await.unwrap();
    assert!(
        matches!(&recovered, Recovered::Repeatable(r) if r.contains("nothing exists")),
        "{recovered:?}"
    );
    assert_eq!(f.active(DB).await.unwrap().status, "failed");

    fake.behavior.lock().unwrap().clear();
    let outcomes = planner.run(f.accept().await).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    assert_eq!(f.guresid(DB).await, db);
    assert_eq!(fake.job(DB).guresid.as_str(), db);
}

#[tokio::test]
async fn an_operation_still_running_is_left_unknown() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("postgres", Behavior::Crash)]);
    let planner = Arc::new(planner(&f, &fake, fast()));
    crash(&f, &planner, f.accept().await, || async {
        fake.objects.lock().unwrap().values().any(|d| d == DB)
            && f.provisioned(&[BASE, DNS, ROUTE]).await
    })
    .await;
    fake.observations
        .lock()
        .unwrap()
        .insert("postgres".into(), Observed::InProgress);
    assert_eq!(
        f.recover(&planner, DB).await.unwrap(),
        Recovered::InProgress
    );
    assert_eq!(f.active(DB).await.unwrap().status, "unknown");
    assert!(matches!(
        f.try_accept().await,
        Err(StoreError::RecoveryRequired(_))
    ));
    // Once it settles, observing again finds the result.
    fake.observations.lock().unwrap().clear();
    assert_eq!(
        f.recover(&planner, DB).await.unwrap(),
        Recovered::Provisioned
    );
}

#[tokio::test]
async fn an_unobservable_outcome_waits_for_an_administrator() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("postgres", Behavior::Crash)]);
    let planner = Arc::new(planner(&f, &fake, fast()));
    crash(&f, &planner, f.accept().await, || async {
        fake.objects.lock().unwrap().values().any(|d| d == DB)
            && f.provisioned(&[BASE, DNS, ROUTE]).await
    })
    .await;
    let db = f.guresid(DB).await;
    fake.observations.lock().unwrap().insert(
        "postgres".into(),
        Observed::Unknown("the driver cannot observe databases".into()),
    );
    let recovered = f.recover(&planner, DB).await.unwrap();
    assert!(
        matches!(&recovered, Recovered::RecoveryRequired(r) if r.contains("cannot observe")),
        "{recovered:?}"
    );
    assert_eq!(f.active(DB).await.unwrap().status, "recovery_required");
    // No deployment touches it, and it is never mutated automatically.
    assert!(matches!(
        f.try_accept().await,
        Err(StoreError::RecoveryRequired(_))
    ));
    assert_eq!(fake.attempts(DB), 1);

    // Only an administrator can say that repeating it is safe.
    let op = f
        .store
        .unknown_operation(ORG, "shop", "development", DB)
        .await
        .unwrap()
        .unwrap();
    assert!(op.error.unwrap().contains("recovery required"));
    let resolve = |actor: &str| AuditIntent {
        actor: Some(actor.into()),
        ..audit("POST", RECOVER)
    };
    let refused = f
        .store
        .resolve_operation(
            &op.operation_id,
            Resolution::Operator {
                reason: "checked the console".into(),
            },
            resolve("ci"),
        )
        .await;
    assert!(
        matches!(refused, Err(StoreError::Forbidden(_))),
        "{refused:?}"
    );
    f.store
        .resolve_operation(
            &op.operation_id,
            Resolution::Operator {
                reason: "checked the console".into(),
            },
            resolve("root"),
        )
        .await
        .unwrap();
    assert_eq!(f.active(DB).await.unwrap().status, "failed");
    let entries = f.store.audit_entries(ORG, 5).await.unwrap();
    assert!(
        entries.iter().any(
            |e| e.operation_id.as_deref() == Some(op.operation_id.as_str())
                && e.actor_id.as_deref() == Some("root")
        ),
        "{entries:?}"
    );

    // The next deployment repeats the creation with the same identity, which
    // converges on the object that exists.
    fake.behavior.lock().unwrap().clear();
    let outcomes = planner.run(f.accept().await).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    assert_eq!(f.guresid(DB).await, db);
    assert_eq!(fake.creations(DB), 1);
    no_duplicates(&f, &fake).await;
}

#[tokio::test]
async fn a_known_outcome_is_not_resolved_again() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let planner = planner(&f, &fake, fast());
    planner.run(f.accept().await).await;
    assert!(matches!(
        f.recover(&planner, DB).await,
        Err(RecoverError::NothingToRecover(_))
    ));
    assert!(
        f.store
            .unknown_operation(ORG, "shop", "development", DB)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_crash_during_deletion_keeps_the_rest_tracked_and_deletes_nothing_twice() {
    let f = Fixture::in_env("pr-7").await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    let planner = Arc::new(deleting(&f, &fake, &checks));
    planner.run(f.accept().await).await;
    let db = f.guresid(DB).await;
    // The database runs in the environment's base.
    sqlx::query("UPDATE active_resources SET dependencies = ? WHERE descriptor = ?")
        .bind(json!([BASE]).to_string())
        .bind(DB)
        .execute(f.store.pool())
        .await
        .unwrap();
    let base = f.guresid(BASE).await;

    // The database's deletion takes effect, then the server stops.
    fake.deletions
        .lock()
        .unwrap()
        .insert("postgres".into(), Behavior::Crash);
    let teardown = f.accept_teardown().await.unwrap();
    let report = crash(&f, &planner, teardown, || async {
        !fake.objects.lock().unwrap().contains_key(&db)
            && f.active(WORKLOAD).await.is_none()
            && f.active(ROUTE).await.is_none()
            && f.active(DNS).await.is_none()
    })
    .await;
    assert_eq!(report.uncertain.len(), 1, "{report:?}");
    assert_eq!(f.env_state().await.as_deref(), Some("deletion_failed"));
    // The database and what it needs stay tracked.
    assert_eq!(f.active(DB).await.unwrap().status, "unknown");
    assert_eq!(f.active(BASE).await.unwrap().guresid, base);
    assert!(fake.objects.lock().unwrap().contains_key(&base));
    assert!(matches!(
        f.accept_teardown().await,
        Err(StoreError::RecoveryRequired(_))
    ));

    // Observing finds it gone, which completes its deletion.
    assert_eq!(f.recover(&planner, DB).await.unwrap(), Recovered::Deleted);
    assert!(f.active(DB).await.is_none());
    assert!(
        f.tombstones()
            .await
            .contains(&(DB.to_owned(), db.clone(), "deleted".to_owned()))
    );

    // The retry deletes the rest, without deleting the database again.
    fake.deletions.lock().unwrap().clear();
    let outcomes = planner.run(f.accept_teardown().await.unwrap()).await;
    assert_eq!(outcomes[&d(BASE)], Outcome::Deleted, "{outcomes:?}");
    assert!(!outcomes.contains_key(&d(DB)), "{outcomes:?}");
    let deleted = fake.deletions();
    assert_eq!(
        deleted.iter().filter(|d| *d == DB).count(),
        1,
        "{deleted:?}"
    );
    assert_eq!(f.env_state().await, None);
    assert!(fake.objects.lock().unwrap().is_empty());
}

#[tokio::test]
async fn nothing_found_by_a_driver_that_cannot_recover_waits_for_an_administrator() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("postgres", Behavior::Crash)]);
    let planner = Arc::new(planner(&f, &fake, fast()));
    crash(&f, &planner, f.accept().await, || async {
        fake.objects.lock().unwrap().values().any(|d| d == DB)
            && f.provisioned(&[BASE, DNS, ROUTE]).await
    })
    .await;
    // The creation may still be on its way, and repeating it could
    // duplicate it.
    fake.observations
        .lock()
        .unwrap()
        .insert("postgres".into(), Observed::Absent { repeatable: false });
    let recovered = f.recover(&planner, DB).await.unwrap();
    assert!(
        matches!(&recovered, Recovered::RecoveryRequired(r) if r.contains("duplicated")),
        "{recovered:?}"
    );
    assert_eq!(f.active(DB).await.unwrap().status, "recovery_required");
}

#[tokio::test]
async fn an_interrupted_update_is_repeated_rather_than_recorded() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let planner = Arc::new(planner(&f, &fake, fast()));
    planner.run(f.accept().await).await;
    let db = f.active(DB).await.unwrap();
    // The update crashes after it reached the driver.
    fake.behavior
        .lock()
        .unwrap()
        .insert("postgres".into(), Behavior::Crash);
    // The other branches finish first, so only the database is left with an
    // unknown outcome.
    let update = f.accept().await;
    let id = update.id.clone();
    crash(&f, &planner, update, || async {
        if fake.attempts(DB) != 2 {
            return false;
        }
        for descriptor in [BASE, DNS, ROUTE] {
            match f.active(descriptor).await {
                Some(r) if r.status == "provisioned" && r.last_deploy_id.as_ref() == Some(&id) => {}
                _ => return false,
            }
        }
        true
    })
    .await;
    // The resource exists, which says nothing about whether the update
    // applied, so its last success stays recorded and the update repeats.
    let recovered = f.recover(&planner, DB).await.unwrap();
    assert!(
        matches!(&recovered, Recovered::Repeatable(r) if r.contains("update")),
        "{recovered:?}"
    );
    let after = f.active(DB).await.unwrap();
    assert_eq!(after.status, "failed");
    assert_eq!(after.last_deploy_id, db.last_deploy_id);
    fake.behavior.lock().unwrap().clear();
    let outcomes = planner.run(f.accept().await).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    assert_eq!(f.guresid(DB).await, db.guresid);
    assert_eq!(fake.creations(DB), 1);
}
