use super::*;
use crate::catalog::{ResourceClass, builtin_types};
use crate::workload::Workload;
use serde_json::json;

struct Fixture {
    org: OrgId,
    app: AppId,
    env: EnvId,
    env_type: EnvTypeId,
    types: Vec<ResourceType>,
    classes: ClassRegistry,
    definitions: Vec<ResourceDefinition>,
}

impl Fixture {
    fn new(definitions: Vec<Value>) -> Self {
        Self {
            org: "acme".parse().unwrap(),
            app: "shop".parse().unwrap(),
            env: "dev".parse().unwrap(),
            env_type: "development".parse().unwrap(),
            types: builtin_types(),
            classes: ClassRegistry::new([ResourceClass {
                id: "aws-lambda".parse().unwrap(),
                resource_type: "workload".parse().unwrap(),
                description: String::new(),
            }]),
            definitions: definitions
                .into_iter()
                .map(|d| serde_json::from_value(d).unwrap())
                .collect(),
        }
    }

    fn build(&self, set: &DeploymentSet) -> Result<ResourceGraph, GraphError> {
        build_graph(GraphInput {
            org: &self.org,
            app: &self.app,
            env: &self.env,
            env_type: &self.env_type,
            set,
            definitions: &self.definitions,
            types: &self.types,
            classes: &self.classes,
        })
    }
}

/// A definition matching every resource of `ty` with the default class.
fn def(id: &str, ty: &str, values: Value) -> Value {
    json!({"id": id, "type": ty, "driver_type": "openporch/echo", "driver_inputs": {"values": values}, "criteria": [{}]})
}

fn base_defs() -> Vec<Value> {
    vec![
        def("none-workload", "workload", json!({})),
        def("base-env", "base-env", json!({})),
        def(
            "pg",
            "postgres",
            json!({"host": "${resources.postgres-instance.outputs.host}"}),
        ),
        def("pg-instance", "postgres-instance", json!({})),
        def("s3", "s3", json!({})),
        def("sqs", "sqs", json!({})),
        def("dns", "dns", json!({})),
        def("ddb", "dynamodb-table", json!({})),
        def(
            "policy",
            "aws-policy",
            json!({"table": "${resources.dynamodb-table.outputs.name}"}),
        ),
    ]
}

fn set_of(workloads: &[&str]) -> DeploymentSet {
    let mut set = DeploymentSet::default();
    for doc in workloads {
        let c = Workload::parse(doc, None)
            .unwrap()
            .to_module(&builtin_types())
            .unwrap();
        set.modules.insert(c.id, c.module);
    }
    set
}

fn d(s: &str) -> Descriptor {
    Descriptor::parse(s).unwrap()
}

fn deps(g: &ResourceGraph, s: &str) -> Vec<String> {
    g.nodes[&d(s)]
        .depends_on
        .iter()
        .map(ToString::to_string)
        .collect()
}

const WEB_DB: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers: {web: {image: nginx, variables: {DB_HOST: "${resources.db.host}"}}}
resources:
  db: {type: postgres}
"#;

#[test]
fn workload_with_a_database() {
    let g = Fixture::new(base_defs()).build(&set_of(&[WEB_DB])).unwrap();
    let ids: Vec<String> = g.nodes.keys().map(ToString::to_string).collect();
    assert_eq!(
        ids,
        [
            "base-env.default#base-env",
            "postgres.default#modules.web.externals.db",
            "postgres-instance.default#modules.web.externals.db",
            "workload.default#modules.web",
        ]
    );
    assert_eq!(
        deps(&g, "workload.default#modules.web"),
        ["postgres.default#modules.web.externals.db"]
    );
    assert_eq!(
        deps(&g, "postgres.default#modules.web.externals.db"),
        ["postgres-instance.default#modules.web.externals.db"]
    );
    assert_eq!(
        g.nodes[&d("workload.default#modules.web")]
            .definition
            .as_str(),
        "none-workload"
    );
    let order: Vec<String> = g.order().map(ToString::to_string).collect();
    let pos = |s: &str| order.iter().position(|x| x == s).unwrap();
    assert!(
        pos("postgres-instance.default#modules.web.externals.db")
            < pos("postgres.default#modules.web.externals.db")
    );
    assert!(pos("postgres.default#modules.web.externals.db") < pos("workload.default#modules.web"));
}

