use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use openporch_core::catalog::{ClassRegistry, builtin_types};
use openporch_core::graph::{GraphInput, build_graph};
use openporch_core::placeholder::SecretPart;
use openporch_core::sets::DeploymentSet;
use openporch_core::workload::Workload;
use openporch_store::secrets::{
    BuiltinSecretStore, Keyring, MasterKey, SecretStore as _, SecretValue,
};
use openporch_store::{AuditIntent, DeploymentRequest, DeploymentStatus, Ownership, SqliteStore};
use tokio::sync::Barrier;

use super::*;

const ORG: &str = "acme";

/// Plaintexts in the built-in secret store: the definition's master password,
/// seeded before the deployment, and the secret outputs the executor writes.
const MASTER: &str = "master-plaintext-31";
const PASSWORD: &str = "password-plaintext-47";
const TOKEN: &str = "token-plaintext-59";

const WEB: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
    variables:
      DB_HOST: "${resources.db.host}"
      DB_PASSWORD: "${resources.db.password}"
resources:
  db: {type: postgres}
  route: {type: route}
"#;

const DB: &str = "postgres.default#modules.web.externals.db";
const DNS: &str = "dns.default#modules.web.externals.route";
const ROUTE: &str = "route.default#modules.web.externals.route";
const WORKLOAD: &str = "workload.default#modules.web";
const BASE: &str = "base-env.default#base-env";

fn d(s: &str) -> Descriptor {
    Descriptor::parse(s).unwrap()
}

fn definitions() -> Vec<ResourceDefinition> {
    let def = |id: &str, ty: &str, inputs: Value| {
        serde_json::from_value(json!({
            "id": id, "type": ty, "driver_type": "openporch/echo",
            "driver_inputs": inputs, "criteria": [{}],
        }))
        .unwrap()
    };
    vec![
        def("none-workload", "workload", json!({})),
        def("base-env", "base-env", json!({})),
        def(
            "pg",
            "postgres",
            json!({
                "values": {"name": "${context.app.id}-${context.res.id}", "id": "${context.res.guresid}"},
                "secret_refs": {"master": {"store": "openporch", "ref": "pg-master"}},
            }),
        ),
        def(
            "route",
            "route",
            json!({
                "values": {"host": "${resources.dns.outputs.host}"},
                "secrets": {"auth": "Bearer ${resources.dns.outputs.token}"},
            }),
        ),
        def("dns", "dns", json!({})),
    ]
}

/// How the fake executor answers for a resource type.
#[derive(Debug, Clone)]
enum Behavior {
    Succeed,
    Retryable(Option<Duration>),
    Permanent,
    Hang,
    Panic,
    /// Waits until every resource sharing the barrier is provisioning at once.
    Meet(Arc<Barrier>),
}

/// An executor that provisions nothing and records every job it is handed.
/// With a secret store, it reads every secret input and writes secret outputs
/// as a real executor does.
#[derive(Default)]
struct Fake {
    behavior: Mutex<BTreeMap<String, Behavior>>,
    jobs: Mutex<Vec<ProvisionJob>>,
    running: AtomicUsize,
    most: AtomicUsize,
    secrets: Option<BuiltinSecretStore>,
    read: Mutex<Vec<String>>,
}

/// The secret references in `r`.
fn secret_refs(r: &Resolved, out: &mut Vec<SecretRef>) {
    match r {
        Resolved::Value(_) => {}
        Resolved::Secret(e) => out.extend(e.parts.iter().filter_map(|p| match p {
            SecretPart::Secret(r) => Some(r.clone()),
            SecretPart::Literal(_) => None,
        })),
        Resolved::Array(items) => items.iter().for_each(|i| secret_refs(i, out)),
        Resolved::Object(items) => items.iter().for_each(|(_, i)| secret_refs(i, out)),
    }
}

impl Fake {
    fn with(behavior: &[(&str, Behavior)]) -> Arc<Self> {
        let fake = Self::default();
        *fake.behavior.lock().unwrap() = behavior
            .iter()
            .map(|(ty, b)| ((*ty).to_owned(), b.clone()))
            .collect();
        Arc::new(fake)
    }

