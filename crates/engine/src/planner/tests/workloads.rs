//! A workload is a `workload` resource matched by its class: the matched
//! definition's driver receives the resolved workload document.

use super::*;

/// A zip function workload of class `aws-lambda` that uses a database and
/// reads from a queue.
const WORKER: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: worker, class: aws-lambda}
function:
  package: {type: zip, artifact: "s3://artifacts/worker.zip", runtime: python3.12, handler: app.handler}
  variables:
    DB_HOST: "${resources.db.host}"
    DB_PASSWORD: "${resources.db.password}"
triggers:
  orders: {queue: {resource: queue, batchSize: 10}}
resources:
  db: {type: postgres}
  queue: {type: sqs}
"#;

const WORKER_WORKLOAD: &str = "workload.aws-lambda#modules.worker";
const WORKER_DB: &str = "postgres.default#modules.worker.externals.db";

/// The fixture's definitions with `workload` matched by class: `aws-lambda`
/// to OpenTofu, and the rest to `driver`.
fn with_workloads(driver: &str) -> Vec<ResourceDefinition> {
    let mut defs: Vec<_> = definitions()
        .into_iter()
        .filter(|d| d.ty.as_str() != "workload")
        .collect();
    defs.push(
        serde_json::from_value(json!({
            "id": "lambda", "type": "workload", "driver_type": "openporch/opentofu",
            "driver_inputs": {"values": {"source": {"path": "modules/lambda"}}},
            "criteria": [{"class": "aws-lambda"}],
        }))
        .unwrap(),
    );
    defs.push(
        serde_json::from_value(json!({
            "id": "sqs", "type": "sqs", "driver_type": "openporch/echo",
            "driver_inputs": {}, "criteria": [{}],
        }))
        .unwrap(),
    );
    defs.push(
        serde_json::from_value(json!({
            "id": "plain", "type": "workload", "driver_type": driver,
            "driver_inputs": {}, "criteria": [{}],
        }))
        .unwrap(),
    );
    defs
}

#[tokio::test]
async fn a_lambda_class_workload_gets_its_resolved_document() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let deployment = f
        .accept_set(Some(WORKER), with_workloads("openporch/echo"))
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert_eq!(outcomes[&d(WORKER_WORKLOAD)], Outcome::Provisioned);

    // The class picked the OpenTofu definition.
    let job = fake.job(WORKER_WORKLOAD);
    assert_eq!(job.definition_id.as_str(), "lambda");
    assert_eq!(job.driver_type.to_string(), "openporch/opentofu");
    // Its inputs are the workload document with the database's outputs
    // filled in; the password stays a reference.
    let Resolved::Object(inputs) = &job.inputs.resource else {
        panic!("{:?}", job.inputs.resource);
    };
    let input = |name: &str| &inputs.iter().find(|(k, _)| k == name).unwrap().1;
    assert_eq!(input("id"), &Resolved::Value(json!("worker")));
    assert_eq!(input("class"), &Resolved::Value(json!("aws-lambda")));
    // Its direct dependencies' outputs, by key.
    let Resolved::Object(dependencies) = input("dependencies") else {
        panic!("{inputs:?}");
    };
    let dependency = |key: &str| &dependencies.iter().find(|(k, _)| k == key).unwrap().1;
    assert_eq!(
        dependency("queue"),
        &Resolved::Value(json!({"host": "sqs.internal"}))
    );
    assert!(matches!(dependency("db"), Resolved::Object(db)
        if db.iter().any(|(k, r)| k == "password" && matches!(r, Resolved::Secret(_)))));
    let Resolved::Object(doc) = input("spec") else {
        panic!("{inputs:?}");
    };
    let field = |name: &str| &doc.iter().find(|(k, _)| k == name).unwrap().1;
    let Resolved::Object(function) = field("function") else {
        panic!("{doc:?}");
    };
    let get = |name: &str| &function.iter().find(|(k, _)| k == name).unwrap().1;
    assert_eq!(
        get("package"),
        &Resolved::Value(json!({
            "type": "zip", "artifact": "s3://artifacts/worker.zip",
            "runtime": "python3.12", "handler": "app.handler",
        }))
    );
    let Resolved::Object(variables) = get("variables") else {
        panic!("{function:?}");
    };
    assert_eq!(
        variables[0],
        (
            "DB_HOST".into(),
            Resolved::Value(json!("postgres.internal"))
        )
    );
    assert!(
        matches!(&variables[1], (k, Resolved::Secret(e)) if k == "DB_PASSWORD"
        && matches!(&e.parts[..], [SecretPart::Secret(r)] if r.reference == "pg-password"))
    );
    // The trigger carries its queue's outputs.
    let Resolved::Value(triggers) = field("triggers") else {
        panic!("{doc:?}");
    };
    assert_eq!(
        triggers["orders"]["queue"],
        json!({"resource": "queue", "batchSize": 10, "outputs": {"host": "sqs.internal"}})
    );
    // The database was provisioned first.
    let db = f.active(WORKER_DB).await.unwrap();
    let workload = f.active(WORKER_WORKLOAD).await.unwrap();
    assert!(workload.dependencies.contains(&db.descriptor));
}

