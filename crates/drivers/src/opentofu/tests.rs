//! Input checks and a fake `tofu` run without OpenTofu; end-to-end runs against
//! the real binary are `#[ignore]`d and run in CI's OpenTofu job
//! (`cargo test -p openporch-drivers opentofu -- --ignored`). Set
//! `OPENPORCH_TEST_TOFU` to the binary when it is not on the search path.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use openporch_core::ids::Guresid;
use serde_json::{Map, Value, json};

use super::*;
use crate::{DriverContext, LogSink, Operation};

fn object(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

fn request(values: Value, secrets: Value) -> DriverRequest {
    DriverRequest {
        guresid: Guresid::new("0123456789abcdef0123456789abcdef").unwrap(),
        operation: Operation {
            id: "op-1".into(),
            fingerprint: "f1".into(),
        },
        resource_type: "password".into(),
        resource: Map::new(),
        resource_secrets: SecretMap::default(),
        resource_secret_paths: Vec::new(),
        values: object(values),
        secrets: SecretMap::new(object(secrets)),
        credentials: None,
        context: DriverContext::default(),
        cookie: None,
        permit: None,
        pin: None,
        log: LogSink::default(),
    }
}

fn driver(root: &Path) -> OpenTofu {
    let mut config = Config::new(root);
    if let Ok(tofu) = std::env::var("OPENPORCH_TEST_TOFU") {
        config.tofu = tofu.into();
    }
    config.allow_file_sources = true;
    OpenTofu::new(config)
}

/// Collects log lines.
fn capture(req: &mut DriverRequest) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = lines.clone();
    req.log = LogSink::new(move |l| sink.lock().unwrap().push(l.to_owned()));
    lines
}

fn invalid(err: DriverError) -> (String, String) {
    match err {
        DriverError::Invalid { code, message } => (code.unwrap_or_default(), message),
        other => panic!("expected an invalid-input error, got {other:?}"),
    }
}

#[tokio::test]
async fn inputs_are_checked_before_anything_runs() {
    let root = tempfile::tempdir().unwrap();
    let mut d = driver(root.path());
    d.config.tofu = "/nonexistent/tofu".into();
    d.config.allow_file_sources = false;
    for (values, secrets, want) in [
        (json!({}), json!({}), "set `source`, `script` or both"),
        (
            json!({"script": "", "colour": 1}),
            json!({}),
            "unknown OpenTofu driver input `colour`",
        ),
        (
            json!({"script": "", "backend": {"s3": {}}}),
            json!({}),
            "`backend` is not supported yet",
        ),
        (
            json!({"source": {"url": "https://git.example/m", "rev": "--upload-pack=x"}}),
            json!({}),
            "`source.rev` must be",
        ),
        (
            json!({"source": {"url": "https://user:pw@git.example/m"}}),
            json!({}),
            "must not contain credentials",
        ),
        (
            json!({"source": {"url": "file:///etc"}}),
            json!({}),
            "must use https or ssh",
        ),
        (
            json!({"source": {"url": "https://git.example/m", "path": "../x"}}),
            json!({}),
            "`source.path` must be",
        ),
        (
            json!({"script": "", "files": {"/etc/passwd": "x"}}),
            json!({}),
            "file `/etc/passwd` must be",
        ),
        (
            json!({"script": ""}),
            json!({"files": {"a/../../b": "x"}}),
            "file `a/../../b` must be",
        ),
        (
            json!({"script": "", "credentials_config": {"environment": {}}}),
            json!({}),
            "needs a `driver_account`",
        ),
        (
            json!({"script": ""}),
            json!({"tokens": {"bad host": "token-1"}}),
            "is not a host name",
        ),
        (
            json!({"script": "", "variables": {"resource": {}}}),
            json!({}),
            "the variable `resource` is reserved",
        ),
        (
            json!({"script": ""}),
            json!({"variables": {"resource_secrets": "x"}}),
            "the variable `resource_secrets` is reserved",
        ),
        (
            json!({"script": "", "files": {"z_override.tf": "terraform {}"}}),
            json!({}),
            "would replace the script, the backend",
        ),
        (
            json!({"script": "", "files": {"main.tf": ""}}),
            json!({}),
            "would replace the script",
        ),
        (
            json!({"script": "", "files": {".terraform/x": ""}}),
            json!({}),
            "OpenTofu's own files",
        ),
        (
            json!({"source": {"url": "ssh://git:pw@git.example/m"}}),
            json!({}),
            "must not contain credentials",
        ),
    ] {
        let err = d.put(&request(values, secrets)).await.unwrap_err();
        let (code, message) = invalid(err);
        assert_eq!(code, "TOFU-003", "{message}");
        assert!(message.contains(want), "{message} should contain {want}");
    }
}

/// A stand-in for `tofu` that records its arguments and environment and
/// prints a plan with one create and one sensitive output.
const FAKE_TOFU: &str = r#"#!/bin/sh
{ echo "ARGS $*"; env | sort; for f in "$@"; do case "$f" in -var-file=*) echo "VARFILE ${f#-var-file=}"; cat "${f#-var-file=}"; echo; esac; done; } >> "$FAKE_TOFU_LOG"
case "$1" in
  init) { cat zzzz_openporch_backend_override.tf 2>/dev/null || [ "$3" = -backend=false ]; } >> "$FAKE_TOFU_LOG" ;;
  show) echo '{"resource_changes":[{"address":"random_password.p","change":{"actions":["create"]}}]}' ;;
  output) echo '{"password":{"sensitive":true,"value":"generated-pw-123"},"length":{"sensitive":false,"value":12}}' ;;
  apply) echo "applied password generated-pw-123 with token hunter2-token" ;;
esac
"#;

