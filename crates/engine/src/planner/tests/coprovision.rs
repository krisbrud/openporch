//! Co-provisioning as the planner runs it: every row of the truth table
//! orders provisioning along its edges, a co-provisioned policy is in place
//! before the workload that needs it, and the order workloads are added in
//! changes nothing.

use super::*;

const TABLE: &str = "dynamodb-table.default#modules.web.externals.orders";
const POLICY: &str = "aws-policy.default#modules.web.externals.orders";

const TABLE_WORKLOAD: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers: {web: {image: nginx}}
resources: {orders: {type: dynamodb-table}}
"#;

/// The definitions, with a table that co-provisions a policy, which
/// references `policy_inputs`.
fn coprovisioning(provision: Value, policy_inputs: Value) -> Vec<ResourceDefinition> {
    let mut defs = definitions();
    defs.push(
        serde_json::from_value(json!({
            "id": "ddb", "type": "dynamodb-table", "driver_type": "openporch/echo",
            "criteria": [{}], "provision": {"aws-policy": provision},
        }))
        .unwrap(),
    );
    defs.push(
        serde_json::from_value(json!({
            "id": "policy", "type": "aws-policy", "driver_type": "openporch/echo",
            "criteria": [{}], "driver_inputs": {"values": policy_inputs},
        }))
        .unwrap(),
    );
    defs
}

/// Where each descriptor started among the jobs.
fn started(fake: &Fake) -> Vec<String> {
    let jobs = fake.jobs.lock().unwrap();
    jobs.iter().map(|j| j.descriptor.to_string()).collect()
}

#[tokio::test]
async fn every_row_of_the_truth_table_orders_provisioning() {
    // X -> Y: X starts only after Y provisioned.
    for (is_dependent, match_dependents, edges) in [
        (false, false, vec![(POLICY, TABLE), (WORKLOAD, TABLE)]),
        (
            false,
            true,
            vec![(POLICY, TABLE), (WORKLOAD, TABLE), (WORKLOAD, POLICY)],
        ),
        (true, false, vec![(TABLE, POLICY), (WORKLOAD, TABLE)]),
        (
            true,
            true,
            vec![(TABLE, POLICY), (WORKLOAD, TABLE), (WORKLOAD, POLICY)],
        ),
    ] {
        let case = format!("is_dependent={is_dependent} match_dependents={match_dependents}");
        let f = Fixture::new().await;
        let fake = Fake::with(&[]);
        let defs = coprovisioning(
            json!({"is_dependent": is_dependent, "match_dependents": match_dependents}),
            json!({}),
        );
        let deployment = f.accept_set(Some(TABLE_WORKLOAD), defs).await.unwrap();
        let outcomes = planner(&f, &fake, fast()).run(deployment).await;
        assert!(
            outcomes.values().all(|o| *o == Outcome::Provisioned),
            "{case}: {outcomes:?}"
        );
        let order = started(&fake);
        let at = |d: &str| order.iter().position(|x| x == d).unwrap();
        for (later, earlier) in &edges {
            assert!(
                at(later) > at(earlier),
                "{case}: {later} -> {earlier} in {order:?}"
            );
            // The edge is recorded with the resource, so deletion follows it
            // in reverse.
            let recorded = f.active(later).await.unwrap().dependencies;
            assert!(
                recorded.contains(&(*earlier).to_owned()),
                "{case}: {recorded:?}"
            );
        }
        // Without `match_dependents` the workload does not wait for the
        // policy.
        let workload = f.active(WORKLOAD).await.unwrap().dependencies;
        assert_eq!(
            workload.contains(&POLICY.to_owned()),
            match_dependents,
            "{case}: {workload:?}"
        );
        // The workload sees the policy's outputs exactly when it waits for it.
        let dependencies = &fake.job(WORKLOAD).inputs.resource;
        let rendered = serde_json::to_string(dependencies).unwrap();
        assert_eq!(
            rendered.contains(POLICY),
            match_dependents,
            "{case}: {rendered}"
        );
        // Nothing provisioned twice.
        assert_eq!(fake.attempts(TABLE), 1, "{case}");
        assert_eq!(fake.attempts(POLICY), 1, "{case}");
    }
}

#[tokio::test]
async fn a_policy_is_in_place_before_the_workload_that_needs_it() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    // The policy is provisioned after its table and grants access to it;
    // the workload waits for both.
    let defs = coprovisioning(
        json!({"is_dependent": false, "match_dependents": true}),
        json!({"table": "${resources.dynamodb-table.outputs.host}"}),
    );
    let deployment = f.accept_set(Some(TABLE_WORKLOAD), defs).await.unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
    let order = started(&fake);
    let at = |d: &str| order.iter().position(|x| x == d).unwrap();
    assert!(
        at(TABLE) < at(POLICY) && at(POLICY) < at(WORKLOAD),
        "{order:?}"
    );
    // The policy got the table's output, and the workload got both
    // resources' outputs.
    assert_eq!(
        fake.job(POLICY).inputs.values,
        json!({"table": "dynamodb-table.internal"})
    );
    let workload = fake.job(WORKLOAD);
    let Resolved::Value(inputs) = &workload.inputs.resource else {
        panic!("{:?}", workload.inputs.resource);
    };
    // The environment's base, which it also waits for, is not among them.
    assert_eq!(
        inputs["dependencies"],
        json!({
            "orders": {"host": "dynamodb-table.internal"},
            POLICY: {"host": "aws-policy.internal"},
        })
    );

    // A policy that fails leaves the workload waiting, never running without
    // its access.
    let f = Fixture::new().await;
    let fake = Fake::with(&[("aws-policy", Behavior::Permanent)]);
    let defs = coprovisioning(
        json!({"is_dependent": false, "match_dependents": true}),
        json!({}),
    );
    let deployment = f.accept_set(Some(TABLE_WORKLOAD), defs).await.unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert_eq!(outcomes[&d(TABLE)], Outcome::Provisioned);
    assert!(matches!(outcomes[&d(POLICY)], Outcome::Failed(_)));
    assert_eq!(
        outcomes[&d(WORKLOAD)],
        Outcome::Pending(format!("waiting for `{POLICY}`"))
    );
    assert_eq!(fake.attempts(WORKLOAD), 0);
}