    fn attempts(&self, descriptor: &str) -> usize {
        let jobs = self.jobs.lock().unwrap();
        jobs.iter()
            .filter(|j| j.descriptor.to_string() == descriptor)
            .count()
    }

    fn job(&self, descriptor: &str) -> ProvisionJob {
        let jobs = self.jobs.lock().unwrap();
        jobs.iter()
            .find(|j| j.descriptor.to_string() == descriptor)
            .cloned()
            .unwrap()
    }
}

impl Executor for Fake {
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let behavior = self
            .behavior
            .lock()
            .unwrap()
            .get(job.descriptor.ty.as_str())
            .cloned()
            .unwrap_or(Behavior::Succeed);
        let ty = job.descriptor.ty.to_string();
        let mut refs = Vec::new();
        for input in [
            &job.inputs.secrets,
            &job.inputs.secret_refs,
            &job.inputs.resource,
        ] {
            secret_refs(input, &mut refs);
        }
        self.jobs.lock().unwrap().push(job);
        if let Some(store) = &self.secrets {
            for r in refs {
                let value = store.read(&r).await.unwrap();
                self.read.lock().unwrap().push(value.expose().to_owned());
            }
        }
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        let result = match behavior {
            Behavior::Succeed => Ok(()),
            Behavior::Meet(barrier) => {
                barrier.wait().await;
                Ok(())
            }
            Behavior::Retryable(retry_after) => Err(ExecFailure::Retryable {
                error: "503 from the DNS API".into(),
                retry_after,
            }),
            Behavior::Permanent => Err(ExecFailure::Permanent("400: invalid zone".into())),
            Behavior::Hang => std::future::pending().await,
            Behavior::Panic => panic!("the driver crashed"),
        };
        self.running.fetch_sub(1, Ordering::SeqCst);
        result?;
        let mut provisioned = ProvisionResult::default();
        provisioned
            .outputs
            .insert("host".into(), json!(format!("{ty}.internal")));
        let secret = match ty.as_str() {
            "dns" => Some(("token", "dns-token", TOKEN)),
            "postgres" => Some(("password", "pg-password", PASSWORD)),
            _ => None,
        };
        if let Some((output, name, value)) = secret {
            let reference = match &self.secrets {
                Some(store) => store.write(name, &SecretValue::new(value)).await.unwrap(),
                None => SecretRef {
                    store: "openporch".into(),
                    reference: name.into(),
                    version: None,
                },
            };
            provisioned.secret_outputs.insert(output.into(), reference);
        }
        Ok(provisioned)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<SqliteStore>,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ownership = Ownership::acquire(dir.path()).unwrap();
        let store = SqliteStore::open(ownership, ORG).await.unwrap();
        for (id, role) in [("alice", "manager"), ("ci", "member")] {
            sqlx::query(
                "INSERT INTO principals (org_id, id, type, name, org_role, created_at) \
                 VALUES (?, ?, 'user', ?, ?, 0)",
            )
            .bind(ORG)
            .bind(id)
            .bind(id)
            .bind(role)
            .execute(store.pool())
            .await
            .unwrap();
        }
        store
            .create_app(
                ORG,
                "shop",
                "Shop",
                "alice",
                AuditIntent {
                    actor: None,
                    ..audit("POST", "/orgs/acme/apps")
                },
            )
            .await
            .unwrap();
        for query in [
            "INSERT INTO app_roles (org_id, app_id, principal_id, role) \
             VALUES ('acme', 'shop', 'ci', 'developer')",
            "INSERT INTO env_type_roles (org_id, env_type_id, principal_id, role) \
             VALUES ('acme', 'development', 'ci', 'deployer')",
        ] {
            sqlx::query(query).execute(store.pool()).await.unwrap();
        }
        Self {
            _dir: dir,
            store: Arc::new(store),
        }
    }

    /// Accepts a deployment of the `web` workload and reads it back from its
    /// snapshot, as the server's runner does.
    async fn accept(&self) -> Deployment {
        self.try_accept().await.unwrap()
    }

    async fn try_accept(&self) -> Result<Deployment, StoreError> {
        let mut set = DeploymentSet::default();
        let c = Workload::parse(WEB, None)
            .unwrap()
            .to_module(&builtin_types())
            .unwrap();
        set.modules.insert(c.id, c.module);
        let defs = definitions();
        let (org, app, env, env_type) = (
            ORG.parse().unwrap(),
            "shop".parse().unwrap(),
            "development".parse().unwrap(),
            "development".parse().unwrap(),
        );
        let graph = build_graph(GraphInput {
            org: &org,
            app: &app,
            env: &env,
            env_type: &env_type,
            set: &set,
            definitions: &defs,
            types: &builtin_types(),
            classes: &ClassRegistry::new([]),
        })
        .unwrap();
        let snapshot = json!({
            "version": 1,
            "mode": "full",
            "permits": [],
            "graph": graph,
            "definitions": defs
                .iter()
                .map(|d| (d.id.to_string(), json!({"version": 1, "definition": d})))
                .collect::<serde_json::Map<_, _>>(),
        });
        let set_id = self.store.put_set(ORG, "shop", &set).await.unwrap();
        let generation = self
            .store
            .env(ORG, "shop", "development")
            .await
            .unwrap()
            .unwrap()
            .generation;
        let accepted = self
            .store
            .accept_deployment(DeploymentRequest {
                org: ORG.into(),
                app: "shop".into(),
                env: "development".into(),
                actor: "ci".into(),
                expected_generation: generation,
                idempotency_key: None,
                fingerprint: format!("body-{generation}"),
                set_id: set_id.clone(),
                delta_id: None,
                delta_content: None,
                comment: String::new(),
                snapshot: snapshot.clone(),
                audit: audit("POST", "/orgs/acme/apps/shop/envs/development/deploys"),
            })
            .await?;
        Ok(Deployment::from_snapshot(
            org,
            app,
            env,
            env_type,
            accepted.deployment().id.clone(),
            set_id,
            &snapshot,
        )
        .unwrap())
    }

    async fn status(&self, deployment: &Deployment) -> (DeploymentStatus, Option<String>) {
        let record = self
            .store
            .deployment(ORG, "shop", "development", &deployment.id)
            .await
            .unwrap()
            .unwrap();
        (record.status, record.error)
    }

    /// Each resource's recorded status in `deployment`.
    async fn resources(&self, deployment: &Deployment) -> BTreeMap<String, String> {
        sqlx::query_as::<_, (String, String)>(
            "SELECT descriptor, status FROM deployment_resources WHERE deployment_id = ?",
        )
        .bind(&deployment.id)
        .fetch_all(self.store.pool())
        .await
        .unwrap()
        .into_iter()
        .collect()
    }
}