#[tokio::test]
async fn fake_run_passes_credentials_through_files_and_environment_only() {
    let root = tempfile::tempdir().unwrap();
    let fake = root.path().join("fake-tofu");
    std::fs::write(&fake, FAKE_TOFU).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let record = root.path().join("record.log");
    // The fake reads its log path from its environment, which only
    // `credentials_config` can set.
    let mut d = driver(root.path());
    d.config.tofu = fake;
    let mut req = request(
        json!({
            "script": "resource \"random_password\" \"p\" { length = var.length }",
            "variables": {"length": 12},
            "credentials_config": {"environment": {"FAKE_TOFU_LOG": "LogPath", "AWS_ACCESS_KEY_ID": "AccessKeyId"},
                                   "variables": {"secret_key": "SecretAccessKey"}},
        }),
        json!({"variables": {"api_token": "hunter2-token"}, "tokens": {"app.terraform.io": "tfc-token-1"}}),
    );
    req.credentials = Some(SecretMap::new(object(json!({
        "LogPath": record.display().to_string(),
        "AccessKeyId": "AKIAFAKE",
        "SecretAccessKey": "aws-secret-1",
    }))));
    // The resource's inputs reach the module as two variables.
    req.resource = object(json!({"primaryKey": {"name": "id", "type": "S"}}));
    req.resource_secrets = SecretMap::new(object(json!({"spec": {"KEY": "resource-secret-1"}})));
    let lines = capture(&mut req);
    let PutOutcome::Ready { outputs, cookie } = d.put(&req).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.values["length"], 12);
    assert_eq!(outputs.secrets.expose()["password"], "generated-pw-123");
    assert!(cookie.is_some());

    let record = std::fs::read_to_string(&record).unwrap();
    // Secrets travel in variable files and environment entries, never arguments.
    for line in record.lines().filter(|l| l.starts_with("ARGS ")) {
        for secret in [
            "hunter2-token",
            "aws-secret-1",
            "tfc-token-1",
            "resource-secret-1",
        ] {
            assert!(!line.contains(secret), "{line}");
        }
    }
    assert!(record.contains("AWS_ACCESS_KEY_ID=AKIAFAKE"));
    assert!(record.contains("TF_TOKEN_app_terraform_io=tfc-token-1"));
    assert!(record.contains("TF_IN_AUTOMATION=1"));
    assert!(record.contains("AWS_EC2_METADATA_DISABLED=true"));
    assert!(record.contains(r#""api_token":"hunter2-token""#));
    assert!(record.contains(r#""secret_key":"aws-secret-1""#));
    assert!(record.contains(r#"{"length":12,"resource":{"primaryKey":{"name":"id","type":"S"}}}"#));
    assert!(record.contains(r#""resource_secrets":{"spec":{"KEY":"resource-secret-1"}}"#));
    // The server's own environment stays out: cargo sets these for the tests.
    assert!(std::env::var("CARGO_PKG_NAME").is_ok());
    assert!(!record.contains("CARGO_"));
    let calls: Vec<&str> = record
        .lines()
        .filter_map(|l| l.strip_prefix("ARGS "))
        .map(|a| a.split(' ').next().unwrap())
        .collect();
    assert_eq!(calls, ["init", "state", "plan", "show", "apply", "output"]);

    // The log shows the run but none of its secrets.
    let log = lines.lock().unwrap().join("\n");
    assert!(
        log.contains("applied password [REDACTED] with token [REDACTED]"),
        "{log}"
    );
    assert!(
        !log.contains("generated-pw-123") && !log.contains("hunter2-token"),
        "{log}"
    );
    // Machine-readable output, which holds every value, is not logged.
    assert!(!log.contains("resource_changes"), "{log}");
    assert!(log.contains("bytes of output not logged"), "{log}");

    // The run directory, with its secret files, is gone.
    let runs = root.path().join("runs/0123456789abcdef0123456789abcdef");
    assert_eq!(std::fs::read_dir(runs).unwrap().count(), 0);
    // State lives outside it, owner-only, with what the apply ran.
    let state = root.path().join("state/0123456789abcdef0123456789abcdef");
    let mode = std::fs::metadata(&state).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert!(state.join("script.tf").exists());
    assert!(state.join("retained.json").exists());
}

/// A driver whose `tofu` is [`FAKE_TOFU`] with `edit` applied, recording to
/// `<root>/record.log`.
fn fake_driver(root: &Path, edit: impl Fn(&str) -> String) -> OpenTofu {
    let fake = root.join("fake-tofu");
    let record = root.join("record.log");
    let script = FAKE_TOFU.replace("\"$FAKE_TOFU_LOG\"", &format!("'{}'", record.display()));
    std::fs::write(&fake, edit(&script)).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut d = driver(root);
    d.config.tofu = fake;
    d
}

#[tokio::test]
async fn a_failed_apply_is_uncertain_and_keeps_what_it_ran() {
    let root = tempfile::tempdir().unwrap();
    let d = fake_driver(root.path(), |s| {
        s.replace(
            r#"apply) echo "applied"#,
            r##"apply) echo "partial apply failed" >&2; exit 1; echo ""##,
        )
    });
    let req = request(json!({"script": "terraform {}"}), json!({}));
    match d.put(&req).await.unwrap_err() {
        DriverError::Uncertain(m) => assert!(m.contains("partial apply failed"), "{m}"),
        other => panic!("expected uncertain, got {other:?}"),
    }
    // A later delete plans with the script the failed apply ran.
    let state = root.path().join("state/0123456789abcdef0123456789abcdef");
    assert_eq!(
        std::fs::read_to_string(state.join("script.tf")).unwrap(),
        "terraform {}"
    );
    assert!(state.join("retained.json").exists());
}

#[tokio::test]
async fn lost_state_needs_recovery() {
    let root = tempfile::tempdir().unwrap();
    let d = fake_driver(root.path(), str::to_owned);
    let mut req = request(json!({"script": "terraform {}"}), json!({}));
    req.cookie = Some(Cookie::new(COOKIE_VERSION, "{}").unwrap());
    // Applied before (there is a cookie) but no state: never start afresh.
    assert!(matches!(d.put(&req).await, Err(DriverError::Uncertain(_))));
    assert!(matches!(d.plan(&req).await, Err(DriverError::Uncertain(_))));
    assert!(matches!(
        d.delete(&req).await,
        Err(DriverError::Uncertain(_))
    ));
    assert!(matches!(
        d.plan_delete(&req).await,
        Err(DriverError::Uncertain(_))
    ));
    assert!(matches!(
        d.get(&req).await.unwrap(),
        Observation::Unknown { .. }
    ));
    assert!(!root.path().join("record.log").exists());
    // The documented recovery, once nothing the resource created remains.
    let record = root.path().join("state/0123456789abcdef0123456789abcdef");
    std::fs::create_dir_all(&record).unwrap();
    std::fs::write(record.join("deleted"), "").unwrap();
    d.put(&req).await.unwrap();
}

#[tokio::test]
async fn permits_cover_one_resource_and_one_kind_of_call() {
    let root = tempfile::tempdir().unwrap();
    let d = fake_driver(root.path(), |s| {
        s.replace(
            r#""actions":["create"]"#,
            r#""actions":["delete","create"]"#,
        )
    });
    let plan = async |guresid: &str, fingerprint: &str| {
        let mut req = request(json!({"script": "terraform {}"}), json!({}));
        req.guresid = Guresid::new(guresid).unwrap();
        req.operation.fingerprint = fingerprint.into();
        let Plan::Actions {
            actions,
            fingerprint,
        } = d.plan(&req).await.unwrap()
        else {
            panic!("no actions");
        };
        assert_eq!(actions, [(Action::Replace, "random_password.p".to_owned())]);
        fingerprint
    };
    let a = plan("0123456789abcdef0123456789abcdef", "f1").await;
    assert_eq!(a, plan("0123456789abcdef0123456789abcdef", "f1").await);
    assert_ne!(a, plan("fedcba9876543210fedcba9876543210", "f1").await);
    assert_ne!(a, plan("0123456789abcdef0123456789abcdef", "f2").await);
}

const PASSWORD_MODULE: &str = r#"
terraform {
  required_providers {
    random = { source = "hashicorp/random", version = "~> 3.6" }
  }
}
variable "length" { type = number }
variable "label" {
  type      = string
  sensitive = true
}
resource "random_password" "p" { length = var.length }
output "password" {
  value     = random_password.p.result
  sensitive = true
}
output "length" { value = var.length }
output "label_length" { value = length(nonsensitive(var.label)) }
"#;

#[test]
fn secret_resource_inputs_are_moved_out_one_by_one() {
    let mut req = request(json!({"script": ""}), json!({}));
    req.resource = object(json!({"id": "worker"}));
    req.resource_secrets = SecretMap::new(object(json!({
        "spec": {"function": {"variables": {"TABLE": "orders", "DB_PASSWORD": "pw-1"}},
                 "args": ["--token", "tok-1"]},
        "dependencies": {"db": {"host": "db.internal", "password": "pw-1"}},
        "whole": "w-1",
        "unlocated": {"a": "u-1", "b": "plain"},
    })));
    let path = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    req.resource_secret_paths = vec![
        path(&["spec", "function", "variables", "DB_PASSWORD"]),
        path(&["spec", "args", "1"]),
        path(&["dependencies", "db", "password"]),
        path(&["whole"]),
        path(&["unlocated", "missing"]),
    ];
    let (plain, secrets) = split_resource(&req);
    assert_eq!(
        Value::Object(plain),
        json!({
            "id": "worker",
            "spec": {"function": {"variables": {"TABLE": "orders"}}, "args": ["--token", null]},
            "dependencies": {"db": {"host": "db.internal"}},
        })
    );
    // A secret the paths do not locate stays a secret as a whole.
    assert_eq!(
        Value::Object(secrets),
        json!({
            "spec": {"function": {"variables": {"DB_PASSWORD": "pw-1"}}, "args": {"1": "tok-1"}},
            "dependencies": {"db": {"password": "pw-1"}},
            "whole": "w-1",
            "unlocated": {"a": "u-1", "b": "plain"},
        })
    );
}

#[test]
fn a_secret_inside_another_moves_with_it() {
    let mut req = request(json!({"script": ""}), json!({}));
    req.resource = object(json!({"id": "worker"}));
    req.resource_secrets = SecretMap::new(object(json!({
        "dependencies": {"db": {"host": "db.internal", "auth": {"password": "pw-1"}}},
    })));
    let path = |p: &[&str]| p.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    // The inner secret first, so moving the outer one cannot overwrite it.
    req.resource_secret_paths = vec![
        path(&["dependencies", "db", "auth", "password"]),
        path(&["dependencies", "db", "auth"]),
    ];
    let (plain, secrets) = split_resource(&req);
    assert_eq!(
        Value::Object(plain),
        json!({"id": "worker", "dependencies": {"db": {"host": "db.internal"}}})
    );
    assert_eq!(
        Value::Object(secrets),
        json!({"dependencies": {"db": {"auth": {"password": "pw-1"}}}})
    );
}

#[tokio::test]
async fn the_plain_variable_file_has_no_secret_input() {
    let root = tempfile::tempdir().unwrap();
    let d = driver(root.path());
    let mut req = request(json!({"script": ""}), json!({}));
    req.resource = object(json!({"id": "worker"}));
    req.resource_secrets = SecretMap::new(object(json!({
        "spec": {"function": {"variables": {"TABLE": "orders", "DB_PASSWORD": "pw-1"}}},
    })));
    req.resource_secret_paths = vec![
        ["spec", "function", "variables", "DB_PASSWORD"]
            .map(String::from)
            .to_vec(),
    ];
    let mut run = d.start(&req).await.unwrap();
    d.prepare(&mut run, &req, SourceAt::Current).await.unwrap();
    let read = |name: &str| -> Value {
        serde_json::from_slice(&std::fs::read(run.dir.join(name)).unwrap()).unwrap()
    };
    let plain = read("values.tfvars.json");
    assert_eq!(
        plain["resource"],
        json!({"id": "worker", "spec": {"function": {"variables": {"TABLE": "orders"}}}})
    );
    assert!(!plain.to_string().contains("pw-1"), "{plain}");
    assert_eq!(
        read("secrets.tfvars.json")["resource_secrets"],
        json!({"spec": {"function": {"variables": {"DB_PASSWORD": "pw-1"}}}})
    );
}

/// A module that reads the resource's inputs and creates nothing.
const RESOURCE_MODULE: &str = r#"
variable "resource" {
  type     = any
  default  = {}
  nullable = false
}
variable "resource_secrets" {
  type      = any
  default   = {}
  nullable  = false
  sensitive = true
}
output "key" { value = try(var.resource.primaryKey.name, "id") }
output "token" {
  value     = try(var.resource_secrets.token, null)
  sensitive = true
}
"#;

#[tokio::test]
#[ignore = "needs OpenTofu"]
async fn opentofu_modules_read_the_resource_inputs() {
    let root = tempfile::tempdir().unwrap();
    let d = driver(root.path());
    let mut req = request(json!({"script": RESOURCE_MODULE}), json!({}));
    req.resource = object(json!({"primaryKey": {"name": "orderId", "type": "S"}}));
    req.resource_secrets = SecretMap::new(object(json!({"token": "resource-secret-1"})));
    let lines = capture(&mut req);
    let PutOutcome::Ready { outputs, cookie } = d.put(&req).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.values["key"], "orderId");
    assert_eq!(outputs.secrets.expose()["token"], "resource-secret-1");
    let log = lines.lock().unwrap().join("\n");
    assert!(!log.contains("resource-secret-1"), "{log}");

    // A resource without inputs leaves the variables at their defaults, and a
    // module that declares neither variable still runs with inputs given.
    let mut bare = request(json!({"script": RESOURCE_MODULE}), json!({}));
    bare.cookie = cookie;
    let PutOutcome::Ready { outputs, .. } = d.put(&bare).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.values["key"], "id");
    assert!(!outputs.secrets.expose().contains_key("token"));
    let mut other = request(json!({"script": "output \"n\" { value = 1 }"}), json!({}));
    other.guresid = Guresid::new("fedcba9876543210fedcba9876543210").unwrap();
    other.resource = object(json!({"primaryKey": {"name": "orderId"}}));
    let PutOutcome::Ready { outputs, .. } = d.put(&other).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.values["n"], 1);
}

/// Planned actions on resources, leaving out outputs.
fn resources(actions: &[(Action, String)]) -> Vec<(Action, String)> {
    actions
        .iter()
        .filter(|(_, address)| !address.starts_with("output."))
        .cloned()
        .collect()
}

fn password_request(length: u32) -> DriverRequest {
    request(
        json!({"script": PASSWORD_MODULE, "variables": {"length": length}}),
        json!({"variables": {"label": "label-secret-value"}}),
    )
}

#[tokio::test]
#[ignore = "needs OpenTofu and the provider registry"]
async fn opentofu_runs_a_local_module_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let d = driver(root.path());

    // First apply: a sensitive output becomes a secret and stays out of logs.
    let mut req = password_request(16);
    let lines = capture(&mut req);
    let PutOutcome::Ready { outputs, cookie } = d.put(&req).await.unwrap() else {
        panic!("not ready");
    };
    let password = outputs.secrets.expose()["password"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(password.len(), 16);
    assert_eq!(outputs.values["length"], 16);
    assert_eq!(outputs.values["label_length"], 18);
    assert!(!outputs.values.contains_key("password"));
    let log = lines.lock().unwrap().join("\n");
    assert!(log.contains("$ tofu apply"), "{log}");
    assert!(!log.contains(&password), "{log}");
    let escaped = serde_json::to_string(&password).unwrap();
    let escaped = &escaped[1..escaped.len() - 1];
    assert!(!log.contains(escaped), "{log}");
    assert!(!log.contains("label-secret-value"), "{log}");

    // Observing reads the state.
    let Observation::Ready { outputs: seen, .. } = d.get(&req).await.unwrap() else {
        panic!("not observed");
    };
    assert_eq!(seen.secrets.expose()["password"], password.as_str());

    // Unchanged inputs plan no changes, and applying again keeps the password.
    let mut again = password_request(16);
    again.cookie = cookie.clone();
    assert_eq!(d.plan(&again).await.unwrap(), Plan::NoEffects);
    let PutOutcome::Ready { outputs, .. } = d.put(&again).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.secrets.expose()["password"], password.as_str());

    // A change that replaces the password needs a permit for exactly that plan.
    let mut longer = password_request(20);
    longer.cookie = cookie.clone();
    let Plan::Actions {
        actions,
        fingerprint,
    } = d.plan(&longer).await.unwrap()
    else {
        panic!("no actions");
    };
    assert_eq!(
        resources(&actions),
        [(Action::Replace, "random_password.p".to_owned())]
    );
    let (code, message) = invalid(d.put(&longer).await.unwrap_err());
    assert_eq!(code, "TOFU-002");
    assert!(message.contains("random_password.p"), "{message}");
    longer.permit = Some("not-this-plan".into());
    assert!(d.put(&longer).await.is_err());
    longer.permit = Some(fingerprint);
    let PutOutcome::Ready { outputs, cookie } = d.put(&longer).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(
        outputs.secrets.expose()["password"].as_str().unwrap().len(),
        20
    );

    // Deleting destroys with its own permit, then removes the state.
    let mut del = password_request(20);
    del.cookie = cookie;
    let (code, _) = invalid(d.delete(&del).await.unwrap_err());
    assert_eq!(code, "TOFU-002");
    let Plan::Actions {
        actions,
        fingerprint,
    } = d.plan_delete(&del).await.unwrap()
    else {
        panic!("no destroy actions");
    };
    assert_eq!(
        resources(&actions),
        [(Action::Delete, "random_password.p".to_owned())]
    );
    del.permit = Some(fingerprint);
    assert_eq!(d.delete(&del).await.unwrap(), DeleteOutcome::Deleted);
    assert!(
        !root
            .path()
            .join("state/0123456789abcdef0123456789abcdef/terraform.tfstate")
            .exists()
    );
    assert_eq!(d.get(&del).await.unwrap(), Observation::Absent);
    // Repeating it is a no-op.
    assert_eq!(d.delete(&del).await.unwrap(), DeleteOutcome::Deleted);
}

#[tokio::test]
#[ignore = "needs OpenTofu and the provider registry"]
async fn opentofu_failures_are_reported_redacted() {
    let root = tempfile::tempdir().unwrap();
    let d = driver(root.path());
    let script = r#"
variable "token" {
  type = string
  sensitive = true
}
output "n" { value = tonumber(nonsensitive(var.token)) }
"#;
    let mut req = request(
        json!({"script": script, "append_logs_to_error": true}),
        json!({"variables": {"token": "hunter2-not-a-number"}}),
    );
    let lines = capture(&mut req);
    let (code, message) = invalid(d.put(&req).await.unwrap_err());
    assert_eq!(code, "TOFU-001");
    assert!(message.starts_with("tofu: plan failed:"), "{message}");
    assert!(message.contains("Full log:"), "{message}");
    assert!(!message.contains("hunter2-not-a-number"), "{message}");
    let log = lines.lock().unwrap().join("\n");
    assert!(log.contains("$ tofu plan"), "{log}");
    assert!(!log.contains("hunter2-not-a-number"), "{log}");
    // Nothing was applied, so there is no state to keep.
    assert_eq!(d.get(&req).await.unwrap(), Observation::Absent);
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
#[ignore = "needs OpenTofu and the provider registry"]
async fn opentofu_applies_a_git_module_and_deletes_the_retained_commit() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join("modules/pw")).unwrap();
    std::fs::write(repo.join("modules/pw/main.tf"), PASSWORD_MODULE).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "module"]);
    let url = format!("file://{}", repo.display());
    let d = driver(root.path());
    let req = |cookie| {
        let mut r = request(
            json!({"source": {"url": url, "rev": "main", "path": "modules/pw"},
                   "variables": {"length": 12},
                   // A backend in the script is overridden like one in the module.
                   "script": "terraform {\n  backend \"local\" { path = \"script.tfstate\" }\n}\n"}),
            json!({"variables": {"label": "label-secret-value"}}),
        );
        r.cookie = cookie;
        r
    };
    let PutOutcome::Ready { outputs, cookie } = d.put(&req(None)).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.values["length"], 12);
    let retained: Retained = serde_json::from_str(&cookie.as_ref().unwrap().data).unwrap();
    assert_eq!(retained.source.as_ref().unwrap().commit.len(), 40);
    assert!(
        root.path()
            .join("state/0123456789abcdef0123456789abcdef/terraform.tfstate")
            .exists()
    );

    // The branch moves on to a broken module; deletion still uses the commit
    // that was applied.
    std::fs::write(repo.join("modules/pw/main.tf"), "this is not HCL {").unwrap();
    git(&repo, &["commit", "-q", "-am", "break"]);
    // A replay pinned to what was applied still applies, with the lock file
    // read-only.
    let pin = outputs.footprint.pin.clone().unwrap();
    assert!(
        pin["lock"].as_str().unwrap().contains("hashicorp/random"),
        "{pin}"
    );
    let mut replay = req(cookie.clone());
    replay.pin = Some(pin.clone());
    let PutOutcome::Ready { outputs, cookie } = d.put(&replay).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.footprint.pin, Some(pin));
    let mut del = req(cookie);
    let Plan::Actions { fingerprint, .. } = d.plan_delete(&del).await.unwrap() else {
        panic!("no destroy actions");
    };
    del.permit = Some(fingerprint);
    assert_eq!(d.delete(&del).await.unwrap(), DeleteOutcome::Deleted);
}

