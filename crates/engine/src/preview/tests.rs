//! Previews of changes, removals and migrations, without drivers.

use std::collections::BTreeSet;

use openporch_core::graph::{Node, ResourceGraph};
use openporch_core::ids::Descriptor;
use openporch_core::model::ResourceDefinition;
use openporch_store::secrets::Sealed;
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
        pins: BTreeMap::new(),
        types: BTreeMap::new(),
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
        action: "replace".into(),
        fingerprint: "fp-1".into(),
        actions: vec![("replace".into(), "aws_db_instance.main".into())],
        config_digest: None,
    };
    let entries = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::from([(db.guresid.clone(), plan.clone())]),
        &BTreeMap::new(),
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
    let migrations = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(
        migrations[0].contains("driver `openporch/template`"),
        "{migrations:?}"
    );

    db.driver_type = Some("openporch/opentofu".into());
    db.driver_account = Some("aws-dev".into());
    let migrations = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(
        migrations[0].contains("cloud account `aws-dev`"),
        "{migrations:?}"
    );

    db.driver_account = Some("aws-prod".into());
    db.backend = Some("local state".into());
    let migrations = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(
        migrations[0].contains("keeps its state in local state"),
        "{migrations:?}"
    );

    // The same driver, account and backend is an update.
    db.backend = Some("s3://state/openporch".into());
    let entries = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(entries[0].action, Change::Update);

    // An account from a placeholder is checked when the deployment runs.
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", Some("${resources.dns.account}")),
    )]);
    db.driver_account = Some("aws-dev".into());
    assert!(
        preview(
            &d,
            std::slice::from_ref(&db),
            facts,
            &BTreeMap::new(),
            &BTreeMap::new()
        )
        .is_ok()
    );
}

#[test]
fn a_failed_first_attempt_and_a_cookie_only_resource() {
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", None),
    )]);
    // The first attempt failed and left nothing the orchestrator recorded,
    // but its driver may still find state when it plans.
    let mut db = active(DB, "openporch/template");
    db.last_deploy_id = None;
    db.status = "failed".into();
    let entries = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(entries[0].action, Change::Create);
    assert!(!entries[0].known && !entries[0].permit_required);
    assert!(
        entries[0]
            .reason
            .starts_with("an earlier attempt did not finish")
    );

    // A cookie from that attempt makes the resource its driver's.
    db.cookie = Some(openporch_store::secrets::Sealed {
        key_id: "k".into(),
        nonce: Vec::new(),
        ciphertext: Vec::new(),
    });
    let migrations = preview(
        &d,
        std::slice::from_ref(&db),
        facts,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap_err();
    assert!(migrations[0].contains("driver `openporch/template`"));
}

#[test]
fn cleanup_and_drivers_without_facts() {
    let d = deployment(&[(node(QUEUE, "qu", "test/gone"), def("qu", "test/gone", None))]);
    let mut bucket = active(BUCKET, "openporch/opentofu");
    bucket.scheduled_deletion = true;
    bucket.cleanup_pending = true;
    let entries = preview(
        &d,
        &[active(QUEUE, "test/gone"), bucket],
        |driver| (driver != "test/gone").then(DriverFacts::default),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();
    let e = by_descriptor(&entries);
    // A driver nothing is known about is treated as one that cannot preview.
    assert_eq!(e[QUEUE].action, Change::Unknown);
    assert!(e[QUEUE].permit_required);
    // Removing only a record needs no permit.
    assert_eq!(e[BUCKET].action, Change::Delete);
    assert!(!e[BUCKET].permit_required);
    assert!(e[BUCKET].reason.contains("only its record is removed"));
}

#[test]
fn a_digest_follows_the_configuration() {
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", None),
    )]);
    let db = active(DB, "openporch/opentofu");
    let run = |d: &Deployment| {
        preview(
            d,
            std::slice::from_ref(&db),
            facts,
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap()[0]
            .digest
            .clone()
    };
    let first = run(&d);
    assert_eq!(first, run(&d));
    // A new definition version is a different configuration.
    let mut changed = d.clone();
    for (version, _) in changed.definitions.values_mut() {
        *version = 2;
    }
    assert_ne!(first, run(&changed));
    // So is a change to a shared value the resource references, but not to
    // one it does not.
    let value = |v: &str| openporch_core::placeholder::SharedValue::Value(serde_json::json!(v));
    let mut uses = d.clone();
    for n in uses.graph.nodes.values_mut() {
        n.params = Some(serde_json::json!({"host": "${values.DB_HOST}"}));
    }
    uses.values.insert("DB_HOST".into(), value("a"));
    uses.values.insert("OTHER".into(), value("x"));
    let before = run(&uses);
    uses.values.insert("OTHER".into(), value("y"));
    assert_eq!(before, run(&uses));
    uses.values.insert("DB_HOST".into(), value("b"));
    assert_ne!(before, run(&uses));
    // A detach names its own action.
    let detached = detach(&db);
    assert_eq!(detached.action.permit_action(), Some("detach"));
    assert_ne!(detached.digest, first);
}