fn audit(method: &str, path: &str) -> AuditIntent {
    AuditIntent::accepted(ORG, "ci", method, path, "test")
}

fn fast() -> Limits {
    Limits {
        backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        ..Limits::default()
    }
}

fn planner(f: &Fixture, fake: &Arc<Fake>, limits: Limits) -> Planner<SqliteStore, Fake> {
    Planner::new(f.store.clone(), fake.clone(), limits)
}

#[tokio::test]
async fn one_branch_fails() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Retryable(None))]);
    let deployment = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;

    assert_eq!(fake.attempts(DNS), 3);
    assert!(matches!(&outcomes[&d(DNS)], Outcome::Failed(e) if e.contains("after 3 attempts")));
    assert_eq!(
        outcomes[&d(ROUTE)],
        Outcome::Pending(format!("waiting for `{DNS}`"))
    );
    assert!(matches!(outcomes[&d(WORKLOAD)], Outcome::Pending(_)));
    assert_eq!(outcomes[&d(DB)], Outcome::Provisioned);
    assert_eq!(outcomes[&d(BASE)], Outcome::Provisioned);
    assert_eq!(fake.attempts(ROUTE), 0);

    let recorded = f.resources(&deployment).await;
    assert_eq!(recorded[DNS], "failed");
    assert_eq!(recorded[ROUTE], "pending");
    assert_eq!(recorded[WORKLOAD], "pending");
    assert_eq!(recorded[DB], "provisioned");
    let (status, error) = f.status(&deployment).await;
    assert_eq!(status, DeploymentStatus::Failed);
    let error = error.unwrap();
    assert!(error.contains(DNS), "{error}");
    assert!(error.contains("2 resource(s) not started"), "{error}");
    // The database's result is retained.
    let db = f
        .store
        .active_resource(ORG, "shop", "development", DB)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(db.outputs["host"], "postgres.internal");
}

