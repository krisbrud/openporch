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
        values: object(values),
        secrets: SecretMap::new(object(secrets)),
        credentials: None,
        context: DriverContext::default(),
        cookie: None,
        permit: None,
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
            json!({"tokens": {"bad host": "t"}}),
            "is not a host name",
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
        for secret in ["hunter2-token", "aws-secret-1", "tfc-token-1"] {
            assert!(!line.contains(secret), "{line}");
        }
    }
    assert!(record.contains("AWS_ACCESS_KEY_ID=AKIAFAKE"));
    assert!(record.contains("TF_TOKEN_app_terraform_io=tfc-token-1"));
    assert!(record.contains("TF_IN_AUTOMATION=1"));
    assert!(record.contains(r#""api_token":"hunter2-token""#));
    assert!(record.contains(r#""secret_key":"aws-secret-1""#));
    assert!(record.contains(r#"{"length":12}"#));
    // The server's own environment stays out: cargo sets these for the tests.
    assert!(std::env::var("CARGO_PKG_NAME").is_ok());
    assert!(!record.contains("CARGO_"));
    let calls: Vec<&str> = record
        .lines()
        .filter_map(|l| l.strip_prefix("ARGS "))
        .map(|a| a.split(' ').next().unwrap())
        .collect();
    assert_eq!(calls, ["init", "plan", "show", "apply", "output"]);

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

    // The run directory, with its secret files, is gone.
    let runs = root.path().join("runs/0123456789abcdef0123456789abcdef");
    assert_eq!(std::fs::read_dir(runs).unwrap().count(), 0);
    // State lives outside it, owner-only.
    let state = root.path().join("state/0123456789abcdef0123456789abcdef");
    let mode = std::fs::metadata(state).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
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
    assert_eq!(actions, [(Action::Replace, "random_password.p".to_owned())]);
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
    assert_eq!(actions, [(Action::Delete, "random_password.p".to_owned())]);
    del.permit = Some(fingerprint);
    assert_eq!(d.delete(&del).await.unwrap(), DeleteOutcome::Deleted);
    assert!(
        !root
            .path()
            .join("state/0123456789abcdef0123456789abcdef")
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
                   "variables": {"length": 12}}),
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

    // The branch moves on to a broken module; deletion still uses the commit
    // that was applied.
    std::fs::write(repo.join("modules/pw/main.tf"), "this is not HCL {").unwrap();
    git(&repo, &["commit", "-q", "-am", "break"]);
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
