//! Previews of changes, removals and migrations, without drivers.

use std::collections::BTreeSet;

use openporch_core::graph::{Node, ResourceGraph};
use openporch_core::ids::Descriptor;
use openporch_core::model::ResourceDefinition;
use serde_json::{Value, json};

use super::*;

const DB: &str = "postgres.default#modules.web.externals.db";
const QUEUE: &str = "sqs.default#modules.web.externals.queue";
const WORKLOAD: &str = "workload.default#modules.web";
const OLD: &str = "workload.default#modules.old";
const BUCKET: &str = "s3.default#shared.bucket";

fn def(id: &str, driver: &str, account: Option<&str>) -> ResourceDefinition {
    serde_json::from_value(json!({
        "id": id, "type": "postgres", "driver_type": driver,
        "driver_account": account, "criteria": [{}],
    }))
    .unwrap()
}

fn node(descriptor: &str, definition: &str, driver: &str) -> Node {
    Node {
        descriptor: Descriptor::parse(descriptor).unwrap(),
        definition: definition.parse().unwrap(),
        driver_type: driver.into(),
        params: None,
        requests: Vec::new(),
        depends_on: BTreeSet::new(),
    }
}

/// A deployment of `nodes`, each with its definition.
fn deployment(nodes: &[(Node, ResourceDefinition)]) -> Deployment {
    Deployment {
        org: "acme".parse().unwrap(),
        app: "shop".parse().unwrap(),
        env: "development".parse().unwrap(),
        env_type: "development".parse().unwrap(),
        id: "d1".into(),
        set_id: "s1".into(),
        graph: ResourceGraph {
            nodes: nodes
                .iter()
                .map(|(n, _)| (n.descriptor.clone(), n.clone()))
                .collect(),
            layers: Vec::new(),
        },
        definitions: nodes
            .iter()
            .map(|(_, d)| (d.id.clone(), (1, d.clone())))
            .collect(),
        values: BTreeMap::new(),
        permits: Vec::new(),
        teardown: false,
    }
}

/// A provisioned resource.
fn active(descriptor: &str, driver: &str) -> ActiveResource {
    ActiveResource {
        app: "shop".into(),
        env: "development".into(),
        descriptor: descriptor.into(),
        guresid: format!("{:0>32}", descriptor.len()),
        definition_id: Some("pg".into()),
        definition_version: Some(1),
        driver_type: Some(driver.into()),
        driver_account: None,
        target: None,
        backend: None,
        status: "provisioned".into(),
        inputs: Value::Null,
        outputs: json!({}),
        secret_outputs: BTreeMap::new(),
        cookie: None,
        snapshot: Value::Null,
        recovery: Value::Null,
        ownership: "owned".into(),
        dependencies: Vec::new(),
        last_deploy_id: Some("d0".into()),
        scheduled_deletion: false,
        cleanup_pending: false,
        observed_at: None,
        created_at: 0,
    }
}

fn facts(driver: &str) -> Option<DriverFacts> {
    match driver {
        "openporch/opentofu" => Some(DriverFacts {
            preview: true,
            backend: Some("s3://state/openporch".into()),
        }),
        "test/blind" => Some(DriverFacts::default()),
        _ => Some(DriverFacts {
            preview: true,
            backend: None,
        }),
    }
}

fn by_descriptor(entries: &[Entry]) -> BTreeMap<&str, &Entry> {
    entries.iter().map(|e| (e.descriptor.as_str(), e)).collect()
}

#[test]
fn new_previewed_and_blind_changes() {
    let d = deployment(&[
        (
            node(DB, "pg", "openporch/opentofu"),
            def("pg", "openporch/opentofu", None),
        ),
        (
            node(QUEUE, "qu", "test/blind"),
            def("qu", "test/blind", None),
        ),
        (
            node(WORKLOAD, "wl", "openporch/none"),
            def("wl", "openporch/none", None),
        ),
    ]);
    let entries = preview(
        &d,
        &[
            active(DB, "openporch/opentofu"),
            active(QUEUE, "test/blind"),
        ],
        facts,
        &BTreeMap::new(),
    )
    .unwrap();
    let e = by_descriptor(&entries);
    assert_eq!(e[WORKLOAD].action, Change::Create);
    assert!(e[WORKLOAD].known && !e[WORKLOAD].permit_required);
    // The driver plans the database's change when the deployment runs.
    assert_eq!(e[DB].action, Change::Update);
    assert!(!e[DB].known && !e[DB].permit_required);
    // A change no driver can preview is explicit about it and needs a permit.
    assert_eq!(e[QUEUE].action, Change::Unknown);
    assert!(!e[QUEUE].known && e[QUEUE].permit_required);
}