#[tokio::test]
async fn independent_branches_run_in_parallel() {
    let f = Fixture::new().await;
    let barrier = Arc::new(Barrier::new(2));
    let fake = Fake::with(&[
        ("dns", Behavior::Meet(barrier.clone())),
        ("postgres", Behavior::Meet(barrier)),
    ]);
    let deployment = f.accept().await;
    let planner = planner(&f, &fake, fast());
    let run = planner.run(deployment.clone());
    // Each branch waits for the other, so this finishes only if they overlap.
    let outcomes = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the branches ran one after another");
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
    assert!(fake.most.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );

    // Dependents run only after their dependencies and see their outputs.
    let order: Vec<String> = fake
        .jobs
        .lock()
        .unwrap()
        .iter()
        .map(|j| j.descriptor.to_string())
        .collect();
    let pos = |s: &str| order.iter().position(|x| x == s).unwrap();
    assert!(pos(DNS) < pos(ROUTE));
    assert!(pos(ROUTE) < pos(WORKLOAD) && pos(DB) < pos(WORKLOAD));
    assert_eq!(
        fake.job(ROUTE).inputs.values,
        json!({"host": "dns.internal"})
    );
}

#[tokio::test]
async fn concurrency_is_bounded() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let deployment = f.accept().await;
    let limits = Limits {
        concurrency: 1,
        ..fast()
    };
    let outcomes = planner(&f, &fake, limits).run(deployment).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    assert_eq!(fake.most.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn permanent_errors_are_not_retried() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Permanent)]);
    let deployment = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert_eq!(fake.attempts(DNS), 1);
    assert_eq!(
        outcomes[&d(DNS)],
        Outcome::Failed("400: invalid zone".into())
    );
    assert_eq!(f.resources(&deployment).await[DNS], "failed");
}

#[tokio::test]
async fn a_retry_hint_past_the_deadline_stops_retrying() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Retryable(Some(Duration::from_secs(3600))))]);
    let deployment = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(deployment).await;
    assert_eq!(fake.attempts(DNS), 1);
    assert!(
        matches!(&outcomes[&d(DNS)], Outcome::Failed(e) if e.contains("no time left")),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn retries_keep_the_resource_identity() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Retryable(None))]);
    let deployment = f.accept().await;
    planner(&f, &fake, fast()).run(deployment).await;
    let jobs: Vec<ProvisionJob> = fake
        .jobs
        .lock()
        .unwrap()
        .iter()
        .filter(|j| j.descriptor == d(DNS))
        .cloned()
        .collect();
    assert_eq!(
        jobs.iter().map(|j| j.attempt).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(jobs.iter().all(|j| j.guresid == jobs[0].guresid));
    assert!(
        jobs.iter()
            .all(|j| j.input_fingerprint == jobs[0].input_fingerprint)
    );
    let ids: BTreeSet<&str> = jobs.iter().map(|j| j.operation_id.as_str()).collect();
    assert_eq!(ids.len(), 3);
}

#[tokio::test]
async fn a_timeout_leaves_the_resource_unknown_until_recovered() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Hang)]);
    let deployment = f.accept().await;
    let limits = Limits {
        deadline: Duration::from_millis(500),
        ..fast()
    };
    let outcomes = planner(&f, &fake, limits).run(deployment.clone()).await;
    assert!(
        matches!(outcomes[&d(DNS)], Outcome::Unknown(_)),
        "{outcomes:?}"
    );
    assert_eq!(f.resources(&deployment).await[DNS], "unknown");
    let (status, error) = f.status(&deployment).await;
    assert_eq!(status, DeploymentStatus::Failed);
    assert!(error.unwrap().contains("needs recovery"));

    // A new deployment cannot mutate the resource until its outcome is resolved.
    assert!(matches!(
        f.try_accept().await,
        Err(StoreError::RecoveryRequired(r)) if r == DNS
    ));
}

