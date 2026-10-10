use super::*;
use crate::catalog::builtin_types;
use serde_json::json;

fn parse(doc: &str) -> Result<Workload, WorkloadError> {
    Workload::parse(doc, None)
}

const FUNCTION: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata:
  name: order-worker
  class: aws-lambda
function:
  package:
    type: zip
    artifact: .
    runtime: python3.12
    handler: app.handler
  variables:
    TABLE: ${resources.orders.name}
triggers:
  inbox:
    queue:
      resource: inbox
      batchSize: 10
  nightly:
    schedule:
      cron: "0 3 * * *"
resources:
  orders:
    type: dynamodb-table
  inbox:
    type: sqs
"#;

#[test]
fn function_workload() {
    let w = parse(FUNCTION).unwrap();
    let f = w.function.as_ref().unwrap();
    assert!(w.containers.is_none());
    assert_eq!(f.memory, "256Mi");
    assert_eq!(f.timeout, "30s");
    assert!(
        matches!(&f.package, Package::Zip { artifact, runtime, .. } if artifact == "." && runtime == "python3.12")
    );
    assert_eq!(w.class().as_str(), "aws-lambda");
}

#[test]
fn image_function() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: api}
function:
  package: {type: image, image: .}
"#,
    )
    .unwrap();
    assert!(matches!(w.function.unwrap().package, Package::Image { image } if image == "."));
}

#[test]
fn both_kinds_are_rejected() {
    let e = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: api}
containers: {web: {image: nginx}}
function: {package: {type: image, image: x}}
"#,
    )
    .unwrap_err();
    assert!(e.message.contains("either containers or a function"), "{e}");
}

#[test]
fn neither_kind_is_rejected() {
    let e =
        parse("apiVersion: openporch.dev/v1\nkind: Workload\nmetadata: {name: api}\n").unwrap_err();
    assert!(
        e.message.contains("needs `containers` or `function`"),
        "{e}"
    );
}

#[test]
fn unknown_field_is_named() {
    let e = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: api}
containers: {web: {image: nginx}}
volumes: {}
"#,
    )
    .unwrap_err();
    assert!(e.message.contains("volumes"), "{e}");

    let e = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: api}
containers: {web: {image: nginx, volumes: []}}
"#,
    )
    .unwrap_err();
    assert_eq!(e.path, "containers.web.volumes");
    assert!(e.message.contains("volumes"), "{e}");
}

#[test]
fn web_container_implies_http_trigger() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: ghcr.io/acme/web:1
service:
  ports:
    http: {port: 80, targetPort: 8080}
    metrics: {port: 9090}
"#,
    )
    .unwrap();
    let triggers = w.effective_triggers();
    assert_eq!(triggers.len(), 1);
    assert_eq!(
        triggers["http"],
        Trigger::Http(HttpTrigger {
            port: Some("http".into()),
            ..Default::default()
        })
    );
    let m = w.to_module(&builtin_types()).unwrap().module;
    assert_eq!(
        m.spec["triggers"],
        json!({"http": {"http": {"port": "http"}}})
    );
}

#[test]
fn queue_trigger_on_a_dependency() {
    let w = parse(FUNCTION).unwrap();
    assert_eq!(w.triggers["inbox"].resource(), Some("inbox"));
    let c = w.to_module(&builtin_types()).unwrap();
    assert_eq!(c.module.externals["inbox"].ty.as_str(), "sqs");
    assert_eq!(
        c.module.spec["triggers"]["inbox"],
        json!({"queue": {"resource": "inbox", "batchSize": 10}})
    );
    assert_eq!(
        c.module.spec["function"]["variables"]["TABLE"],
        "${externals.orders.name}"
    );
}

#[test]
fn trigger_names_missing_resource() {
    let doc = FUNCTION.replace("  inbox:\n    type: sqs\n", "");
    let e = parse(&doc).unwrap_err();
    assert_eq!(e.path, "triggers.inbox.queue.resource");
    assert!(e.message.contains("`inbox`"), "{e}");
}

#[test]
fn trigger_has_exactly_one_kind() {
    let doc = FUNCTION.replace("    queue:\n", "    topic: {resource: inbox}\n    queue:\n");
    assert!(parse(&doc).is_err());
    let doc = FUNCTION.replace("cron: \"0 3 * * *\"", "cron: \"0 3 * *\"");
    assert_eq!(
        parse(&doc).unwrap_err().path,
        "triggers.nightly.schedule.cron"
    );
}

#[test]
fn private_and_shared_dependencies() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    variables:
      API: https://${resources.dns.host}/api
resources:
  db: {type: postgres}
  dns: {type: dns, id: api-dns}
"#,
    )
    .unwrap();
    let c = w.to_module(&builtin_types()).unwrap();
    assert_eq!(c.module.externals["db"].id, None);
    assert_eq!(c.module.externals["dns"].id.as_deref(), Some("api-dns"));
    assert_eq!(
        c.module.spec["containers"]["web"]["variables"]["API"],
        "https://${shared.api-dns.host}/api"
    );
}