#[tokio::test]
async fn the_order_workloads_are_added_in_changes_nothing() {
    let shared = |name: &str| {
        format!(
            r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {{name: {name}}}
containers: {{web: {{image: nginx}}}}
resources: {{orders: {{type: dynamodb-table, id: orders, params: {{hash_key: id}}}}}}
"#
        )
    };
    let (alpha, beta) = (shared("alpha"), shared("beta"));
    let mut runs = Vec::new();
    // Workloads and definitions in either order, provisioned in parallel or
    // one at a time, so resources finish in different orders.
    for (reversed, concurrency) in [(false, 4), (true, 1)] {
        let f = Fixture::new().await;
        let fake = Fake::with(&[]);
        let mut defs = coprovisioning(json!({"match_dependents": true}), json!({}));
        let mut workloads = [alpha.as_str(), beta.as_str()];
        if reversed {
            defs.reverse();
            workloads.reverse();
        }
        let deployment = f.accept_workloads(&workloads, defs).await.unwrap();
        let graph = deployment.graph.clone();
        let limits = Limits {
            concurrency,
            ..fast()
        };
        let outcomes = planner(&f, &fake, limits).run(deployment).await;
        assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
        // The shared table and its policy are one resource each.
        assert_eq!(fake.attempts("dynamodb-table.default#shared.orders"), 1);
        assert_eq!(fake.attempts("aws-policy.default#shared.orders"), 1);
        let recorded: BTreeMap<String, (Vec<String>, Value)> = f
            .store
            .active_resources(ORG, "shop", "development")
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.descriptor, (r.dependencies, r.inputs)))
            .collect();
        runs.push((graph, recorded));
    }
    assert_eq!(runs[0].0, runs[1].0);
    assert_eq!(runs[0].1, runs[1].1);
    for workload in [
        "workload.default#modules.alpha",
        "workload.default#modules.beta",
    ] {
        let (deps, _) = &runs[0].1[workload];
        assert!(
            deps.contains(&"aws-policy.default#shared.orders".to_owned()),
            "{deps:?}"
        );
    }
}

#[tokio::test]
async fn removing_one_workload_keeps_what_the_other_still_uses() {
    let shared = |name: &str| {
        format!(
            r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {{name: {name}}}
containers: {{web: {{image: nginx}}}}
resources: {{orders: {{type: dynamodb-table, id: orders}}}}
"#
        )
    };
    let (alpha, beta) = (shared("alpha"), shared("beta"));
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    let defs = || coprovisioning(json!({"match_dependents": true}), json!({}));
    let deployment = f.accept_workloads(&[&alpha, &beta], defs()).await.unwrap();
    deleting(&f, &fake, &checks).run(deployment).await;
    let table = f
        .active("dynamodb-table.default#shared.orders")
        .await
        .unwrap();
    let policy = f.active("aws-policy.default#shared.orders").await.unwrap();

    // Removing alpha deletes only its workload; the shared table and policy
    // stay tracked, with their identities, for beta.
    let deployment = f.accept_workloads(&[&beta], defs()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert_eq!(
        outcomes[&d("workload.default#modules.alpha")],
        Outcome::Deleted
    );
    assert_eq!(fake.deletions(), ["workload.default#modules.alpha"]);
    assert_eq!(
        f.active("dynamodb-table.default#shared.orders")
            .await
            .unwrap()
            .guresid,
        table.guresid
    );
    assert_eq!(
        f.active("aws-policy.default#shared.orders")
            .await
            .unwrap()
            .guresid,
        policy.guresid
    );
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );

    // Removing beta too leaves them tracked, since nothing scheduled them.
    // So does a removed workload's own table and its policy.
    let deployment = f.accept_workloads(&[TABLE_WORKLOAD], defs()).await.unwrap();
    deleting(&f, &fake, &checks).run(deployment).await;
    assert!(f.active(TABLE).await.is_some());
    let deployment = f.accept_workloads(&[], defs()).await.unwrap();
    deleting(&f, &fake, &checks).run(deployment).await;
    assert!(f.active("workload.default#modules.beta").await.is_none());
    assert!(f.active(WORKLOAD).await.is_none());
    assert!(f.active(TABLE).await.is_some());
    assert!(f.active(POLICY).await.is_some());
    assert!(
        f.active("dynamodb-table.default#shared.orders")
            .await
            .is_some()
    );
    assert!(f.active("aws-policy.default#shared.orders").await.is_some());
}