#[tokio::test]
async fn no_secret_value_reaches_the_planner() {
    let f = Fixture::new().await;
    let secrets = BuiltinSecretStore::new(
        &f.store,
        Arc::new(Keyring::new(MasterKey::from_bytes([7; 32]), [])),
        ORG,
    );
    secrets
        .write("pg-master", &SecretValue::new(MASTER))
        .await
        .unwrap();
    let fake = Arc::new(Fake {
        secrets: Some(secrets),
        ..Fake::default()
    });
    let deployment = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );

    // Secret inputs reach the executor as references, which only it reads.
    let db = fake.job(DB);
    let Resolved::Object(refs) = &db.inputs.secret_refs else {
        panic!("{:?}", db.inputs.secret_refs);
    };
    assert_eq!(refs[0].0, "master");
    assert!(matches!(&refs[0].1, Resolved::Secret(e)
        if matches!(&e.parts[..], [SecretPart::Secret(r)] if r.reference == "pg-master")));
    assert_eq!(db.inputs.values["name"], "shop-modules.web.externals.db");
    assert_eq!(db.inputs.values["id"], db.guresid.to_string());

    // A secret output stays a reference in a dependent's inputs.
    let route = fake.job(ROUTE);
    let Resolved::Object(secrets) = &route.inputs.secrets else {
        panic!("{:?}", route.inputs.secrets);
    };
    assert_eq!(secrets[0].0, "auth");
    assert!(
        matches!(&secrets[0].1, Resolved::Secret(e) if matches!(&e.parts[..],
        [SecretPart::Literal(l), SecretPart::Secret(r)] if l == "Bearer " && r.reference == "dns-token"))
    );
    let mut read = fake.read.lock().unwrap().clone();
    read.sort();
    assert_eq!(read, [MASTER, TOKEN]);

    // The plaintexts are in no job, outcome or deployment record.
    let jobs = fake.jobs.lock().unwrap().clone();
    let mut seen = format!("{jobs:?} {outcomes:?} {:?}", f.status(&deployment).await);
    for job in &jobs {
        seen.push_str(&serde_json::to_string(&job.inputs).unwrap());
    }
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT COALESCE(outputs, '') || COALESCE(error, '') FROM deployment_resources",
    )
    .fetch_all(f.store.pool())
    .await
    .unwrap();
    seen.extend(rows.into_iter().map(|(r,)| r));
    for plaintext in [MASTER, PASSWORD, TOKEN] {
        assert!(!seen.contains(plaintext), "{plaintext}");
    }
}

#[tokio::test]
async fn an_executor_panic_leaves_the_resource_unknown() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Panic)]);
    let deployment = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert!(
        matches!(outcomes[&d(DNS)], Outcome::Unknown(_)),
        "{outcomes:?}"
    );
    assert_eq!(f.resources(&deployment).await[DNS], "unknown");
    assert_eq!(outcomes[&d(DB)], Outcome::Provisioned);
    assert!(matches!(
        f.try_accept().await,
        Err(StoreError::RecoveryRequired(r)) if r == DNS
    ));
}

#[test]
fn backoff_is_jittered_exponential_and_capped() {
    let limits = Limits::default();
    for _ in 0..100 {
        let first = backoff(&limits, 1);
        assert!(first >= Duration::from_millis(500) && first <= Duration::from_secs(1));
        let second = backoff(&limits, 2);
        assert!(second >= Duration::from_secs(1) && second <= Duration::from_secs(2));
        let late = backoff(&limits, 10);
        assert!(late >= Duration::from_secs(15) && late <= Duration::from_secs(30));
    }
}

#[test]
fn secret_references_mirror_the_input_structure() {
    let r = references(
        &json!({"a": 1, "b": {"store": "openporch", "ref": "x"}, "c": [{"store": "s", "ref": "y"}]}),
    );
    let Resolved::Object(items) = r else {
        panic!("{r:?}")
    };
    assert_eq!(items[0], ("a".to_owned(), Resolved::Value(json!(1))));
    assert!(matches!(items[1].1, Resolved::Secret(_)));
    assert!(matches!(&items[2].1, Resolved::Array(a) if matches!(a[0], Resolved::Secret(_))));
}