#[tokio::test]
async fn fake_run_exports_the_default_aws_environment() {
    // The `Default AWS environment` scenario: no `credentials_config`.
    let root = tempfile::tempdir().unwrap();
    let fake = root.path().join("fake-tofu");
    let record = root.path().join("record.log");
    // This fake has no environment entry for its log path, so it is fixed.
    std::fs::write(
        &fake,
        FAKE_TOFU.replace("\"$FAKE_TOFU_LOG\"", &format!("'{}'", record.display())),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut d = driver(root.path());
    d.config.tofu = fake;
    let mut req = request(json!({"script": "terraform {}"}), json!({}));
    req.credentials = Some(SecretMap::new(object(json!({
        "AccessKeyId": "ASIAEXAMPLE",
        "SecretAccessKey": "aws-secret-2",
        "SessionToken": "session-3",
        "Region": "eu-north-1",
    }))));
    d.put(&req).await.unwrap();
    let record = std::fs::read_to_string(&record).unwrap();
    for want in [
        "AWS_ACCESS_KEY_ID=ASIAEXAMPLE",
        "AWS_SECRET_ACCESS_KEY=aws-secret-2",
        "AWS_SESSION_TOKEN=session-3",
        "AWS_REGION=eu-north-1",
    ] {
        assert!(record.contains(want), "{want}");
    }
}

#[tokio::test]
async fn s3_state_uses_a_workspace_per_resource_and_its_own_credentials() {
    let root = tempfile::tempdir().unwrap();
    let mut d = fake_driver(root.path(), str::to_owned);
    let mut s3 = S3State::new("state-bucket", "eu-north-1");
    s3.credentials = Some(S3Credentials {
        access_key_id: "AKIASTATE".into(),
        secret_access_key: "state-secret-1".into(),
        session_token: None,
    });
    d.config.state = StateBackend::S3(Box::new(s3));
    let mut req = request(json!({"script": "terraform {}"}), json!({}));
    req.credentials = Some(SecretMap::new(object(json!({
        "AccessKeyId": "AKIARESOURCE",
        "SecretAccessKey": "resource-secret-2",
    }))));
    let lines = capture(&mut req);
    let PutOutcome::Ready { cookie, .. } = d.put(&req).await.unwrap() else {
        panic!("not ready");
    };
    let record = std::fs::read_to_string(root.path().join("record.log")).unwrap();
    assert!(record.contains(r#"backend "s3""#), "{record}");
    assert!(record.contains(r#"workspace_key_prefix = "openporch""#));
    assert!(record.contains("use_lockfile = true"));
    let calls: Vec<&str> = record
        .lines()
        .filter_map(|l| l.strip_prefix("ARGS "))
        .collect();
    // The state check lists workspaces without creating one; the run then
    // selects or creates the resource's own.
    assert!(calls[1].starts_with("workspace list"), "{calls:?}");
    assert!(
        calls
            .iter()
            .any(|c| c
                .starts_with("workspace select -or-create=true 0123456789abcdef0123456789abcdef")),
        "{calls:?}"
    );
    assert!(record.contains("TF_WORKSPACE=0123456789abcdef0123456789abcdef"));
    // Backend credentials go in a file, the resource's in the environment.
    assert!(calls.iter().all(|c| !c.contains("state-secret-1")));
    assert!(calls[0].contains("-backend-config="), "{calls:?}");
    assert!(record.contains("AWS_ACCESS_KEY_ID=AKIARESOURCE"));
    assert!(!record.contains("AWS_ACCESS_KEY_ID=AKIASTATE"));
    let log = lines.lock().unwrap().join("\n");
    assert!(!log.contains("state-secret-1"), "{log}");
    // The backend is kept with the resource.
    let retained: Retained = serde_json::from_str(&cookie.unwrap().data).unwrap();
    assert_eq!(
        retained.backend,
        Some(BackendId::S3 {
            bucket: "state-bucket".into(),
            key_prefix: "openporch".into(),
            endpoint: None,
        })
    );
}

#[tokio::test]
async fn a_changed_state_backend_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut d = fake_driver(root.path(), str::to_owned);
    let mut req = request(json!({"script": "terraform {}"}), json!({}));
    let PutOutcome::Ready { cookie, .. } = d.put(&req).await.unwrap() else {
        panic!("not ready");
    };
    req.cookie = cookie;
    d.config.state = StateBackend::S3(Box::new(S3State::new("state-bucket", "eu-north-1")));
    for err in [
        d.put(&req).await.unwrap_err(),
        d.delete(&req).await.unwrap_err(),
    ] {
        let (code, message) = invalid(err);
        assert_eq!(code, "TOFU-004");
        assert!(
            message.contains("in local state") && message.contains("s3://state-bucket/openporch"),
            "{message}"
        );
    }
}

const LOCAL_BACKEND_MODULE: &str = r#"
terraform {
  backend "local" { path = "module-chosen.tfstate" }
}
resource "terraform_data" "x" { input = var.value }
variable "value" { type = string }
output "value" { value = terraform_data.x.output }
"#;

/// Applies [`LOCAL_BACKEND_MODULE`] twice with `d`, checks the second run
/// plans nothing, then deletes it.
async fn apply_twice_then_delete(d: &OpenTofu) {
    let req = |cookie| {
        let mut r = request(
            json!({"script": LOCAL_BACKEND_MODULE, "variables": {"value": "v1"}}),
            json!({}),
        );
        r.cookie = cookie;
        r
    };
    let PutOutcome::Ready { outputs, cookie } = d.put(&req(None)).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.values["value"], "v1");
    // The second run reads the state the first wrote.
    assert_eq!(d.plan(&req(cookie.clone())).await.unwrap(), Plan::NoEffects);
    let PutOutcome::Ready { outputs, cookie } = d.put(&req(cookie)).await.unwrap() else {
        panic!("not ready");
    };
    assert_eq!(outputs.values["value"], "v1");
    let Observation::Ready { outputs, .. } = d.get(&req(cookie.clone())).await.unwrap() else {
        panic!("not observed");
    };
    assert_eq!(outputs.values["value"], "v1");

    let mut del = req(cookie);
    let Plan::Actions { fingerprint, .. } = d.plan_delete(&del).await.unwrap() else {
        panic!("no destroy actions");
    };
    del.permit = Some(fingerprint);
    assert_eq!(d.delete(&del).await.unwrap(), DeleteOutcome::Deleted);
    assert_eq!(d.get(&del).await.unwrap(), Observation::Absent);
    assert_eq!(d.delete(&del).await.unwrap(), DeleteOutcome::Deleted);
}

#[tokio::test]
#[ignore = "needs OpenTofu"]
async fn opentofu_local_state_ignores_the_module_backend() {
    let root = tempfile::tempdir().unwrap();
    let d = driver(root.path());
    let req = request(
        json!({"script": LOCAL_BACKEND_MODULE, "variables": {"value": "v1"}}),
        json!({}),
    );
    d.put(&req).await.unwrap();
    let state = root
        .path()
        .join("state/0123456789abcdef0123456789abcdef/terraform.tfstate");
    let mode = std::fs::metadata(&state).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "state is owner-only");
    std::fs::remove_dir_all(root.path().join("state")).unwrap();
    apply_twice_then_delete(&driver(root.path())).await;
}

/// Opt-in: set `OPENPORCH_TEST_S3_BUCKET` (and `OPENPORCH_TEST_S3_REGION`,
/// default `us-east-1`; `OPENPORCH_TEST_S3_ENDPOINT` for an S3-compatible
/// service) with the backend's keys in `AWS_ACCESS_KEY_ID` and
/// `AWS_SECRET_ACCESS_KEY`.
#[tokio::test]
#[ignore = "needs OpenTofu and an S3 bucket"]
async fn opentofu_s3_state_applies_twice() {
    let Ok(bucket) = std::env::var("OPENPORCH_TEST_S3_BUCKET") else {
        eprintln!("OPENPORCH_TEST_S3_BUCKET is not set; skipping");
        return;
    };
    let region = std::env::var("OPENPORCH_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    let mut s3 = S3State::new(bucket, region);
    s3.endpoint = std::env::var("OPENPORCH_TEST_S3_ENDPOINT").ok();
    // A prefix of its own, so test runs never share state.
    s3.key_prefix = format!("openporch-test-{:016x}", rand::random::<u64>());
    let root = tempfile::tempdir().unwrap();
    let mut d = driver(root.path());
    d.config.state = StateBackend::S3(Box::new(s3));
    // A preview leaves an empty workspace, which is not the resource's state.
    let mut preview = request(
        json!({"script": LOCAL_BACKEND_MODULE, "variables": {"value": "v1"}}),
        json!({}),
    );
    preview.guresid = Guresid::new("fedcba9876543210fedcba9876543210").unwrap();
    assert!(matches!(
        d.plan(&preview).await.unwrap(),
        Plan::Actions { .. }
    ));
    assert_eq!(d.get(&preview).await.unwrap(), Observation::Absent);
    assert_eq!(d.delete(&preview).await.unwrap(), DeleteOutcome::Deleted);
    apply_twice_then_delete(&d).await;
    // Only recovery metadata stays local.
    let local = root.path().join("state/0123456789abcdef0123456789abcdef");
    assert!(!local.join("terraform.tfstate").exists());
}

#[tokio::test]
async fn an_empty_s3_workspace_from_a_preview_is_not_state() {
    let root = tempfile::tempdir().unwrap();
    let mut d = fake_driver(root.path(), |s| {
        s.replace(
            "case \"$1\" in\n",
            "case \"$1\" in\n  workspace) if [ \"$2\" = list ]; then printf '* default\\n  0123456789abcdef0123456789abcdef\\n'; fi ;;\n  state) echo '{\"version\":4,\"serial\":1,\"lineage\":\"l\",\"outputs\":{},\"resources\":[]}' ;;\n",
        )
    });
    let mut s3 = S3State::new("state-bucket", "eu-north-1");
    s3.credentials = Some(S3Credentials {
        access_key_id: "AKIASTATE".into(),
        secret_access_key: "state-secret-1".into(),
        session_token: None,
    });
    d.config.state = StateBackend::S3(Box::new(s3));
    let req = request(json!({"script": "terraform {}"}), json!({}));
    assert_eq!(d.get(&req).await.unwrap(), Observation::Absent);
    assert_eq!(d.plan_delete(&req).await.unwrap(), Plan::NoEffects);
    // Deleting removes the empty workspace without needing a retained script.
    assert_eq!(d.delete(&req).await.unwrap(), DeleteOutcome::Deleted);
    let record = std::fs::read_to_string(root.path().join("record.log")).unwrap();
    assert!(
        record.contains("ARGS workspace delete 0123456789abcdef0123456789abcdef"),
        "{record}"
    );
}

#[test]
fn credentials_config_cannot_redirect_aws_endpoints() {
    for name in [
        "AWS_ENDPOINT_URL",
        "AWS_ENDPOINT_URL_S3",
        "AWS_CA_BUNDLE",
        "AWS_PROFILE",
        "TF_WORKSPACE",
        "AWS_EC2_METADATA_DISABLED",
    ] {
        assert!(check_env_name(name).is_err(), "{name}");
    }
    assert!(check_env_name("AWS_ACCESS_KEY_ID").is_ok());
}

#[tokio::test]
async fn deleting_a_script_resource_never_fetches_a_newer_source() {
    let root = tempfile::tempdir().unwrap();
    let d = fake_driver(root.path(), str::to_owned);
    let req = request(json!({"script": "terraform {}"}), json!({}));
    let PutOutcome::Ready { cookie, .. } = d.put(&req).await.unwrap() else {
        panic!("not ready");
    };
    let state = root.path().join("state/0123456789abcdef0123456789abcdef");
    std::fs::write(state.join("terraform.tfstate"), r#"{"resources":[]}"#).unwrap();
    // The definition has since gained a source that cannot be fetched.
    let mut del = request(
        json!({"source": {"url": "https://git.invalid/m"}, "script": "changed"}),
        json!({}),
    );
    del.cookie = cookie;
    assert_eq!(d.delete(&del).await.unwrap(), DeleteOutcome::Deleted);
}

// --- faults (4.8) -----------------------------------------------------------------

/// Whether `pid` is a live process; zombies count as gone.
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
        .ok()
        .and_then(|s| s.rsplit(')').next().map(|r| r.trim().to_owned()))
        .is_some_and(|rest| !rest.starts_with('Z'))
}

/// A fake `tofu` whose `init` hangs while `<root>/hang` exists, with a child
/// process, after running `trap` (a shell `trap` line for the parent) and
/// `child_trap` (one for the child).
fn hanging_driver(root: &Path, trap: &str, child_trap: &str) -> OpenTofu {
    let r = root.display();
    let hang = format!(
        "init) if [ -e '{r}/hang' ]; then echo $$ > '{r}/tofu.pid'; {trap}\n    \
         sh -c '{child_trap} echo $$ > \"{r}/child.pid\"; exec sleep 300' &\n    \
         wait; echo finished >> '{r}/events'; fi ;;"
    );
    let mut d = fake_driver(root, |s| {
        s.replacen(
            "case \"$1\" in\n",
            &format!("case \"$1\" in\n  {hang}\n"),
            1,
        )
    });
    d.config.stop_grace = std::time::Duration::from_secs(1);
    d
}

async fn assert_timeout_reaps(root: &Path, d: &OpenTofu) {
    std::fs::write(root.join("hang"), "").unwrap();
    let req = request(json!({"script": "terraform {}"}), json!({}));
    let err = crate::with_timeout(std::time::Duration::from_millis(800), d.put(&req))
        .await
        .unwrap_err();
    assert!(matches!(err, DriverError::Timeout(_)), "{err:?}");
    let tofu = std::fs::read_to_string(root.join("tofu.pid")).unwrap();
    let child = std::fs::read_to_string(root.join("child.pid")).unwrap();
    std::fs::remove_file(root.join("hang")).unwrap();
    // The next call for the resource waits until the abandoned run's whole
    // process group is gone and its directory removed.
    d.put(&req).await.unwrap();
    assert!(!alive(&tofu), "tofu {tofu} still runs");
    assert!(!alive(&child), "its child {child} still runs");
    let runs = root.join("runs/0123456789abcdef0123456789abcdef");
    assert_eq!(std::fs::read_dir(runs).unwrap().count(), 0);
}

#[tokio::test]
async fn a_timed_out_run_is_interrupted_and_reaped_before_the_next() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path().display().to_string();
    let d = hanging_driver(
        root.path(),
        &format!("trap 'echo interrupted >> \"{r}/events\"; exit 130' INT;"),
        "",
    );
    assert_timeout_reaps(root.path(), &d).await;
    // OpenTofu got SIGINT, which lets it write state and release its lock.
    let events = std::fs::read_to_string(root.path().join("events")).unwrap();
    assert!(events.contains("interrupted"), "{events}");
}

#[tokio::test]
async fn a_run_that_ignores_interrupts_is_killed_after_the_grace_period() {
    let root = tempfile::tempdir().unwrap();
    let d = hanging_driver(root.path(), "trap '' INT;", "trap \"\" INT;");
    assert_timeout_reaps(root.path(), &d).await;
}

#[tokio::test]
async fn a_held_state_lock_is_a_conflict_and_never_bypassed() {
    let root = tempfile::tempdir().unwrap();
    let d = fake_driver(root.path(), |s| {
        s.replace(
            "case \"$1\" in\n",
            "case \"$1\" in\n  plan) printf 'Error: Error acquiring the state lock\\n\\nLock Info:\\n  ID:        5c2d-77ab\\n' >&2; exit 1 ;;\n",
        )
    });
    let req = request(json!({"script": "terraform {}"}), json!({}));
    match d.put(&req).await.unwrap_err() {
        DriverError::Conflict(m) => {
            assert!(m.contains("tofu force-unlock 5c2d-77ab"), "{m}");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    let record = std::fs::read_to_string(root.path().join("record.log")).unwrap();
    assert!(record.contains("-lock-timeout=30000ms"), "{record}");
    assert!(!record.contains("force-unlock") && !record.contains("-lock=false"));
}

#[tokio::test]
async fn a_lock_held_at_apply_time_leaves_no_record_of_an_apply() {
    let root = tempfile::tempdir().unwrap();
    let locked = root.path().join("locked");
    let d = fake_driver(root.path(), |s| {
        s.replace(
            "case \"$1\" in\n",
            &format!(
                "case \"$1\" in\n  apply) if [ -e '{}' ]; then printf 'Error: Error acquiring \
                 the state lock\\n\\nLock Info:\\n  ID:        5c2d-77ab\\n' >&2; exit 1; fi ;;\n",
                locked.display()
            ),
        )
    });
    let req = request(json!({"script": "terraform {}"}), json!({}));
    std::fs::write(&locked, "").unwrap();
    assert!(matches!(
        d.put(&req).await.unwrap_err(),
        DriverError::Conflict(_)
    ));
    let record = root.path().join("state/0123456789abcdef0123456789abcdef");
    assert!(!record.join("retained.json").exists());
    // Nothing ran, so missing state is not lost state.
    std::fs::remove_file(&locked).unwrap();
    d.put(&req).await.unwrap();
}

#[tokio::test]
async fn state_lost_after_an_apply_started_is_never_replaced() {
    // `no accidental fresh state after state loss`, without a cookie: the
    // first apply was cut off before its outcome was recorded.
    let root = tempfile::tempdir().unwrap();
    let d = fake_driver(root.path(), str::to_owned);
    let req = request(json!({"script": "terraform {}"}), json!({}));
    d.put(&req).await.unwrap();
    // The fake writes no state, as if it were lost.
    match d.put(&req).await.unwrap_err() {
        DriverError::Uncertain(m) => assert!(m.contains("needs recovery"), "{m}"),
        other => panic!("expected uncertain, got {other:?}"),
    }
}

#[tokio::test]
async fn cookies_are_checked_for_compatibility() {
    let root = tempfile::tempdir().unwrap();
    let d = fake_driver(root.path(), str::to_owned);
    let mut req = request(json!({"script": "terraform {}"}), json!({}));
    for (version, data) in [(2, "{}"), (1, "not json")] {
        req.cookie = Some(Cookie::new(version, data).unwrap());
        assert!(
            matches!(d.put(&req).await, Err(DriverError::Uncertain(_))),
            "{version} {data}"
        );
    }
    // A cookie from before the backend was recorded still reads.
    let state = root.path().join("state/0123456789abcdef0123456789abcdef");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("terraform.tfstate"), r#"{"resources":[]}"#).unwrap();
    req.cookie = Some(Cookie::new(1, r#"{"source":null}"#).unwrap());
    d.put(&req).await.unwrap();
}

#[tokio::test]
async fn symlinks_cannot_lead_out_of_a_git_module() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(repo.join("module")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(repo.join("module/main.tf"), "").unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("module/escape")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("escape-module")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "module"]);
    let url = format!("file://{}", repo.display());
    let d = fake_driver(root.path(), str::to_owned);
    for (values, want) in [
        (
            json!({"source": {"url": url, "rev": "main", "path": "module"},
                   "files": {"escape/x.tf": "resource \"x\" \"y\" {}"}}),
            "leaves the module through a symlink",
        ),
        (
            json!({"source": {"url": url, "rev": "main", "path": "escape-module"}}),
            "leaves the module through a symlink",
        ),
    ] {
        let (_, message) = invalid(d.put(&request(values, json!({}))).await.unwrap_err());
        assert!(message.contains(want), "{message}");
    }
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}

#[tokio::test]
#[ignore = "needs OpenTofu"]
async fn opentofu_a_timed_out_apply_leaves_usable_state() {
    let root = tempfile::tempdir().unwrap();
    let mut d = driver(root.path());
    d.config.stop_grace = std::time::Duration::from_secs(30);
    let script = r#"
variable "seconds" { type = number }
resource "terraform_data" "slow" {
  input = var.seconds
  provisioner "local-exec" { command = "sleep ${var.seconds}" }
}
"#;
    let req = |seconds: u32| {
        request(
            json!({"script": script, "variables": {"seconds": seconds}}),
            json!({}),
        )
    };
    let err = crate::with_timeout(std::time::Duration::from_secs(8), d.put(&req(120)))
        .await
        .unwrap_err();
    assert!(matches!(err, DriverError::Timeout(_)), "{err:?}");
    // The interrupted apply wrote state and released its lock, so the next
    // run plans against it: the tainted object is replaced, with a permit.
    let mut next = req(0);
    let Plan::Actions {
        actions,
        fingerprint,
    } = d.plan(&next).await.unwrap()
    else {
        panic!("no actions");
    };
    assert_eq!(
        actions,
        [(Action::Replace, "terraform_data.slow".to_owned())]
    );
    next.permit = Some(fingerprint);
    d.put(&next).await.unwrap();
    let state = root.path().join("state/0123456789abcdef0123456789abcdef");
    assert!(!state.join(".terraform.tfstate.lock.info").exists());
}

#[tokio::test]
async fn a_process_left_holding_the_output_does_not_hold_up_the_run() {
    let root = tempfile::tempdir().unwrap();
    let pid = root.path().join("lingering.pid");
    let d = fake_driver(root.path(), |s| {
        s.replacen(
            "case \"$1\" in\n",
            &format!(
                "case \"$1\" in\n  init) sleep 300 & echo $! > '{}' ;;\n",
                pid.display()
            ),
            1,
        )
    });
    let req = request(json!({"script": "terraform {}"}), json!({}));
    crate::with_timeout(std::time::Duration::from_secs(10), d.put(&req))
        .await
        .unwrap();
    let pid = std::fs::read_to_string(pid).unwrap();
    assert!(!alive(&pid), "{pid} still runs");
}

#[tokio::test]
async fn a_pinned_run_applies_the_commit_and_lock_file_it_names() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir_all(repo.join("modules/pw")).unwrap();
    std::fs::write(repo.join("modules/pw/main.tf"), "# first\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    // `init` writes a lock file when the module has none and records the lock
    // file and module it ran with.
    let record = root.path().join("record.log");
    let d = fake_driver(root.path(), |script| {
        let log = format!("'{}'", record.display());
        script.replace(
            "  init) ",
            &format!(
                "  init) grep -q yanked .terraform.lock.hcl 2>/dev/null && {{ echo 'Error: provider yanked' >&2; exit 1; }}; \
                 [ -f .terraform.lock.hcl ] || echo 'generated lock' > .terraform.lock.hcl; \
                 echo \"LOCK $(cat .terraform.lock.hcl)\" >> {log}; \
                 echo \"MAIN $(head -1 main.tf)\" >> {log}; "
            ),
        )
    });
    let url = format!("file://{}", repo.display());
    let values = json!({"source": {"url": url, "rev": "main", "path": "modules/pw"}});
    let state = root.path().join("state/0123456789abcdef0123456789abcdef");
    let put = async |pin: Option<Value>| {
        std::fs::write(&record, "").unwrap();
        let mut req = request(values.clone(), json!({}));
        req.pin = pin;
        let PutOutcome::Ready { outputs, .. } = d.put(&req).await.unwrap() else {
            panic!("not ready");
        };
        let log = std::fs::read_to_string(&record).unwrap();
        let line = |prefix: &str| {
            log.lines()
                .find_map(|l| l.strip_prefix(prefix))
                .unwrap()
                .to_owned()
        };
        // The fake applies nothing; the state shows an applied resource.
        std::fs::write(state.join("terraform.tfstate"), r#"{"resources":[]}"#).unwrap();
        (
            outputs.footprint.pin.unwrap(),
            line("ARGS init"),
            line("LOCK "),
            line("MAIN "),
        )
    };

    let (pin, init, lock, main) = put(None).await;
    assert_eq!(pin["source"]["url"], url.as_str());
    assert_eq!(pin["source"]["path"], "modules/pw");
    assert_eq!(pin["source"]["commit"].as_str().unwrap().len(), 40);
    assert_eq!(pin["lock"], "generated lock\n");
    assert!(!init.contains("-lockfile"), "{init}");
    assert_eq!(
        (lock.as_str(), main.as_str()),
        ("generated lock", "# first")
    );

    // The branch moves on and the resource's providers are upgraded.
    std::fs::write(repo.join("modules/pw/main.tf"), "# second\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "second"]);
    std::fs::write(state.join(LOCK_FILE), "upgraded lock\n").unwrap();
    let (_, _, lock, main) = put(None).await;
    assert_eq!(
        (lock.as_str(), main.as_str()),
        ("upgraded lock", "# second")
    );

    // Pinned, the run uses the first commit and its lock file, read-only.
    let (repinned, init, lock, main) = put(Some(pin.clone())).await;
    assert_eq!(
        (lock.as_str(), main.as_str()),
        ("generated lock", "# first")
    );
    assert!(init.contains("-lockfile=readonly"), "{init}");
    assert_eq!(repinned, pin);
    // Checking the pin before a replay fetches it and installs its providers
    // with its lock file, without a backend, and runs nothing else.
    std::fs::write(&record, "").unwrap();
    let mut req = request(values.clone(), json!({}));
    req.pin = Some(pin.clone());
    d.check_pin(&req).await.unwrap();
    let log = std::fs::read_to_string(&record).unwrap();
    let args: Vec<&str> = log.lines().filter(|l| l.starts_with("ARGS")).collect();
    assert_eq!(
        args,
        ["ARGS init -input=false -backend=false -lockfile=readonly -no-color"],
        "{log}"
    );
    assert!(log.contains("LOCK generated lock\nMAIN # first"), "{log}");
    // A provider the lock file selects is gone: the check refuses it.
    let mut yanked = pin.clone();
    yanked["lock"] = json!("yanked lock\n");
    req.pin = Some(yanked);
    let (code, message) = invalid(d.check_pin(&req).await.unwrap_err());
    assert_eq!(code, "TOFU-003", "{message}");
    assert!(message.contains("can no longer be installed"), "{message}");
    assert!(message.contains("provider yanked"), "{message}");
    // With the cloud account's credentials, `init` gets them, as for a run.
    let mut with_creds = req.clone();
    with_creds.pin = Some(pin.clone());
    with_creds.credentials = Some(SecretMap::new(
        json!({"AccessKeyId": "AKIACHECK", "SecretAccessKey": "check-secret"})
            .as_object()
            .unwrap()
            .clone(),
    ));
    std::fs::write(&record, "").unwrap();
    d.check_pin(&with_creds).await.unwrap();
    let log = std::fs::read_to_string(&record).unwrap();
    assert!(log.contains("AWS_ACCESS_KEY_ID=AKIACHECK"), "{log}");
    // Without a recorded lock file, `init` still installs the modules.
    let mut unlocked = pin.clone();
    unlocked["lock"] = Value::Null;
    req.pin = Some(unlocked);
    std::fs::write(&record, "").unwrap();
    d.check_pin(&req).await.unwrap();
    let log = std::fs::read_to_string(&record).unwrap();
    assert!(
        log.contains("ARGS init -input=false -backend=false -no-color"),
        "{log}"
    );
    req.pin = Some(pin.clone());
    // A source the planner could not resolve before the deployment ran
    // cannot be checked, so the replay is refused.
    let mut unknown = request(
        json!({"source": {"url": "(not known before the deployment runs)", "path": "modules/pw"}}),
        json!({}),
    );
    unknown.pin = Some(pin.clone());
    let (code, message) = invalid(d.check_pin(&unknown).await.unwrap_err());
    assert_eq!(code, "TOFU-003", "{message}");
    assert!(message.contains("source.url"), "{message}");
    // An observation reports the version the last apply ran.
    let Observation::Ready { outputs, .. } =
        d.get(&request(values.clone(), json!({}))).await.unwrap()
    else {
        panic!("not ready");
    };
    assert_eq!(outputs.footprint.pin, Some(pin.clone()));

    // A pin from another source, or with no commit, is refused.
    for (edit, want) in [
        (
            json!({"source": {"url": "file:///elsewhere", "path": "modules/pw"}}),
            "not the one",
        ),
        (json!({"source": null}), "not the one"),
        (
            json!({"source": {"commit": "--upload-pack=x"}}),
            "names no commit",
        ),
        (
            json!({"source": {"commit": "0".repeat(40)}}),
            "can no longer be fetched",
        ),
    ] {
        let mut bad = pin.clone();
        for (k, v) in edit.as_object().unwrap() {
            if v.is_object() {
                for (field, value) in v.as_object().unwrap() {
                    bad[k][field] = value.clone();
                }
            } else {
                bad[k] = v.clone();
            }
        }
        let mut req = request(values.clone(), json!({}));
        req.pin = Some(bad);
        let (code, message) = invalid(d.put(&req).await.unwrap_err());
        assert_eq!(code, "TOFU-003", "{message}");
        assert!(message.contains(want), "{message}");
        // The check before a replay refuses it the same way.
        let (code, message) = invalid(d.check_pin(&req).await.unwrap_err());
        assert_eq!(code, "TOFU-003", "{message}");
        assert!(message.contains(want), "{message}");
    }
}