#[test]
fn a_known_driver_plan_shows_as_a_replacement() {
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", None),
    )]);
    let db = active(DB, "openporch/opentofu");
    let plan = DriverPlan {
        fingerprint: "fp-1".into(),
        actions: vec![("replace".into(), "aws_db_instance.main".into())],
    };
    let entries = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::from([(db.guresid.clone(), plan.clone())]),
    )
    .unwrap();
    assert_eq!(entries[0].action, Change::Replace);
    assert!(entries[0].permit_required);
    assert_eq!(entries[0].driver_plan, Some(plan));
}

#[test]
fn removals_need_permits() {
    let d = deployment(&[(
        node(WORKLOAD, "wl", "openporch/none"),
        def("wl", "openporch/none", None),
    )]);
    let mut queue = active(QUEUE, "openporch/opentofu");
    queue.dependencies = vec![OLD.into()];
    let mut bucket = active(BUCKET, "openporch/opentofu");
    bucket.scheduled_deletion = true;
    let mut external = active(DB, "openporch/opentofu");
    external.ownership = "external".into();
    external.scheduled_deletion = true;
    let entries = preview(
        &d,
        &[
            active(WORKLOAD, "openporch/none"),
            active(OLD, "openporch/none"),
            queue,
            bucket,
            external,
        ],
        facts,
        &BTreeMap::new(),
    )
    .unwrap();
    let e = by_descriptor(&entries);
    for (descriptor, reason) in [
        (OLD, "the workload was removed"),
        (QUEUE, "it belongs to a removed workload"),
        (BUCKET, "scheduled for deletion"),
    ] {
        assert_eq!(e[descriptor].action, Change::Delete, "{descriptor}");
        assert!(e[descriptor].permit_required, "{descriptor}");
        assert!(e[descriptor].reason.starts_with(reason), "{descriptor}");
    }
    // An external resource is never deleted.
    assert!(!e.contains_key(DB));

    // When the environment goes, everything it owns goes.
    let mut teardown = deployment(&[]);
    teardown.teardown = true;
    let entries = preview(
        &teardown,
        &[active(WORKLOAD, "openporch/none"), active(BUCKET, "x/y")],
        facts,
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.action == Change::Delete
        && e.permit_required
        && e.reason == "the environment is deleted"));
}

#[test]
fn moving_a_resource_is_refused_as_a_migration() {
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", Some("aws-prod")),
    )]);
    let mut db = active(DB, "openporch/template");
    db.driver_account = Some("aws-prod".into());
    let migrations = preview(&d, std::slice::from_ref(&db), facts, &BTreeMap::new()).unwrap_err();
    assert!(
        migrations[0].contains("driver `openporch/template`"),
        "{migrations:?}"
    );

    db.driver_type = Some("openporch/opentofu".into());
    db.driver_account = Some("aws-dev".into());
    let migrations = preview(&d, std::slice::from_ref(&db), facts, &BTreeMap::new()).unwrap_err();
    assert!(
        migrations[0].contains("cloud account `aws-dev`"),
        "{migrations:?}"
    );

    db.driver_account = Some("aws-prod".into());
    db.backend = Some("local state".into());
    let migrations = preview(&d, std::slice::from_ref(&db), facts, &BTreeMap::new()).unwrap_err();
    assert!(
        migrations[0].contains("keeps its state in local state"),
        "{migrations:?}"
    );

    // The same driver, account and backend is an update.
    db.backend = Some("s3://state/openporch".into());
    let entries = preview(&d, std::slice::from_ref(&db), facts, &BTreeMap::new()).unwrap();
    assert_eq!(entries[0].action, Change::Update);

    // An account from a placeholder is checked when the deployment runs.
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", Some("${resources.dns.account}")),
    )]);
    db.driver_account = Some("aws-dev".into());
    assert!(preview(&d, std::slice::from_ref(&db), facts, &BTreeMap::new()).is_ok());
}
