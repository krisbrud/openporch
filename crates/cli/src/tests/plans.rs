//! Previews and acknowledgements against a dev server: `porch workload plan`,
//! `--plan` and `--ack`, `Stale confirmation` and `Interrupted deployment` in
//! `cli`, and deleting environments and applications behind their previews.

use std::sync::Arc;

use openporch_client::Method;
use openporch_engine::planner::{
    DeprovisionJob, ExecFailure, Executor, Limits, Planner, ProvisionJob, ProvisionResult,
};
use openporch_server::deploy::{PlannerRunner, StorePermits};
use serde_json::{Value, json};

use super::deploys::{deployable, deploys, file, serve};
use super::objects::admin_client;
use super::{DevServer, Machine, Outcome};

const DB: &str = "postgres.default#modules.web.externals.db";

const WITH_DB: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx:1
resources:
  db: {type: postgres}
"#;

const WITHOUT_DB: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx:1
"#;

/// `shop` deploying workloads and databases without a cloud.
async fn with_databases(server: &DevServer) -> Machine {
    let m = deployable(server, json!({"driver_type": "openporch/none"})).await;
    server
        .post(
            "/orgs/acme/resources/defs",
            json!({"id": "pg", "type": "postgres", "driver_type": "openporch/echo", "criteria": [{}]}),
        )
        .await;
    m
}

async fn active(server: &DevServer, env: &str) -> Vec<String> {
    let list = admin_client(server)
        .request(
            Method::GET,
            &format!("/orgs/acme/apps/shop/envs/{env}/resources"),
            None,
        )
        .await
        .unwrap();
    list["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["descriptor"].as_str().unwrap().to_owned())
        .collect()
}

/// Schedules the database for deletion: the next deployment that leaves it
/// out deletes it.
async fn schedule_deletion(server: &DevServer) {
    admin_client(server)
        .request(
            Method::DELETE,
            "/orgs/acme/apps/shop/envs/development/resources/postgres/modules.web.externals.db",
            None,
        )
        .await
        .unwrap();
}

/// The command a preview printed for carrying it out, as arguments.
fn suggested(r: &Outcome, start: &str) -> Vec<String> {
    let text = format!("{}\n{}", r.out, r.err);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(start))
        .unwrap_or_else(|| panic!("no `{start}` command: {r:?}"));
    // `porch` and the arguments, without the quotes around acks.
    line.split_whitespace()
        .skip(1)
        .map(|a| a.trim_matches('\'').to_owned())
        .collect()
}

async fn porch(m: &Machine, args: &[String]) -> Outcome {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    m.porch(&args, "").await
}

/// `porch workload plan` shows the changes and what they need; a deployment
/// that deletes runs only with each deletion acknowledged.
#[tokio::test]
async fn a_deletion_runs_only_once_acknowledged() {
    let server = DevServer::start(None).await;
    let m = with_databases(&server).await;
    let with_db = file(&m, "with-db.yaml", WITH_DB);
    let without_db = file(&m, "without-db.yaml", WITHOUT_DB);
    let r = m
        .porch(&["workload", "deploy", "-f", &with_db, "--wait"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    schedule_deletion(&server).await;

    // Without a preview the deletion is refused, and the CLI says how to
    // preview it. Nothing is deployed.
    let r = m
        .porch(&["workload", "deploy", "-f", &without_db, "--wait"], "")
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("DPL-001"), "{r:?}");
    assert!(r.err.contains("porch workload plan"), "{r:?}");
    assert_eq!(deploys(&server).await.len(), 1);
    // There is no flag that acknowledges everything.
    let r = m
        .porch(&["workload", "deploy", "-f", &without_db, "--yes"], "")
        .await;
    assert_eq!(r.code, 2, "{r:?}");

    let r = m.porch(&["workload", "plan", "-f", &without_db], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.starts_with("Preview pln-"), "{r:?}");
    assert!(
        r.out
            .lines()
            .any(|l| l.contains(&format!("delete   {DB}")) && l.ends_with("[permit required]")),
        "{r:?}"
    );
    assert!(
        r.out
            .contains("Needs the deploy permission on shop/development"),
        "{r:?}"
    );
    let command = suggested(&r, "porch deploy delta ");
    let ack = format!("{DB}=delete");
    assert_eq!(command.last(), Some(&ack), "{command:?}");
    assert!(
        active(&server, "development")
            .await
            .contains(&DB.to_owned())
    );

    // The preview alone deletes nothing, and each marked change must be named.
    let plan_only: Vec<String> = command[..command.len() - 2].to_vec();
    let r = porch(&m, &plan_only).await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("need acknowledging"), "{r:?}");
    assert!(r.err.contains(&format!("--ack '{ack}'")), "{r:?}");
    assert_eq!(deploys(&server).await.len(), 1);

    let mut with_wait = command.clone();
    with_wait.push("--wait".into());
    let r = porch(&m, &with_wait).await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        !active(&server, "development")
            .await
            .contains(&DB.to_owned())
    );
}

