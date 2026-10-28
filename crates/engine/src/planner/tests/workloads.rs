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

/// The worker with its artifact pinned to a version.
fn pinned_worker() -> String {
    WORKER.replace(
        "s3://artifacts/worker.zip",
        "s3://artifacts/worker.zip?versionId=v1",
    )
}

/// [`with_workloads`] with the lambda definition reading artifacts with
/// `account`.
fn with_lambda_account(account: Option<&str>) -> Vec<ResourceDefinition> {
    let mut defs = with_workloads("openporch/echo");
    for def in &mut defs {
        if def.id.as_str() == "lambda" {
            def.driver_account = account.map(str::to_owned);
        }
    }
    defs
}

#[tokio::test]
async fn a_missing_artifact_stops_the_deployment_before_anything_runs() {
    const PINNED: &str = "s3://artifacts/worker.zip?versionId=v1";
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    fake.gone.lock().unwrap().insert(PINNED.into());
    let worker = pinned_worker();
    let deployment = f
        .accept_set(Some(&worker), with_lambda_account(Some("aws-lambda")))
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;

    // The workload names the artifact; nothing else started.
    assert!(
        matches!(&outcomes[&d(WORKER_WORKLOAD)], Outcome::Failed(e)
            if e.contains(&format!("artifact `{PINNED}` no longer exists"))),
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
    assert_eq!(checks[0].definition_id.as_str(), "lambda");
    assert_eq!(checks[0].artifacts, [PINNED]);
    assert_eq!(checks[0].account.as_str(), "aws-lambda");
    assert_eq!(checks[0].context.deployment_id, deployment.id);

    // Once it exists again, the deployment runs.
    fake.gone.lock().unwrap().clear();
    let deployment = f
        .accept_set(Some(&worker), with_lambda_account(Some("aws-lambda")))
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn artifacts_without_a_version_or_an_account_known_up_front_are_left_to_the_driver() {
    let worker = pinned_worker();
    for (workload, account) in [
        // Named without a version.
        (WORKER, Some("aws-lambda")),
        // No account to read it with.
        (worker.as_str(), None),
        // An account that comes from a resource not yet provisioned.
        (
            worker.as_str(),
            Some("${resources['postgres.default#modules.worker.externals.db'].account}"),
        ),
    ] {
        let f = Fixture::new().await;
        let fake = Fake::with(&[]);
        fake.gone.lock().unwrap().extend([
            "s3://artifacts/worker.zip".to_owned(),
            "s3://artifacts/worker.zip?versionId=v1".to_owned(),
        ]);
        let deployment = f
            .accept_set(Some(workload), with_lambda_account(account))
            .await
            .unwrap();
        let outcomes = planner(&f, &fake, fast()).run(deployment).await;
        assert!(
            fake.artifact_checks.lock().unwrap().is_empty(),
            "{account:?}"
        );
        // The rest of the deployment ran.
        assert_eq!(outcomes[&d(WORKER_DB)], Outcome::Provisioned, "{account:?}");
    }
}

#[tokio::test]
async fn a_check_that_may_pass_is_retried() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    *fake.artifact_flakes.lock().unwrap() = 2;
    let deployment = f
        .accept_set(
            Some(&pinned_worker()),
            with_lambda_account(Some("aws-lambda")),
        )
        .await
        .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
    assert_eq!(fake.artifact_checks.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn unpinned_artifacts_are_pinned_with_the_definitions_account() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    *fake.artifact_flakes.lock().unwrap() = 1;
    let deployment = f
        .accept_set(Some(WORKER), with_lambda_account(Some("aws-lambda")))
        .await
        .unwrap();
    let pinned = planner(&f, &fake, fast())
        .pin_artifacts(&deployment, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        pinned,
        BTreeMap::from([(
            d(WORKER_WORKLOAD),
            "s3://artifacts/worker.zip?versionId=v9".to_owned()
        )])
    );
    // Asked again after a failure that may pass, with the definition's account.
    let pins = fake.artifact_pins.lock().unwrap().clone();
    assert_eq!(pins.len(), 2);
    assert_eq!(pins[1].account.as_str(), "aws-lambda");
    assert_eq!(pins[1].definition_id.as_str(), "lambda");
    assert_eq!(pins[1].artifact, "s3://artifacts/worker.zip");

    // An artifact already pinned is kept as it is.
    let worker = pinned_worker();
    let f = Fixture::new().await;
    let deployment = f
        .accept_set(Some(&worker), with_lambda_account(Some("aws-lambda")))
        .await
        .unwrap();
    let pinned = planner(&f, &fake, fast())
        .pin_artifacts(&deployment, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(pinned.is_empty(), "{pinned:?}");

    // A shared value that names a version is used as it resolves.
    let named = WORKER.replace("s3://artifacts/worker.zip", "${values.ARTIFACT}");
    let f = Fixture::new().await;
    let mut deployment = f
        .accept_set(Some(&named), with_lambda_account(Some("aws-lambda")))
        .await
        .unwrap();
    deployment.values.insert(
        "ARTIFACT".into(),
        SharedValue::Value(json!("s3://artifacts/worker.zip?versionId=v3")),
    );
    let pinned = planner(&f, &fake, fast())
        .pin_artifacts(&deployment, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        pinned[&d(WORKER_WORKLOAD)],
        "s3://artifacts/worker.zip?versionId=v3"
    );
    assert_eq!(fake.artifact_pins.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn artifacts_are_not_pinned_without_an_account_known_up_front() {
    for (account, expected) in [
        (None, "has no cloud account"),
        (
            Some("${resources['postgres.default#modules.worker.externals.db'].account}"),
            "comes from another resource",
        ),
    ] {
        let f = Fixture::new().await;
        let fake = Fake::with(&[]);
        let deployment = f
            .accept_set(Some(WORKER), with_lambda_account(account))
            .await
            .unwrap();
        let e = planner(&f, &fake, fast())
            .pin_artifacts(&deployment, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, PinError::Refused(m) if m.contains(expected) && m.contains("?versionId=")),
            "{e:?}"
        );
        assert!(fake.artifact_pins.lock().unwrap().is_empty());
    }
}

const ECR: &str = "111122223333.dkr.ecr.eu-west-1.amazonaws.com";

/// [`WORKER`] as an image function with `image`.
fn image_worker(image: &str) -> String {
    WORKER.replace(
        r#"{type: zip, artifact: "s3://artifacts/worker.zip", runtime: python3.12, handler: app.handler}"#,
        &format!(r#"{{type: image, image: "{image}"}}"#),
    )
}

#[tokio::test]
async fn ecr_images_are_pinned_to_digests_with_the_definitions_account() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let deployment = f
        .accept_set(
            Some(&image_worker(&format!("{ECR}/worker:1"))),
            with_lambda_account(Some("aws-lambda")),
        )
        .await
        .unwrap();
    let pinned = planner(&f, &fake, fast())
        .pin_images(&deployment, Duration::from_secs(5))
        .await
        .unwrap();
    let digest = format!("{ECR}/worker@sha256:{}", "aa".repeat(32));
    assert_eq!(
        pinned,
        BTreeMap::from([(
            d(WORKER_WORKLOAD),
            BTreeMap::from([(
                "/params/spec/function/package/image".to_owned(),
                digest.clone()
            )])
        )])
    );
    let pins = fake.image_pins.lock().unwrap().clone();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].account.as_str(), "aws-lambda");
    assert_eq!(pins[0].definition_id.as_str(), "lambda");

    // Images pinned by digest, and images in other registries, are kept.
    for image in [digest.as_str(), "ghcr.io/acme/worker:1"] {
        let f = Fixture::new().await;
        let deployment = f
            .accept_set(Some(&image_worker(image)), with_lambda_account(None))
            .await
            .unwrap();
        let pinned = planner(&f, &fake, fast())
            .pin_images(&deployment, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(pinned.is_empty(), "{image}: {pinned:?}");
    }
    assert_eq!(fake.image_pins.lock().unwrap().len(), 1);

    // An image from another resource's outputs is pinned only in ECR, and a
    // malformed digest is refused.
    let from_output = "${resources.db.host}/web:1".to_owned();
    let f = Fixture::new().await;
    let deployment = f
        .accept_set(Some(&image_worker(&from_output)), with_lambda_account(None))
        .await
        .unwrap();
    let pinned = planner(&f, &fake, fast())
        .pin_images(&deployment, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(pinned.is_empty(), "{pinned:?}");
    for (image, expected) in [
        (format!("{ECR}/${{resources.db.host}}:1"), "must be known"),
        (format!("{ECR}/worker@sha256:abc"), "not `sha256:`"),
    ] {
        let f = Fixture::new().await;
        let deployment = f
            .accept_set(
                Some(&image_worker(&image)),
                with_lambda_account(Some("aws-lambda")),
            )
            .await
            .unwrap();
        let e = planner(&f, &fake, fast())
            .pin_images(&deployment, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(&e, PinError::Refused(m) if m.contains(expected)),
            "{image}: {e:?}"
        );
    }

    // A shared value naming an ECR tag is resolved, then pinned.
    let f = Fixture::new().await;
    let mut deployment = f
        .accept_set(
            Some(&image_worker("${values.IMAGE}")),
            with_lambda_account(Some("aws-lambda")),
        )
        .await
        .unwrap();
    deployment.values.insert(
        "IMAGE".into(),
        SharedValue::Value(json!(format!("{ECR}/worker:2"))),
    );
    let pinned = planner(&f, &fake, fast())
        .pin_images(&deployment, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        pinned[&d(WORKER_WORKLOAD)]["/params/spec/function/package/image"],
        digest
    );
    assert_eq!(
        fake.image_pins.lock().unwrap()[1].image,
        format!("{ECR}/worker:2")
    );

    // An ECR image needs an account to be read with.
    let f = Fixture::new().await;
    let deployment = f
        .accept_set(
            Some(&image_worker(&format!("{ECR}/worker:1"))),
            with_lambda_account(None),
        )
        .await
        .unwrap();
    let e = planner(&f, &fake, fast())
        .pin_images(&deployment, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, PinError::Refused(m) if m.contains("has no cloud account") && m.contains("@sha256:")),
        "{e:?}"
    );
}

/// The Secret in a composite string scenario, before the executor: the
/// converted workload and the deployment's graph keep
/// `Bearer ${values.API_TOKEN}` as written, and the planner hands the
/// executor an unresolved expression of literal text and the secret's
/// reference, never a value.
#[tokio::test]
async fn a_secret_in_a_composite_string_stays_an_expression_until_the_executor() {
    const EXPRESSION: &str = "Bearer ${values.API_TOKEN}";
    let doc = WEB.replace(
        r#"      DB_PASSWORD: "${resources.db.password}""#,
        &format!("      DB_PASSWORD: \"${{resources.db.password}}\"\n      AUTH: \"{EXPRESSION}\""),
    );
    assert!(doc.contains(EXPRESSION), "{doc}");

    // Conversion keeps the expression.
    let converted = Workload::parse(&doc, None)
        .unwrap()
        .to_module(&builtin_types())
        .unwrap();
    let module = serde_json::to_value(&converted.module).unwrap();
    assert_eq!(
        module["spec"]["containers"]["web"]["variables"]["AUTH"], EXPRESSION,
        "{module}"
    );

    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let mut deployment = f.accept_set(Some(&doc), definitions()).await.unwrap();
    let token = SecretRef {
        store: "openporch".into(),
        reference: "apps/shop/values/API_TOKEN".into(),
        version: Some("1".into()),
    };
    deployment
        .values
        .insert("API_TOKEN".into(), SharedValue::Secret(token.clone()));
    // So does the graph the deployment was accepted with.
    let graph = serde_json::to_string(&deployment.graph).unwrap();
    assert!(graph.contains(EXPRESSION), "{graph}");

    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert_eq!(outcomes[&d(WORKLOAD)], Outcome::Provisioned, "{outcomes:?}");
    let job = fake.job(WORKLOAD);
    let Resolved::Object(inputs) = &job.inputs.resource else {
        panic!("{:?}", job.inputs.resource);
    };
    let field = |items: &[(String, Resolved)], name: &str| {
        items
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| panic!("no `{name}` in {items:?}"))
    };
    let mut at = Resolved::Object(inputs.clone());
    for name in ["spec", "containers", "web", "variables", "AUTH"] {
        let Resolved::Object(items) = &at else {
            panic!("`{name}` is not inside an object: {at:?}");
        };
        at = field(items, name);
    }
    let Resolved::Secret(expr) = &at else {
        panic!("AUTH is not a secret expression: {at:?}");
    };
    assert_eq!(
        expr.parts,
        vec![
            SecretPart::Literal("Bearer ".into()),
            SecretPart::Secret(token.clone()),
        ]
    );
    // The executor learns where the reference came from, to check its use.
    assert!(
        job.inputs.sources.iter().any(|(r, _)| *r == token),
        "{:?}",
        job.inputs.sources
    );
}