#[test]
fn connection_string_is_translated_and_marked_secret() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    variables:
      DB_URL: postgresql://${resources.db.username}:${resources.db.password}@${resources.db.host}:${resources.db.port}/${resources.db.name}
      DB_HOST: ${resources.db.host}
resources:
  db: {type: postgres}
"#,
    )
    .unwrap();
    let c = w.to_module(&builtin_types()).unwrap();
    assert_eq!(
        c.module.spec["containers"]["web"]["variables"]["DB_URL"],
        "postgresql://${externals.db.username}:${externals.db.password}@${externals.db.host}:${externals.db.port}/${externals.db.name}"
    );
    assert_eq!(c.secret_paths, ["/containers/web/variables/DB_URL"]);
}

#[test]
fn unknown_output_is_rejected() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    variables: {H: "${resources.db.hostname}"}
resources:
  db: {type: postgres}
"#,
    )
    .unwrap();
    let e = w.to_module(&builtin_types()).unwrap_err();
    assert_eq!(e.path, "containers.web.variables.H");
    assert!(e.message.contains("`hostname`"), "{e}");
}

#[test]
fn open_output_types_accept_any_output() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    variables: {FLAG: "${resources.cfg.anything.deep}", LIT: "a $\\{b} c ${values.region}"}
resources:
  cfg: {type: config}
"#,
    )
    .unwrap();
    let c = w.to_module(&builtin_types()).unwrap();
    let vars = &c.module.spec["containers"]["web"]["variables"];
    assert_eq!(vars["FLAG"], "${externals.cfg.anything.deep}");
    assert_eq!(vars["LIT"], "a $\\{b} c ${values.region}");
}

#[test]
fn secrets_are_only_allowed_in_variables_and_file_content() {
    let doc = |field: &str| {
        format!(
            r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {{name: web}}
containers:
  web:
    image: nginx
    {field}
resources:
  db: {{type: postgres}}
"#
        )
    };
    let w = parse(&doc(r#"args: ["--password=${resources.db.password}"]"#)).unwrap();
    let e = w.to_module(&builtin_types()).unwrap_err();
    assert_eq!(e.path, "containers.web.args.0");
    assert!(e.message.contains("is a secret output"), "{e}");

    let w = parse(&doc(
        r#"files: {/etc/pw: {content: "${resources.db.password}"}}"#,
    ))
    .unwrap();
    let c = w.to_module(&builtin_types()).unwrap();
    assert_eq!(c.secret_paths, ["/containers/web/files/~1etc~1pw/content"]);
}

#[test]
fn no_expand_file_content_is_kept_literally() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    files:
      /etc/tpl: {content: "${context.env.id} ${resources.nope.x} ${", noExpand: true}
"#,
    )
    .unwrap();
    let c = w.to_module(&builtin_types()).unwrap();
    assert_eq!(
        c.module.spec["containers"]["web"]["files"]["/etc/tpl"]["content"],
        "${context.env.id} ${resources.nope.x} ${"
    );
    assert!(c.secret_paths.is_empty());
}

#[test]
fn placeholders_in_resource_params_are_rejected() {
    let e = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web: {image: nginx}
resources:
  bucket: {type: s3}
  db: {type: postgres, params: {extensions: ["${resources.bucket.name}"]}}
"#,
    )
    .unwrap_err();
    assert_eq!(e.path, "resources.db.params.extensions.0");
    assert!(
        e.message.contains("not supported in resource params"),
        "{e}"
    );
}

#[test]
fn context_placeholders_are_rejected() {
    let e = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    variables: {ENV: "${context.env.id}"}
"#,
    )
    .unwrap_err();
    assert_eq!(e.path, "containers.web.variables.ENV");
    assert!(e.message.contains("use `values`"), "{e}");
}

#[test]
fn deploy_defaults_and_target_passthrough() {
    let w = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web, class: aws-lambda}
containers: {web: {image: nginx}}
target:
  kubernetes:
    pod:
      annotations: {billing-code: X}
"#,
    )
    .unwrap();
    let m = w.to_module(&builtin_types()).unwrap().module;
    assert_eq!(m.class.as_str(), "aws-lambda");
    assert_eq!(
        m.spec["deploy"],
        json!({"status": "available", "timeout": "5m"})
    );
    assert_eq!(
        m.spec["target"]["kubernetes"]["pod"]["annotations"]["billing-code"],
        "X"
    );
}

