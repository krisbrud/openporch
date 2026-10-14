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