#[test]
fn refused_driver_plans_keep_their_action() {
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", None),
    )]);
    let db = active(DB, "openporch/opentofu");
    let run = |action: &str, db: &ActiveResource| {
        let plan = DriverPlan {
            action: action.into(),
            fingerprint: if action == "unknown" {
                String::new()
            } else {
                "fp-1".into()
            },
            actions: Vec::new(),
            config_digest: None,
        };
        preview(
            &d,
            std::slice::from_ref(db),
            facts,
            &BTreeMap::from([(db.guresid.clone(), plan)]),
            &BTreeMap::new(),
        )
        .unwrap()
        .remove(0)
    };
    let e = run("delete", &db);
    assert_eq!(e.action, Change::Delete);
    assert!(e.permit_required && e.driver_plan.is_some());
    let e = run("unknown", &db);
    assert_eq!(e.action, Change::Unknown);
    assert!(e.permit_required && e.driver_plan.is_none());
    // A first attempt that never provisioned keeps its refused plan too.
    let mut fresh = db.clone();
    fresh.last_deploy_id = None;
    assert_eq!(run("replace", &fresh).action, Change::Replace);
}

#[test]
fn a_refused_driver_plan_lapses_when_the_configuration_changes() {
    let d = deployment(&[(
        node(DB, "pg", "openporch/opentofu"),
        def("pg", "openporch/opentofu", None),
    )]);
    let db = active(DB, "openporch/opentofu");
    let run = |d: &Deployment, planned_for: &Deployment| {
        let plan = DriverPlan {
            action: "replace".into(),
            fingerprint: "fp-1".into(),
            actions: Vec::new(),
            config_digest: config_digests(planned_for).remove(DB),
        };
        preview(
            d,
            std::slice::from_ref(&db),
            facts,
            &BTreeMap::from([(db.guresid.clone(), plan)]),
            &BTreeMap::new(),
        )
        .unwrap()
        .remove(0)
    };
    assert_eq!(run(&d, &d).action, Change::Replace);
    // A new definition version is planned again when the deployment runs.
    let mut changed = d.clone();
    for (version, _) in changed.definitions.values_mut() {
        *version = 2;
    }
    let e = run(&changed, &d);
    assert_eq!(e.action, Change::Update);
    assert!(!e.permit_required && e.driver_plan.is_none());
}

#[test]
fn a_digest_follows_what_the_change_acts_on() {
    let mut d = deployment(&[
        (
            node(QUEUE, "blind", "test/blind"),
            def("blind", "test/blind", Some("aws-dev")),
        ),
        (
            node(DB, "pg", "openporch/opentofu"),
            def("pg", "openporch/opentofu", None),
        ),
    ]);
    for n in d.graph.nodes.values_mut() {
        if n.descriptor.to_string() == QUEUE {
            n.depends_on.insert(Descriptor::parse(DB).unwrap());
        }
    }
    let mut queue = active(QUEUE, "test/blind");
    queue.driver_account = Some("aws-dev".into());
    let db = active(DB, "openporch/opentofu");
    let account = |role: &str| -> CloudAccount {
        serde_json::from_value(json!({
            "id": "aws-dev", "type": "aws-role", "config": {"role_arn": role},
        }))
        .unwrap()
    };
    let run = |queue: &ActiveResource, db: &ActiveResource, account: CloudAccount| {
        let e = preview(
            &d,
            &[queue.clone(), db.clone()],
            facts,
            &BTreeMap::new(),
            &BTreeMap::from([("aws-dev".to_owned(), account)]),
        )
        .unwrap();
        let e = by_descriptor(&e)[QUEUE].clone();
        assert_eq!(e.action, Change::Unknown);
        e.digest
    };
    let first = run(&queue, &db, account("arn:aws:iam::1:role/a"));
    assert_eq!(first, run(&queue, &db, account("arn:aws:iam::1:role/a")));
    // The cloud account it runs in changes.
    assert_ne!(first, run(&queue, &db, account("arn:aws:iam::1:role/b")));
    // A dependency's outputs change, as an observation records them.
    let mut observed = db.clone();
    observed.outputs = json!({"host": "db-2.internal"});
    assert_ne!(
        first,
        run(&queue, &observed, account("arn:aws:iam::1:role/a"))
    );
    // Its own record changes.
    let mut recovered = queue.clone();
    recovered.target = Some("123456789012".into());
    assert_ne!(
        first,
        run(&recovered, &db, account("arn:aws:iam::1:role/a"))
    );
    // The driver state a recovery checkpoints changes, even when its inputs
    // and outputs stay the same.
    let mut checkpointed = queue.clone();
    checkpointed.recovery = json!({"etag": "2"});
    assert_ne!(
        first,
        run(&checkpointed, &db, account("arn:aws:iam::1:role/a"))
    );
    let mut checkpointed = queue.clone();
    checkpointed.snapshot = json!({"size": "large"});
    assert_ne!(
        first,
        run(&checkpointed, &db, account("arn:aws:iam::1:role/a"))
    );
    let mut checkpointed = queue.clone();
    checkpointed.cookie = Some(Sealed {
        key_id: "k1".into(),
        nonce: vec![1; 12],
        ciphertext: vec![2; 16],
    });
    assert_ne!(
        first,
        run(&checkpointed, &db, account("arn:aws:iam::1:role/a"))
    );
    // Its status alone does not.
    let mut status = queue.clone();
    status.status = "drifted".into();
    assert_eq!(first, run(&status, &db, account("arn:aws:iam::1:role/a")));
}