#[test]
fn kubernetes_workload_definition_adds_cluster_nodes() {
    let mut defs = base_defs();
    defs[0] = def(
        "k8s-workload",
        "workload",
        json!({"cluster": "${resources.k8s-cluster.outputs.name}", "ns": "${resources.k8s-namespace.outputs.namespace}"}),
    );
    defs.push(def("cluster", "k8s-cluster", json!({})));
    defs.push(def(
        "ns",
        "k8s-namespace",
        json!({"name": "${context.app.id}-${context.env.id}"}),
    ));
    let g = Fixture::new(defs).build(&set_of(&[WEB_DB])).unwrap();
    assert_eq!(
        deps(&g, "workload.default#modules.web"),
        [
            "k8s-cluster.default#k8s-cluster",
            "k8s-namespace.default#k8s-namespace",
            "postgres.default#modules.web.externals.db",
        ]
    );
}

#[test]
fn cycle_is_reported() {
    let mut defs = base_defs();
    defs[2] = def(
        "pg",
        "postgres",
        json!({"host": "${resources.postgres-instance.outputs.host}"}),
    );
    defs[3] = def(
        "pg-instance",
        "postgres-instance",
        json!({"db": "${resources.postgres.outputs.name}"}),
    );
    let e = Fixture::new(defs).build(&set_of(&[WEB_DB])).unwrap_err();
    let GraphError::Cycle(cycle) = &e else {
        panic!("{e}")
    };
    assert_eq!(cycle.first(), cycle.last());
    let msg = e.to_string();
    assert!(
        msg.contains("postgres.default#modules.web.externals.db"),
        "{msg}"
    );
    assert!(
        msg.contains("postgres-instance.default#modules.web.externals.db"),
        "{msg}"
    );
}