#[test]
fn quantities_and_durations() {
    for q in ["256Mi", "1Gi", "512", "0.5", "250m", "1.5G"] {
        assert!(is_quantity(q), "{q}");
    }
    for q in ["", "Mi", "1.Mi", "abc", "1 Gi"] {
        assert!(!is_quantity(q), "{q}");
    }
    for d in ["30s", "5m", "1h30m", "500ms"] {
        assert!(is_duration(d), "{d}");
    }
    for d in ["", "30", "s", "1d", "5m "] {
        assert!(!is_duration(d), "{d}");
    }
    let doc = FUNCTION.replace(
        "handler: app.handler",
        "handler: app.handler\n  timeout: forever",
    );
    assert_eq!(parse(&doc).unwrap_err().path, "function.timeout");
}

const SCORE: &str = r#"
apiVersion: score.dev/v1b1
metadata:
  name: web
containers:
  main:
    image: ghcr.io/acme/web:1
    variables:
      DB_HOST: ${resources.db.host}
service:
  ports:
    http: {port: 80, targetPort: 8080}
resources:
  db:
    type: postgres
"#;

#[test]
fn plain_score_file() {
    let imported = parse(SCORE).unwrap();
    let native = parse(
        r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata:
  name: web
containers:
  main:
    image: ghcr.io/acme/web:1
    variables:
      DB_HOST: ${resources.db.host}
service:
  ports:
    http: {port: 80, targetPort: 8080}
resources:
  db:
    type: postgres
"#,
    )
    .unwrap();
    assert_eq!(imported, native);
    assert_eq!(imported.class().as_str(), "default");
    assert_eq!(
        imported.to_module(&builtin_types()).unwrap(),
        native.to_module(&builtin_types()).unwrap()
    );
}

#[test]
fn overlay_adds_class_and_triggers() {
    let overlay = r#"
apiVersion: openporch.dev/v1
kind: WorkloadOverlay
metadata:
  class: aws-lambda
deploy:
  timeout: 10m
"#;
    let w = Workload::parse(SCORE, Some(overlay)).unwrap();
    assert_eq!(w.class().as_str(), "aws-lambda");
    assert_eq!(w.deploy.unwrap().timeout.as_deref(), Some("10m"));
}

#[test]
fn overlay_is_only_for_score() {
    let overlay = "apiVersion: openporch.dev/v1\nkind: WorkloadOverlay\n";
    assert!(Workload::parse(FUNCTION, Some(overlay)).is_err());
    let bad = "apiVersion: openporch.dev/v1\nkind: Workload\n";
    assert!(Workload::parse(SCORE, Some(bad)).is_err());
}

#[test]
fn unsupported_score_fields_are_named() {
    let doc = SCORE.replace(
        "    image: ghcr.io/acme/web:1\n",
        "    image: ghcr.io/acme/web:1\n    volumes: {}\n",
    );
    let e = parse(&doc).unwrap_err();
    assert_eq!(e.path, "containers.main.volumes");
    assert!(e.message.contains("volumes"), "{e}");
    let doc = SCORE.replace(
        "metadata:\n  name: web\n",
        "metadata:\n  name: web\nextensions: {}\n",
    );
    assert!(parse(&doc).unwrap_err().message.contains("extensions"));
}

#[test]
fn unknown_api_version() {
    let e = parse("apiVersion: score.dev/v2\n").unwrap_err();
    assert_eq!(e.path, "apiVersion");
}

/// The published schema at `schemas/workload-v1.json` must match the types.
/// Run with `OPENPORCH_UPDATE_SCHEMAS=1` to regenerate it.
#[test]
fn published_json_schema_is_current() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/workload-v1.json"
    );
    let generated = serde_json::to_string_pretty(&Workload::json_schema()).unwrap() + "\n";
    if std::env::var_os("OPENPORCH_UPDATE_SCHEMAS").is_some() {
        std::fs::write(path, &generated).unwrap();
    }
    let published = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        published == generated,
        "schemas/workload-v1.json is stale; rerun with OPENPORCH_UPDATE_SCHEMAS=1"
    );
}

#[test]
fn json_schema_rejects_unknown_fields_and_requires_core_fields() {
    let s = Workload::json_schema();
    assert_eq!(s["additionalProperties"], json!(false));
    assert_eq!(s["required"], json!(["apiVersion", "kind", "metadata"]));
    assert!(s["$defs"]["Trigger"].is_object());
    // Each branch requires its own kind and allows the other to be absent or null,
    // matching what the parser accepts.
    // Identifiers carry their validation rules.
    assert_eq!(
        s["$defs"]["WorkloadId"]["pattern"],
        crate::ids::OBJECT_ID_PATTERN
    );
    assert_eq!(s["$defs"]["ClassId"]["minLength"], 2);
    assert_eq!(
        s["$defs"]["ResourceTypeId"]["pattern"],
        crate::ids::RESOURCE_TYPE_PATTERN
    );
    assert_eq!(s["oneOf"][0]["required"], json!(["containers"]));
    assert_eq!(s["oneOf"][1]["required"], json!(["function"]));
    assert_eq!(
        s["oneOf"][1]["anyOf"][1],
        json!({"properties": {"containers": {"type": "null"}}})
    );
}