#[test]
fn a_workload_without_a_definition_names_its_class_and_environment() {
    let mut set = DeploymentSet::default();
    let c = Workload::parse(WORKER, None)
        .unwrap()
        .to_module(&builtin_types())
        .unwrap();
    set.modules.insert(c.id, c.module);
    // Only the default class has a workload definition.
    let defs = definitions();
    let error = build_graph(GraphInput {
        org: &ORG.parse().unwrap(),
        app: &"shop".parse().unwrap(),
        env: &"pr-42".parse().unwrap(),
        env_type: &"development".parse().unwrap(),
        set: &set,
        definitions: &defs,
        types: &builtin_types(),
        classes: &classes(),
    })
    .unwrap_err()
    .to_string();
    for part in ["`workload`", "`aws-lambda`", "`modules.worker`", "`pr-42`"] {
        assert!(error.contains(part), "{part}: {error}");
    }
}

#[tokio::test]
async fn a_none_workload_stores_its_resolved_document() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let deployment = f
        .accept_set(Some(WEB), with_workloads("openporch/none"))
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
    assert_eq!(f.status(&deployment).await.0, DeploymentStatus::Succeeded);
    assert!(f.active(DB).await.is_some());

    // The workload's outputs show the database host; the password is a
    // secret output, kept as a reference.
    let workload = f.active(WORKLOAD).await.unwrap();
    assert_eq!(workload.outputs["id"], "web");
    assert_eq!(
        workload.outputs["dependencies"]["db"]["host"],
        "postgres.internal"
    );
    let spec = &workload.outputs["spec"];
    let variables = &spec["containers"]["web"]["variables"];
    assert_eq!(variables["DB_HOST"], "postgres.internal", "{variables}");
    assert!(variables.get("DB_PASSWORD").is_none(), "{variables}");
    assert_eq!(spec["containers"]["web"]["image"], "nginx");
    let password = &workload.secret_outputs["spec.containers.web.variables.DB_PASSWORD"];
    assert_eq!(password.reference, "pg-password");
}

#[tokio::test]
async fn file_content_marked_no_expand_reaches_the_driver_literally() {
    const TEMPLATED: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    files:
      /etc/nginx/conf.d/app.conf:
        content: "root ${HOME}; ${context.env.id} ${"
        noExpand: true
      /etc/app/db:
        content: "host=${resources.db.host}"
resources:
  db: {type: postgres}
"#;
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let deployment = f
        .accept_set(Some(TEMPLATED), with_workloads("openporch/none"))
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
    let workload = f.active(WORKLOAD).await.unwrap();
    let files = &workload.outputs["spec"]["containers"]["web"]["files"];
    assert_eq!(
        files["/etc/nginx/conf.d/app.conf"]["content"],
        "root ${HOME}; ${context.env.id} ${"
    );
    assert_eq!(files["/etc/app/db"]["content"], "host=postgres.internal");
}

#[tokio::test]
async fn a_missing_artifact_stops_the_deployment_before_anything_runs() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    fake.gone
        .lock()
        .unwrap()
        .insert("s3://artifacts/worker.zip".into());
    let deployment = f
        .accept_set(Some(WORKER), with_workloads("openporch/echo"))
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;

    // The workload names the artifact; nothing else started.
    assert!(
        matches!(&outcomes[&d(WORKER_WORKLOAD)], Outcome::Failed(e)
            if e.contains("artifact `s3://artifacts/worker.zip` no longer exists")),
        "{outcomes:?}"
    );
    for (descriptor, outcome) in &outcomes {
        if descriptor.to_string() != WORKER_WORKLOAD {
            assert!(
                matches!(outcome, Outcome::Pending(r) if r.contains("artifact")),
                "{descriptor}: {outcome:?}"
            );
        }
    }
    assert!(fake.jobs.lock().unwrap().is_empty());
    let (status, error) = f.status(&deployment).await;
    assert_eq!(status, DeploymentStatus::Failed);
    assert!(error.unwrap().contains("no longer exists"));

    // Checked once per workload, with the definition's account.
    let checks = fake.artifact_checks.lock().unwrap().clone();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0].descriptor.to_string(), WORKER_WORKLOAD);
    assert_eq!(checks[0].artifacts, ["s3://artifacts/worker.zip"]);
    assert_eq!(checks[0].account, None);
    assert_eq!(checks[0].context.deployment_id, deployment.id);

    // Once it exists again, the deployment runs.
    fake.gone.lock().unwrap().clear();
    let deployment = f
        .accept_set(Some(WORKER), with_workloads("openporch/echo"))
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
}