#[test]
fn parallel_branches_share_a_layer() {
    let g = Fixture::new(base_defs())
        .build(&set_of(&[r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers: {web: {image: nginx}}
resources:
  db: {type: postgres}
  files: {type: s3}
"#]))
        .unwrap();
    let layer_of = |s: &str| g.layers.iter().position(|l| l.contains(&d(s))).unwrap();
    assert_eq!(
        layer_of("s3.default#modules.web.externals.files"),
        layer_of("postgres-instance.default#modules.web.externals.db")
    );
    assert!(
        layer_of("workload.default#modules.web")
            > layer_of("postgres.default#modules.web.externals.db")
    );
    assert!(
        layer_of("workload.default#modules.web")
            > layer_of("s3.default#modules.web.externals.files")
    );
}

#[test]
fn queue_trigger_dependency_is_in_the_graph() {
    let mut defs = base_defs();
    defs.push(json!({"id": "lambda", "type": "workload", "driver_type": "openporch/opentofu", "criteria": [{"class": "aws-lambda"}]}));
    let g = Fixture::new(defs)
        .build(&set_of(&[r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: worker, class: aws-lambda}
function: {package: {type: zip, artifact: ., runtime: python3.12, handler: app.handler}}
triggers: {inbox: {queue: {resource: inbox}}}
resources: {inbox: {type: sqs}}
"#]))
        .unwrap();
    let w = &g.nodes[&d("workload.aws-lambda#modules.worker")];
    assert_eq!(w.definition.as_str(), "lambda");
    assert!(
        w.depends_on
            .contains(&d("sqs.default#modules.worker.externals.inbox"))
    );
}

#[test]
fn undefined_class_fails_naming_it() {
    let e = Fixture::new(base_defs())
        .build(&set_of(&[r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers: {web: {image: nginx}}
resources: {files: {type: s3, class: sensitive}}
"#]))
        .unwrap_err();
    assert!(
        matches!(&e, GraphError::MissingClass { class, .. } if class.as_str() == "sensitive"),
        "{e}"
    );
    assert!(e.to_string().contains("modules.web.externals.files"));
}

#[test]
fn no_workload_definition_names_workload_class_and_env() {
    let e = Fixture::new(base_defs())
        .build(&set_of(&[r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: api, class: aws-lambda}
function: {package: {type: image, image: .}}
"#]))
        .unwrap_err();
    let msg = e.to_string();
    for part in ["modules.api", "aws-lambda", "dev", "workload"] {
        assert!(msg.contains(part), "{msg} lacks {part}");
    }
}

const DNS_A: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: alpha}
containers: {web: {image: nginx}}
resources: {dns: {type: dns, id: api-dns}}
"#;

#[test]
fn shared_resource_is_one_node() {
    let b = DNS_A.replace("name: alpha", "name: beta");
    let g = Fixture::new(base_defs())
        .build(&set_of(&[DNS_A, &b]))
        .unwrap();
    let shared = &g.nodes[&d("dns.default#shared.api-dns")];
    let paths: Vec<&str> = shared.requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        ["modules.alpha.externals.dns", "modules.beta.externals.dns"]
    );
    assert!(
        g.nodes[&d("workload.default#modules.alpha")]
            .depends_on
            .contains(&shared.descriptor)
    );
    assert!(
        g.nodes[&d("workload.default#modules.beta")]
            .depends_on
            .contains(&shared.descriptor)
    );
}

#[test]
fn conflicting_shared_table_names_both_paths() {
    let table = |name: &str, key: &str| {
        format!(
            r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {{name: {name}}}
containers: {{web: {{image: nginx}}}}
resources: {{orders: {{type: dynamodb-table, id: orders, params: {{hash_key: {key}}}}}}}
"#
        )
    };
    let (a, b) = (table("alpha", "id"), table("beta", "order_id"));
    let e = Fixture::new(base_defs())
        .build(&set_of(&[&a, &b]))
        .unwrap_err();
    assert_eq!(
        e,
        GraphError::Conflict {
            descriptor: d("dynamodb-table.default#shared.orders"),
            first: "modules.alpha.externals.orders".into(),
            second: "modules.beta.externals.orders".into(),
        }
    );
    // Equal params merge.
    let same = table("beta", "id");
    assert!(
        Fixture::new(base_defs())
            .build(&set_of(&[&a, &same]))
            .is_ok()
    );
}

const TABLE_WORKLOAD: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers: {web: {image: nginx}}
resources: {orders: {type: dynamodb-table}}
"#;

fn coprovision_graph(is_dependent: bool, match_dependents: bool) -> ResourceGraph {
    let mut defs = base_defs();
    defs[7] = json!({
        "id": "ddb", "type": "dynamodb-table", "driver_type": "openporch/echo", "criteria": [{}],
        "provision": {"aws-policy": {"is_dependent": is_dependent, "match_dependents": match_dependents}}
    });
    // The policy references nothing here, so only co-provisioning edges appear.
    defs[8] = def("policy", "aws-policy", json!({}));
    Fixture::new(defs)
        .build(&set_of(&[TABLE_WORKLOAD]))
        .unwrap()
}

#[test]
fn coprovisioning_truth_table() {
    let a = "dynamodb-table.default#modules.web.externals.orders";
    let b = "aws-policy.default#modules.web.externals.orders";
    let c = "workload.default#modules.web";
    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort();
        v
    }
    for (is_dependent, match_dependents, a_deps, b_deps, c_deps) in [
        (false, false, vec![], vec![a], vec![a]),
        (false, true, vec![], vec![a], vec![b, a]),
        (true, false, vec![b], vec![], vec![a]),
        (true, true, vec![b], vec![], vec![b, a]),
    ] {
        let g = coprovision_graph(is_dependent, match_dependents);
        let case = format!("is_dependent={is_dependent} match_dependents={match_dependents}");
        assert_eq!(deps(&g, a), sorted(a_deps), "{case}");
        assert_eq!(deps(&g, b), sorted(b_deps), "{case}");
        assert_eq!(deps(&g, c), sorted(c_deps), "{case}");
    }
}

#[test]
fn policy_coprovisioned_with_a_database() {
    let mut defs = base_defs();
    defs[2] = json!({
        "id": "pg", "type": "postgres", "driver_type": "openporch/echo", "criteria": [{}],
        "provision": {"aws-policy": {"is_dependent": false, "match_dependents": true}}
    });
    defs[8] = def(
        "policy",
        "aws-policy",
        json!({"db": "${resources.postgres.outputs.name}"}),
    );
    let g = Fixture::new(defs).build(&set_of(&[WEB_DB])).unwrap();
    let policy = "aws-policy.default#modules.web.externals.db";
    assert_eq!(
        deps(&g, policy),
        ["postgres.default#modules.web.externals.db"]
    );
    assert_eq!(
        deps(&g, "workload.default#modules.web"),
        [policy, "postgres.default#modules.web.externals.db"]
    );
}

#[test]
fn match_dependents_reaches_a_fixed_point_through_chained_coprovisioning() {
    // The table co-provisions a policy, and the policy co-provisions a role; both expose
    // themselves to consumers, so the workload waits for all three.
    let mut defs = base_defs();
    defs[7] = json!({
        "id": "ddb", "type": "dynamodb-table", "driver_type": "openporch/echo", "criteria": [{}],
        "provision": {"aws-policy": {"match_dependents": true}}
    });
    defs[8] = json!({
        "id": "policy", "type": "aws-policy", "driver_type": "openporch/echo", "criteria": [{}],
        "provision": {"aws-role": {"match_dependents": true}}
    });
    defs.push(def("role", "aws-role", json!({})));
    let g = Fixture::new(defs)
        .build(&set_of(&[TABLE_WORKLOAD]))
        .unwrap();
    assert_eq!(
        deps(&g, "workload.default#modules.web"),
        [
            "aws-policy.default#modules.web.externals.orders",
            "aws-role.default#modules.web.externals.orders",
            "dynamodb-table.default#modules.web.externals.orders",
        ]
    );
}

#[test]
fn conflicting_coprovision_params_fail() {
    let mut defs = base_defs();
    defs[7] = json!({
        "id": "ddb", "type": "dynamodb-table", "driver_type": "openporch/echo", "criteria": [{}],
        "provision": {"aws-policy#shared.p": {"params": {"actions": ["read"]}}}
    });
    defs[2] = json!({
        "id": "pg", "type": "postgres", "driver_type": "openporch/echo", "criteria": [{}],
        "provision": {"aws-policy#shared.p": {"params": {"actions": ["write"]}}}
    });
    let w = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers: {web: {image: nginx}}
resources: {orders: {type: dynamodb-table}, db: {type: postgres}}
"#;
    let e = Fixture::new(defs).build(&set_of(&[w])).unwrap_err();
    assert!(
        matches!(&e, GraphError::Conflict { descriptor, .. } if descriptor.to_string() == "aws-policy.default#shared.p"),
        "{e}"
    );
}

#[test]
fn graph_is_independent_of_definition_order() {
    let orders = TABLE_WORKLOAD.replace("name: web", "name: orders");
    let workloads = [WEB_DB, DNS_A, orders.as_str()];
    let mut defs = base_defs();
    defs[7] = json!({
        "id": "ddb", "type": "dynamodb-table", "driver_type": "openporch/echo", "criteria": [{}],
        "provision": {"aws-policy": {"match_dependents": true}}
    });
    let g1 = Fixture::new(defs.clone())
        .build(&set_of(&workloads))
        .unwrap();
    defs.reverse();
    let g2 = Fixture::new(defs.clone())
        .build(&set_of(&workloads))
        .unwrap();
    let mut reordered = workloads;
    reordered.reverse();
    let g3 = Fixture::new(defs).build(&set_of(&reordered)).unwrap();
    assert_eq!(g1.nodes.len(), 9);
    assert_eq!(g1, g2);
    assert_eq!(g1, g3);
}

#[test]
fn shared_resources_from_the_set() {
    let shared = |params: Value| crate::sets::SharedResource {
        ty: "dns".parse().unwrap(),
        class: ClassId::default(),
        params: Some(params),
    };
    // The workload requests `api-dns` without params, so the set must not add any.
    let mut set = set_of(&[DNS_A]);
    set.shared
        .insert("api-dns".into(), shared(json!({"sub": "api"})));
    let e = Fixture::new(base_defs()).build(&set).unwrap_err();
    assert_eq!(
        e,
        GraphError::Conflict {
            descriptor: d("dns.default#shared.api-dns"),
            first: "modules.alpha.externals.dns".into(),
            second: "shared.api-dns".into(),
        }
    );

    let with_params = DNS_A.replace("id: api-dns}", "id: api-dns, params: {sub: api}}");
    let mut set = set_of(&[&with_params]);
    set.shared
        .insert("api-dns".into(), shared(json!({"sub": "api"})));
    let g = Fixture::new(base_defs()).build(&set).unwrap();
    let node = &g.nodes[&d("dns.default#shared.api-dns")];
    assert_eq!(node.params, Some(json!({"sub": "api"})));
    assert_eq!(node.requests.len(), 2);
}

#[test]
fn omitted_params_conflict_with_supplied_params() {
    let with_params = DNS_A
        .replace("name: alpha", "name: beta")
        .replace("id: api-dns}", "id: api-dns, params: {sub: api}}");
    let e = Fixture::new(base_defs())
        .build(&set_of(&[DNS_A, &with_params]))
        .unwrap_err();
    assert!(matches!(e, GraphError::Conflict { .. }), "{e}");
}

#[test]
fn unknown_types_and_bad_placeholders_fail() {
    let w = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers: {web: {image: nginx}}
resources: {x: {type: acme/flags}}
"#;
    let wl = Workload::parse(w, None).unwrap();
    let mut types = builtin_types();
    let mut custom = types[5].clone();
    custom.id = "acme/flags".parse().unwrap();
    types.push(custom);
    let c = wl.to_module(&types).unwrap();
    let mut set = DeploymentSet::default();
    set.modules.insert(c.id, c.module);
    let e = Fixture::new(base_defs()).build(&set).unwrap_err();
    assert!(matches!(e, GraphError::UnknownType { .. }), "{e}");

    let mut defs = base_defs();
    defs[2] = def("pg", "postgres", json!({"x": "${secrets.foo}"}));
    let e = Fixture::new(defs).build(&set_of(&[WEB_DB])).unwrap_err();
    assert!(
        matches!(&e, GraphError::Placeholder { definition, .. } if definition.as_str() == "pg"),
        "{e}"
    );
    assert!(e.to_string().contains("${secrets.foo}"));
}
