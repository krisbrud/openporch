//! The version of the code each driver ran is recorded in the deployment's
//! snapshot as the resource provisions, and a replay of that snapshot hands
//! it back to the driver.

use super::*;

#[tokio::test]
async fn a_replay_runs_the_versions_its_deployment_recorded() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let first = f.accept().await;
    planner(&f, &fake, fast()).run(first.clone()).await;
    // Nothing was pinned, so every driver resolved its own version.
    assert!(fake.jobs.lock().unwrap().iter().all(|j| j.pin.is_none()));

    let recorded = f
        .store
        .deployment_snapshot(ORG, "shop", f.env, &first.id)
        .await
        .unwrap()
        .unwrap();
    let pins = recorded["pins"].as_object().unwrap();
    for descriptor in [BASE, DB, DNS, ROUTE, WORKLOAD] {
        let ty = descriptor.split('.').next().unwrap();
        assert_eq!(
            pins[descriptor],
            json!({"commit": format!("{ty}-commit")}),
            "{pins:?}"
        );
    }
    // What the snapshot recorded at acceptance is unchanged.
    assert_eq!(recorded["permits"], json!(["permit-1"]));
    assert!(recorded["graph"].is_object());

    // A replay of the recorded snapshot runs each resource at its pin.
    let next = f.accept().await;
    let replay = Deployment::from_snapshot(
        next.org.clone(),
        next.app.clone(),
        next.env.clone(),
        next.env_type.clone(),
        next.id.clone(),
        next.set_id.clone(),
        &recorded,
    )
    .unwrap();
    let fake = Fake::with(&[]);
    planner(&f, &fake, fast()).run(replay).await;
    for descriptor in [BASE, DB, DNS, ROUTE, WORKLOAD] {
        assert_eq!(fake.job(descriptor).pin.as_ref(), Some(&pins[descriptor]));
    }
}

#[tokio::test]
async fn a_replay_whose_pinned_version_is_gone_changes_nothing() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let first = f.accept().await;
    planner(&f, &fake, fast()).run(first.clone()).await;
    // A deployment that replays nothing checks no pins.
    assert!(fake.pin_checks.lock().unwrap().is_empty());
    let recorded = f
        .store
        .deployment_snapshot(ORG, "shop", f.env, &first.id)
        .await
        .unwrap()
        .unwrap();

    // The database module's commit can no longer be fetched.
    let next = f.accept().await;
    let replay = Deployment::from_snapshot(
        next.org.clone(),
        next.app.clone(),
        next.env.clone(),
        next.env_type.clone(),
        next.id.clone(),
        next.set_id.clone(),
        &recorded,
    )
    .unwrap();
    let fake = Fake::with(&[]);
    fake.lost_commits
        .lock()
        .unwrap()
        .insert("postgres-commit".into());
    let outcomes = planner(&f, &fake, fast()).run(replay).await;

    // Every pin was checked with its definition's driver inputs, the DNS's
    // outputs the route reads still unknown; nothing ran.
    let checked: BTreeSet<String> = fake
        .pin_checks
        .lock()
        .unwrap()
        .iter()
        .map(|j| j.descriptor.to_string())
        .collect();
    assert_eq!(
        checked,
        [BASE, DB, DNS, ROUTE, WORKLOAD].map(String::from).into()
    );
    let db = fake
        .pin_checks
        .lock()
        .unwrap()
        .iter()
        .find(|j| j.descriptor.to_string() == DB)
        .cloned()
        .unwrap();
    assert_eq!(db.guresid.as_str(), f.active(DB).await.unwrap().guresid);
    assert_eq!(db.inputs.values["id"], db.guresid.as_str());
    let route = fake
        .pin_checks
        .lock()
        .unwrap()
        .iter()
        .find(|j| j.descriptor.to_string() == ROUTE)
        .cloned()
        .unwrap();
    assert_eq!(
        route.inputs.values["host"],
        "(not known before the deployment runs)"
    );
    // Modules may be fetched with the cloud account, unless it comes from
    // another resource.
    let account = |d: &str| {
        let checks = fake.pin_checks.lock().unwrap();
        let job = checks
            .iter()
            .find(|j| j.descriptor.to_string() == d)
            .unwrap();
        job.driver_account.as_ref().map(ToString::to_string)
    };
    assert_eq!(account(DNS).as_deref(), Some("aws-dns"));
    assert_eq!(account(ROUTE), None);
    assert!(fake.jobs.lock().unwrap().is_empty());
    let Outcome::Failed(reason) = &outcomes[&Descriptor::parse(DB).unwrap()] else {
        panic!("{outcomes:?}");
    };
    assert!(
        reason.contains("`postgres-commit` can no longer be fetched"),
        "{reason}"
    );
    let Outcome::Pending(reason) = &outcomes[&Descriptor::parse(WORKLOAD).unwrap()] else {
        panic!("{outcomes:?}");
    };
    assert!(reason.contains("a version of the code"), "{reason}");
}

#[tokio::test]
async fn a_pin_that_cannot_be_checked_before_the_run_is_refused_saying_why() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let first = f.accept().await;
    planner(&f, &fake, fast()).run(first.clone()).await;
    let recorded = f
        .store
        .deployment_snapshot(ORG, "shop", f.env, &first.id)
        .await
        .unwrap()
        .unwrap();
    let next = f.accept().await;
    let replay = Deployment::from_snapshot(
        next.org.clone(),
        next.app.clone(),
        next.env.clone(),
        next.env_type.clone(),
        next.id.clone(),
        next.set_id.clone(),
        &recorded,
    )
    .unwrap();
    // The route's driver inputs read the DNS's outputs, and its check fails.
    let fake = Fake::with(&[]);
    fake.lost_commits
        .lock()
        .unwrap()
        .insert("route-commit".into());
    let outcomes = planner(&f, &fake, fast()).run(replay).await;
    assert!(fake.jobs.lock().unwrap().is_empty());
    let Outcome::Failed(reason) = &outcomes[&Descriptor::parse(ROUTE).unwrap()] else {
        panic!("{outcomes:?}");
    };
    assert!(
        reason.contains("not known before the deployment runs"),
        "{reason}"
    );
}