/// `Stale confirmation` in `cli`.
#[tokio::test]
async fn a_stale_preview_is_refused_and_deletes_nothing() {
    let server = DevServer::start(None).await;
    let m = with_databases(&server).await;
    let with_db = file(&m, "with-db.yaml", WITH_DB);
    let without_db = file(&m, "without-db.yaml", WITHOUT_DB);
    let r = m
        .porch(&["workload", "deploy", "-f", &with_db, "--wait"], "")
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    schedule_deletion(&server).await;
    let r = m.porch(&["workload", "plan", "-f", &without_db], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    let mut command = suggested(&r, "porch deploy delta ");
    command.push("--wait".into());

    // The environment changes after the preview.
    let r = m
        .porch(
            &[
                "workload",
                "deploy",
                "-f",
                &with_db,
                "--property",
                "containers.web.variables.MODE=busy",
                "--wait",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");

    let r = porch(&m, &command).await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("no longer matches"), "{r:?}");
    assert!(r.err.contains("preview again"), "{r:?}");
    assert_eq!(deploys(&server).await.len(), 2);
    assert!(
        active(&server, "development")
            .await
            .contains(&DB.to_owned())
    );
}

/// Provisions everything except postgres, whose outcome is unknown.
#[derive(Debug)]
struct Uncertain;

impl Executor for Uncertain {
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        match job.descriptor.ty.to_string().as_str() {
            "postgres" => Err(ExecFailure::Uncertain("postgres timed out".into())),
            _ => Ok(ProvisionResult::default()),
        }
    }

    async fn deprovision(&self, _: DeprovisionJob) -> Result<(), ExecFailure> {
        Ok(())
    }
}

/// `Interrupted deployment` in `cli`.
#[tokio::test]
async fn an_interrupted_deployment_names_the_resource_and_its_recovery() {
    let data = tempfile::tempdir().unwrap();
    let state = openporch_server::start(openporch_server::Config::new("acme", data.path()), None)
        .await
        .unwrap();
    let planner = Planner::new(
        Arc::new(state.store.clone()),
        Arc::new(Uncertain),
        Limits::default(),
    )
    .with_permit_check(Arc::new(StorePermits(state.store.clone())));
    state
        .runners
        .set(Arc::new(PlannerRunner::new(Arc::new(planner))));
    let (router, _) = openporch_server::router(state);
    let server = serve(router, data).await;
    let m = with_databases(&server).await;
    let with_db = file(&m, "with-db.yaml", WITH_DB);

    let r = m
        .porch(&["workload", "deploy", "-f", &with_db, "--wait"], "")
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("recovery required"), "{r:?}");
    assert!(r.err.contains(&format!("{DB} (operation ")), "{r:?}");
    assert!(
        r.err
            .contains("porch resources recover postgres modules.web.externals.db"),
        "{r:?}"
    );
    assert!(!r.err.contains("deploy again"), "{r:?}");

    // Deploying again is refused until it is recovered.
    let r = m
        .porch(&["workload", "deploy", "-f", &with_db, "--wait"], "")
        .await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("DPL-004"), "{r:?}");
    assert!(r.err.contains("porch resources recover"), "{r:?}");

    let r = m
        .porch(
            &[
                "resources",
                "recover",
                "postgres",
                "modules.web.externals.db",
                "-o",
                "json",
            ],
            "",
        )
        .await;
    assert_eq!(r.code, 0, "{r:?}");
    let outcome: Value = serde_json::from_str(&r.out).unwrap();
    assert_eq!(outcome["outcome"], "recovery_required", "{outcome}");
}

/// Deleting an environment and then the application, each behind a preview
/// whose deletions are acknowledged.
#[tokio::test]
async fn environments_and_applications_are_deleted_behind_previews() {
    let server = DevServer::start(None).await;
    let mut m = with_databases(&server).await;
    server
        .post(
            "/orgs/acme/apps/shop/envs",
            json!({"id": "staging", "type": "development"}),
        )
        .await;
    let with_db = file(&m, "with-db.yaml", WITH_DB);
    for env in ["development", "staging"] {
        let r = m
            .porch(
                &["workload", "deploy", "-f", &with_db, "--env", env, "--wait"],
                "",
            )
            .await;
        assert_eq!(r.code, 0, "{r:?}");
    }

    // An environment's deletion is previewed and refused until acknowledged.
    let r = m.porch(&["delete", "env", "staging"], "").await;
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.out.contains(&format!("delete   {DB}")), "{r:?}");
    assert!(r.err.contains("nothing was deleted"), "{r:?}");
    assert_eq!(active(&server, "staging").await.len(), 3);
    let command = suggested(&r, "porch delete env staging ");
    assert!(command.contains(&format!("{DB}=delete")), "{command:?}");
    let r = porch(&m, &command).await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out.contains("Deletion of environment staging accepted"),
        "{r:?}"
    );

    let client = admin_client(&server);
    for _ in 0..100 {
        if client
            .request(Method::GET, "/orgs/acme/apps/shop/envs/staging", None)
            .await
            .is_err()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // The application's deletion previews every environment.
    m.set("OPENPORCH_APP", "");
    let r = m.porch(&["delete", "app", "shop", "--preview"], "").await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("Preview pln-"), "{r:?}");
    let command = suggested(&r, "porch delete app shop ");
    assert!(
        command.contains(&format!("development:{DB}=delete")),
        "{command:?}"
    );
    let r = porch(&m, &command).await;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out.contains("Deletion of application shop accepted"),
        "{r:?}"
    );
}
