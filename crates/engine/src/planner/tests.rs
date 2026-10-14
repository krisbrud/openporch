use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use openporch_core::catalog::{ClassRegistry, ResourceClass, builtin_types};
use openporch_core::graph::{GraphInput, build_graph};
use openporch_core::placeholder::SecretPart;
use openporch_core::sets::DeploymentSet;
use openporch_core::workload::Workload;
use openporch_store::secrets::{
    BuiltinSecretStore, Keyring, MasterKey, SecretStore as _, SecretValue,
};
use openporch_store::{
    AuditIntent, DeploymentKind, DeploymentRequest, DeploymentStatus, Ownership, SqliteStore,
};
use openporch_store::{Detach, NewEnv, Tombstone};
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

/// `web` without its database.
const WEB_NO_DB: &str = r#"
apiVersion: openporch.dev/v1
kind: Workload
metadata: {name: web}
containers:
  web:
    image: nginx
resources:
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

/// The classes defined besides `default`.
fn classes() -> ClassRegistry {
    ClassRegistry::new([ResourceClass {
        id: "aws-lambda".parse().unwrap(),
        resource_type: "workload".parse().unwrap(),
        description: String::new(),
    }])
}

fn definitions() -> Vec<ResourceDefinition> {
    let def = |id: &str, ty: &str, inputs: Value| {
        let mut def = json!({
            "id": id, "type": ty, "driver_type": "openporch/echo",
            "driver_inputs": inputs, "criteria": [{}],
        });
        // DNS runs in an account, and its route in the same one.
        match ty {
            "dns" => def["driver_account"] = json!("aws-dns"),
            "route" => def["driver_account"] = json!("${resources.dns.account}"),
            _ => {}
        }
        serde_json::from_value(def).unwrap()
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
    /// Fails with the secret values it read in its error, as a careless driver
    /// would.
    Leak,
    /// Takes effect in the cloud, then never answers, as when the server
    /// stops during the call.
    Crash,
}

/// An executor that provisions nothing and records every job it is handed.
/// With a secret store, it reads every secret input and writes secret outputs
/// as a real executor does.
#[derive(Default)]
struct Fake {
    behavior: Mutex<BTreeMap<String, Behavior>>,
    /// The artifact checks asked for, and the artifacts that are gone.
    artifact_checks: Mutex<Vec<ArtifactCheck>>,
    gone: Mutex<BTreeSet<String>>,
    /// How many artifact checks fail in a way that may pass before one
    /// answers.
    artifact_flakes: Mutex<usize>,
    jobs: Mutex<Vec<ProvisionJob>>,
    running: AtomicUsize,
    most: AtomicUsize,
    secrets: Option<BuiltinSecretStore>,
    read: Mutex<Vec<String>>,
    /// How deletions answer, by resource type.
    deletions: Mutex<BTreeMap<String, Behavior>>,
    /// Every deletion job, in the order they started.
    deleted: Mutex<Vec<DeprovisionJob>>,
    /// The cloud objects that exist: descriptor by GUResID.
    objects: Mutex<BTreeMap<String, String>>,
    /// Every object created, as descriptor and GUResID, in order.
    created: Mutex<Vec<(String, String)>>,
    /// How observations answer, by resource type, instead of from the
    /// objects.
    observations: Mutex<BTreeMap<String, Observed>>,
    /// The descriptors observed, in order.
    observed: Mutex<Vec<String>>,
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

    /// Creates the object for `guresid` unless it exists: puts are
    /// idempotent per GUResID.
    fn create(&self, descriptor: &str, guresid: &str) {
        let mut objects = self.objects.lock().unwrap();
        if !objects.contains_key(guresid) {
            objects.insert(guresid.to_owned(), descriptor.to_owned());
            self.created
                .lock()
                .unwrap()
                .push((descriptor.to_owned(), guresid.to_owned()));
        }
    }

    /// How many objects were ever created for `descriptor`.
    fn creations(&self, descriptor: &str) -> usize {
        let created = self.created.lock().unwrap();
        created.iter().filter(|(d, _)| d == descriptor).count()
    }

    /// The result of provisioning a resource of type `ty`.
    async fn result(&self, ty: &str, guresid: &str) -> ProvisionResult {
        let mut provisioned = ProvisionResult::default();
        provisioned
            .outputs
            .insert("host".into(), json!(format!("{ty}.internal")));
        if ty == "dns" {
            provisioned.target = Some("111122223333".into());
        }
        provisioned.recovery = json!({"state": format!("{ty}-state")});
        provisioned.backend = Some(format!("s3://state/{guresid}"));
        provisioned.external_ids = vec![format!("arn:aws:{ty}:{guresid}")];
        provisioned.pin = Some(json!({"commit": format!("{ty}-commit")}));
        provisioned.cookie = Some(Sealed {
            key_id: "k1".into(),
            nonce: vec![0; 12],
            ciphertext: format!("{ty}-cookie").into_bytes(),
        });
        let secret = match ty {
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
        provisioned
    }

    fn job(&self, descriptor: &str) -> ProvisionJob {
        let jobs = self.jobs.lock().unwrap();
        jobs.iter()
            .find(|j| j.descriptor.to_string() == descriptor)
            .cloned()
            .unwrap()
    }
}

/// The plain part of `resolved`, collecting each secret inside under its
/// dotted path. A secret made of one reference keeps it.
fn none_outputs(
    resolved: &Resolved,
    path: &str,
    secrets: &mut BTreeMap<String, SecretRef>,
) -> Value {
    let at = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };
    match resolved {
        Resolved::Value(v) => v.clone(),
        Resolved::Secret(e) => {
            if let [SecretPart::Secret(r)] = &e.parts[..] {
                secrets.insert(path.to_owned(), r.clone());
            }
            Value::Null
        }
        Resolved::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, r)| none_outputs(r, &at(&i.to_string()), secrets))
                .collect(),
        ),
        Resolved::Object(fields) => {
            let mut values = serde_json::Map::new();
            for (k, r) in fields {
                let value = none_outputs(r, &at(k), secrets);
                if !matches!(r, Resolved::Secret(_)) {
                    values.insert(k.clone(), value);
                }
            }
            Value::Object(values)
        }
    }
}

impl Executor for Fake {
    async fn check_artifacts(&self, check: ArtifactCheck) -> Result<(), ExecFailure> {
        self.artifact_checks.lock().unwrap().push(check.clone());
        {
            let mut flakes = self.artifact_flakes.lock().unwrap();
            if *flakes > 0 {
                *flakes -= 1;
                return Err(ExecFailure::Retryable {
                    error: "S3 is slow down".into(),
                    retry_after: None,
                });
            }
        }
        let gone = self.gone.lock().unwrap();
        match check.artifacts.iter().find(|a| gone.contains(*a)) {
            Some(a) => Err(ExecFailure::Permanent(format!(
                "artifact `{a}` no longer exists"
            ))),
            None => Ok(()),
        }
    }

    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let behavior = self
            .behavior
            .lock()
            .unwrap()
            .get(job.descriptor.ty.as_str())
            .cloned()
            .unwrap_or(Behavior::Succeed);
        let ty = job.descriptor.ty.to_string();
        let descriptor = job.descriptor.to_string();
        let guresid = job.guresid.clone();
        let driver_type = job.driver_type.to_string();
        let resource = job.inputs.resource.clone();
        let mut refs = Vec::new();
        for input in [
            &job.inputs.secrets,
            &job.inputs.secret_refs,
            &job.inputs.resource,
        ] {
            secret_refs(input, &mut refs);
        }
        let mut log = job.log.redacting();
        self.jobs.lock().unwrap().push(job);
        let mut values = Vec::new();
        if let Some(store) = &self.secrets {
            for r in refs {
                let value = store.read(&r).await.unwrap();
                self.read.lock().unwrap().push(value.expose().to_owned());
                values.push(value.expose().to_owned());
            }
        }
        // As a real executor does: register what it resolved, log, and hand
        // failures back redacted.
        log.secret(&values);
        for v in &values {
            log.info(&format!("connecting with {v}"));
            log.info(&format!("{{\"password\": {}}}", json!(v)));
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
            Behavior::Crash => {
                self.create(&descriptor, guresid.as_str());
                std::future::pending().await
            }
            Behavior::Panic => panic!("the driver crashed"),
            Behavior::Leak => Err(ExecFailure::Permanent(format!(
                "Error: authentication failed for {}",
                values.join(", ")
            ))),
        };
        self.running.fetch_sub(1, Ordering::SeqCst);
        result.map_err(|f| log.failure(f))?;
        self.create(&descriptor, guresid.as_str());
        let mut provisioned = ProvisionResult::default();
        // As `openporch/none` does through the executor: the resolved inputs
        // are the outputs, with what resolved from secrets kept as references.
        if driver_type == "openporch/none" {
            let mut secrets = BTreeMap::new();
            if let Value::Object(values) = none_outputs(&resource, "", &mut secrets) {
                provisioned.outputs = values;
            }
            provisioned.secret_outputs = secrets;
            return Ok(provisioned);
        }
        Ok(self.result(&ty, guresid.as_str()).await)
    }

    async fn deprovision(&self, job: DeprovisionJob) -> Result<(), ExecFailure> {
        let behavior = self
            .deletions
            .lock()
            .unwrap()
            .get(job.job.descriptor.ty.as_str())
            .cloned()
            .unwrap_or(Behavior::Succeed);
        let guresid = job.job.guresid.to_string();
        self.deleted.lock().unwrap().push(job);
        match behavior {
            Behavior::Succeed => {
                self.objects.lock().unwrap().remove(&guresid);
                Ok(())
            }
            Behavior::Crash => {
                self.objects.lock().unwrap().remove(&guresid);
                std::future::pending().await
            }
            Behavior::Meet(barrier) => {
                barrier.wait().await;
                Ok(())
            }
            Behavior::Retryable(retry_after) => Err(ExecFailure::Retryable {
                error: "503 from the database API".into(),
                retry_after,
            }),
            Behavior::Permanent | Behavior::Leak => Err(ExecFailure::Permanent(
                "409: the database has deletion protection".into(),
            )),
            Behavior::Hang => std::future::pending().await,
            Behavior::Panic => panic!("the driver crashed"),
        }
    }

    async fn observe(&self, job: ProvisionJob, _deleting: bool) -> Result<Observed, ExecFailure> {
        let ty = job.descriptor.ty.to_string();
        self.observed
            .lock()
            .unwrap()
            .push(job.descriptor.to_string());
        if let Some(observed) = self.observations.lock().unwrap().get(&ty) {
            return Ok(observed.clone());
        }
        let exists = self
            .objects
            .lock()
            .unwrap()
            .contains_key(job.guresid.as_str());
        Ok(if exists {
            Observed::Ready {
                result: Box::new(self.result(&ty, job.guresid.as_str()).await),
                repeatable: true,
            }
        } else {
            Observed::Absent { repeatable: true }
        })
    }
}

/// Allows every destructive action and records what it was asked.
#[derive(Debug, Default)]
struct AllowAll {
    asked: Mutex<Vec<(String, DestructiveAction, Vec<String>)>>,
}

impl PermitCheck for AllowAll {
    fn check<'a>(
        &'a self,
        req: PermitRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        self.asked.lock().unwrap().push((
            req.descriptor.to_owned(),
            req.action,
            req.permits.to_vec(),
        ));
        Box::pin(async { Ok(()) })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<SqliteStore>,
    /// The environment deployments go to.
    env: &'static str,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ownership = Ownership::acquire(dir.path()).unwrap();
        let store = SqliteStore::open(ownership, ORG).await.unwrap();
        for (id, role) in [
            ("alice", "manager"),
            ("ci", "member"),
            ("root", "administrator"),
        ] {
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
            env: "development",
        }
    }

    /// A fixture whose deployments go to a new `development`-type
    /// environment `env`.
    async fn in_env(env: &'static str) -> Self {
        let mut f = Self::new().await;
        f.store
            .create_env(
                ORG,
                "shop",
                NewEnv {
                    id: env.into(),
                    name: env.into(),
                    env_type: "development".into(),
                    draft: None,
                },
                audit("POST", "/orgs/acme/apps/shop/envs"),
            )
            .await
            .unwrap();
        f.env = env;
        f
    }

    /// Accepts a deployment of the `web` workload and reads it back from its
    /// snapshot, as the server's runner does.
    async fn accept(&self) -> Deployment {
        self.try_accept().await.unwrap()
    }

    async fn try_accept(&self) -> Result<Deployment, StoreError> {
        self.accept_with(definitions()).await
    }

    async fn accept_with(&self, defs: Vec<ResourceDefinition>) -> Result<Deployment, StoreError> {
        self.accept_set(Some(WEB), defs).await
    }

    /// Accepts a deployment of `workload`, or of no workload at all.
    async fn accept_set(
        &self,
        workload: Option<&str>,
        defs: Vec<ResourceDefinition>,
    ) -> Result<Deployment, StoreError> {
        self.accept_workloads(workload.as_slice(), defs).await
    }

    /// Accepts a deployment of `workloads`, added to the set in that order.
    async fn accept_workloads(
        &self,
        workloads: &[&str],
        defs: Vec<ResourceDefinition>,
    ) -> Result<Deployment, StoreError> {
        let mut set = DeploymentSet::default();
        for workload in workloads {
            let c = Workload::parse(workload, None)
                .unwrap()
                .to_module(&builtin_types())
                .unwrap();
            set.modules.insert(c.id, c.module);
        }
        let (org, app, env, env_type) = (
            ORG.parse().unwrap(),
            "shop".parse().unwrap(),
            self.env.parse().unwrap(),
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
            classes: &classes(),
        })
        .unwrap();
        let snapshot = json!({
            "version": 1,
            "mode": "full",
            "permits": ["permit-1"],
            "graph": graph,
            "definitions": defs
                .iter()
                .map(|d| (d.id.to_string(), json!({"version": 1, "definition": d})))
                .collect::<serde_json::Map<_, _>>(),
        });
        let set_id = self.store.put_set(ORG, "shop", &set).await.unwrap();
        let generation = self
            .store
            .env(ORG, "shop", self.env)
            .await
            .unwrap()
            .unwrap()
            .generation;
        let accepted = self
            .store
            .accept_deployment(DeploymentRequest {
                permits: Default::default(),
                org: ORG.into(),
                app: "shop".into(),
                env: self.env.into(),
                actor: "ci".into(),
                expected_generation: generation,
                idempotency_key: None,
                fingerprint: format!("body-{generation}"),
                kind: DeploymentKind::Deploy,
                set_id: Some(set_id.clone()),
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
            .deployment(ORG, "shop", self.env, &deployment.id)
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
    let route = fake.job(ROUTE);
    assert_eq!(route.inputs.values, json!({"host": "dns.internal"}));
    assert_eq!(route.driver_account.unwrap().as_str(), "aws-dns");
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
    // The operation kept what recovery needs to observe or repeat it.
    let (recovery,): (String,) = sqlx::query_as(
        "SELECT recovery FROM operations WHERE deployment_id = ? AND state = 'unknown'",
    )
    .bind(&deployment.id)
    .fetch_one(f.store.pool())
    .await
    .unwrap();
    let recovery: Value = serde_json::from_str(&recovery).unwrap();
    let job = fake.job(DNS);
    assert_eq!(recovery["guresid"], job.guresid.to_string());
    assert_eq!(recovery["driver_account"], "aws-dns");
    assert_eq!(
        recovery["inputs"],
        serde_json::to_value(&job.inputs).unwrap()
    );
    assert_eq!(recovery["definition_id"], "dns");
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
    // The executor learns that the reference is the DNS record's output, so
    // it authorizes the read as that resource's, and in which deployment.
    let dns = fake.job(DNS).guresid;
    assert!(
        matches!(&route.inputs.sources[..], [(r, SecretSource::Output { guresid })]
            if r.reference == "dns-token" && *guresid == dns),
        "{:?}",
        route.inputs.sources
    );
    assert_eq!(route.context, JobContext::of(&deployment));
    // The workload gets the database password with its dependencies'
    // outputs and in its variables.
    let mut read = fake.read.lock().unwrap().clone();
    read.sort();
    assert_eq!(read, [MASTER, PASSWORD, PASSWORD, TOKEN]);

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

#[tokio::test]
async fn a_resource_does_not_move_to_another_account() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let deployment = f.accept().await;
    planner(&f, &fake, fast()).run(deployment).await;
    let dns = f
        .store
        .active_resource(ORG, "shop", "development", DNS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dns.driver_account.as_deref(), Some("aws-dns"));

    let mut defs = definitions();
    for def in &mut defs {
        if def.id.as_str() == "dns" {
            def.driver_account = Some("aws-other".into());
        }
    }
    let next = f.accept_with(defs).await.unwrap();
    let outcomes = planner(&f, &fake, fast()).run(next).await;
    assert!(
        matches!(&outcomes[&d(DNS)], Outcome::Failed(e)
            if e.contains("cloud account `aws-dns`") && e.contains("`aws-other`")),
        "{outcomes:?}"
    );
    assert_eq!(fake.attempts(DNS), 1);
    let dns = f
        .store
        .active_resource(ORG, "shop", "development", DNS)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dns.driver_account.as_deref(), Some("aws-dns"));
}

#[tokio::test]
async fn the_executor_learns_the_previous_target() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let first = f.accept().await;
    planner(&f, &fake, fast()).run(first).await;
    assert_eq!(fake.job(DNS).target, None);
    let next = f.accept().await;
    planner(&f, &fake, fast()).run(next).await;
    let jobs = fake.jobs.lock().unwrap().clone();
    let dns: Vec<_> = jobs.iter().filter(|j| j.descriptor == d(DNS)).collect();
    assert_eq!(dns.len(), 2);
    assert_eq!(dns[1].target.as_deref(), Some("111122223333"));
}

fn deleting(f: &Fixture, fake: &Arc<Fake>, checks: &Arc<AllowAll>) -> Planner<SqliteStore, Fake> {
    planner(f, fake, fast()).with_permit_check(checks.clone())
}

impl Fixture {
    async fn schedule(&self, descriptor: &str) {
        self.store
            .schedule_deletion(
                ORG,
                "shop",
                self.env,
                descriptor,
                audit("DELETE", "/orgs/acme/apps/shop/envs/development/resources"),
            )
            .await
            .unwrap();
    }

    async fn active(&self, descriptor: &str) -> Option<openporch_store::ActiveResource> {
        self.store
            .active_resource(ORG, "shop", self.env, descriptor)
            .await
            .unwrap()
    }

    /// `(descriptor, guresid, reason)` of every tombstone.
    async fn tombstones(&self) -> Vec<(String, String, String)> {
        sqlx::query_as("SELECT descriptor, guresid, reason FROM tombstones ORDER BY descriptor")
            .fetch_all(self.store.pool())
            .await
            .unwrap()
    }
}

impl Fake {
    /// The descriptors of deletion jobs, in the order they started.
    fn deletions(&self) -> Vec<String> {
        let jobs = self.deleted.lock().unwrap();
        jobs.iter().map(|j| j.job.descriptor.to_string()).collect()
    }
}

#[tokio::test]
async fn a_scheduled_resource_is_deleted_by_the_next_deployment_without_it() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    let db = f.active(DB).await.unwrap();
    let provisioned = fake.job(DB);

    // Left out but not scheduled: retained.
    let retained = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(retained).await;
    assert!(!outcomes.contains_key(&d(DB)));
    assert!(f.active(DB).await.is_some());
    assert!(fake.deletions().is_empty());

    f.schedule(DB).await;
    assert!(f.active(DB).await.unwrap().scheduled_deletion);
    let deployment = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert_eq!(outcomes[&d(DB)], Outcome::Deleted);
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );
    assert_eq!(f.resources(&deployment).await[DB], "deleted");

    // The driver deleted it with the identity and inputs it was provisioned with.
    let deleted = fake.deleted.lock().unwrap().clone();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].job.guresid.as_str(), db.guresid);
    assert_eq!(deleted[0].job.inputs, provisioned.inputs);
    assert_eq!(deleted[0].job.driver_type, "openporch/echo");
    // It also got the definition and driver state the resource retained.
    assert_eq!(deleted[0].snapshot, db.snapshot);
    assert_eq!(deleted[0].snapshot["definition"]["id"], "pg");
    assert_eq!(deleted[0].recovery, json!({"state": "postgres-state"}));
    assert_eq!(
        checks.asked.lock().unwrap().clone(),
        vec![(
            DB.to_owned(),
            DestructiveAction::Delete,
            vec!["permit-1".to_owned()]
        )]
    );
    assert!(f.active(DB).await.is_none());
    assert_eq!(
        f.tombstones().await,
        vec![(DB.to_owned(), db.guresid, "deleted".to_owned())]
    );
}

#[tokio::test]
async fn a_removed_workload_is_deleted_before_what_it_used() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    for descriptor in [DB, ROUTE, DNS] {
        f.schedule(descriptor).await;
    }
    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    for descriptor in [WORKLOAD, DB, ROUTE, DNS] {
        assert_eq!(outcomes[&d(descriptor)], Outcome::Deleted, "{descriptor}");
        assert!(f.active(descriptor).await.is_none(), "{descriptor}");
    }
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );
    let order = fake.deletions();
    let at = |descriptor: &str| order.iter().position(|d| d == descriptor).unwrap();
    assert_eq!(order.len(), 4);
    assert!(
        at(WORKLOAD) < at(DB) && at(WORKLOAD) < at(ROUTE),
        "{order:?}"
    );
    assert!(at(ROUTE) < at(DNS), "{order:?}");
    assert!(f.active(BASE).await.is_some());
}

#[tokio::test]
async fn a_removed_workload_keeps_its_unscheduled_dependencies() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // DNS is scheduled, but the retained route still uses it.
    f.schedule(DNS).await;
    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert_eq!(outcomes[&d(WORKLOAD)], Outcome::Deleted);
    assert_eq!(
        outcomes[&d(DNS)],
        Outcome::Kept(format!(
            "scheduled for deletion, but still used by `{ROUTE}`"
        ))
    );
    assert!(!outcomes.contains_key(&d(DB)));
    assert!(!outcomes.contains_key(&d(ROUTE)));
    assert_eq!(fake.deletions(), vec![WORKLOAD.to_owned()]);
    for descriptor in [DB, ROUTE, DNS] {
        assert!(f.active(descriptor).await.is_some(), "{descriptor}");
    }
    // Keeping it does not fail the deployment.
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );
    assert_eq!(f.resources(&deployment).await[DNS], "skipped");
}

#[tokio::test]
async fn a_failed_deletion_blocks_what_it_uses_until_retried() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    fake.deletions
        .lock()
        .unwrap()
        .insert("route".into(), Behavior::Retryable(None));
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    let route = f.active(ROUTE).await.unwrap();
    f.schedule(ROUTE).await;
    f.schedule(DNS).await;

    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert_eq!(outcomes[&d(WORKLOAD)], Outcome::Deleted);
    assert!(
        matches!(&outcomes[&d(ROUTE)], Outcome::Failed(e) if e.contains("after 3 attempts")),
        "{outcomes:?}"
    );
    assert_eq!(
        outcomes[&d(DNS)],
        Outcome::Pending(format!("waiting for `{ROUTE}` to be deleted"))
    );
    let (status, error) = f.status(&deployment).await;
    assert_eq!(status, DeploymentStatus::Failed);
    assert!(error.unwrap().contains(ROUTE));
    assert_eq!(f.active(ROUTE).await.unwrap().status, "deletion_failed");
    assert!(f.active(DNS).await.is_some());
    assert!(f.active(WORKLOAD).await.is_none());

    // The retry deletes the same route and then DNS, and leaves the deleted
    // workload alone.
    fake.deletions.lock().unwrap().clear();
    let retry = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(retry.clone()).await;
    assert_eq!(outcomes[&d(ROUTE)], Outcome::Deleted);
    assert_eq!(outcomes[&d(DNS)], Outcome::Deleted);
    assert!(!outcomes.contains_key(&d(WORKLOAD)));
    assert_eq!(f.status(&retry).await, (DeploymentStatus::Succeeded, None));
    let deleted = fake.deleted.lock().unwrap().clone();
    let routes: Vec<_> = deleted
        .iter()
        .filter(|j| j.job.descriptor == d(ROUTE))
        .collect();
    assert_eq!(routes.len(), 4);
    assert!(
        routes
            .iter()
            .all(|j| j.job.guresid.as_str() == route.guresid)
    );
    assert_eq!(
        fake.deletions().iter().filter(|d| *d == WORKLOAD).count(),
        1
    );
    assert_eq!(fake.deletions().last().map(String::as_str), Some(DNS));
}

#[tokio::test]
async fn deletion_without_a_permit_needs_approval() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    planner(&f, &fake, fast()).run(f.accept().await).await;
    f.schedule(DB).await;
    let deployment = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert!(
        matches!(&outcomes[&d(DB)], Outcome::Failed(e) if e.starts_with("approval_required")),
        "{outcomes:?}"
    );
    assert!(fake.deletions().is_empty());
    assert!(f.active(DB).await.is_some());
    assert_eq!(f.status(&deployment).await.0, DeploymentStatus::Failed);
}

#[tokio::test]
async fn adopted_and_external_resources_are_not_deleted() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    sqlx::query("UPDATE active_resources SET ownership = 'external' WHERE descriptor = ?")
        .bind(DB)
        .execute(f.store.pool())
        .await
        .unwrap();
    let refused = f
        .store
        .schedule_deletion(ORG, "shop", "development", DB, audit("DELETE", "/"))
        .await;
    assert!(
        matches!(refused, Err(StoreError::Conflict(_))),
        "{refused:?}"
    );
    // Even when marked by other means, an external resource stays; an
    // adopted one stays unless scheduled.
    sqlx::query("UPDATE active_resources SET scheduled_deletion = 1 WHERE descriptor = ?")
        .bind(DB)
        .execute(f.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE active_resources SET ownership = 'adopted' WHERE descriptor IN (?, ?)")
        .bind(ROUTE)
        .bind(DNS)
        .execute(f.store.pool())
        .await
        .unwrap();
    f.schedule(DNS).await;

    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment).await;
    assert_eq!(outcomes[&d(WORKLOAD)], Outcome::Deleted);
    assert!(!outcomes.contains_key(&d(DB)));
    assert!(!outcomes.contains_key(&d(ROUTE)));
    // The scheduled adopted DNS stays while the kept route uses it.
    assert_eq!(
        outcomes[&d(DNS)],
        Outcome::Kept(format!(
            "scheduled for deletion, but still used by `{ROUTE}`"
        ))
    );
    assert_eq!(fake.deletions(), vec![WORKLOAD.to_owned()]);
    for descriptor in [DB, ROUTE, DNS] {
        assert!(f.active(descriptor).await.is_some(), "{descriptor}");
    }
}

#[tokio::test]
async fn nothing_is_deleted_while_the_deployment_has_not_converged() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    f.schedule(DB).await;
    fake.behavior
        .lock()
        .unwrap()
        .insert("dns".into(), Behavior::Permanent);
    let deployment = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment).await;
    assert!(
        matches!(outcomes[&d(DB)], Outcome::Pending(_)),
        "{outcomes:?}"
    );
    assert!(fake.deletions().is_empty());
    assert!(checks.asked.lock().unwrap().is_empty());
    assert!(f.active(DB).await.is_some());
}

#[tokio::test]
async fn an_uncertain_deletion_needs_recovery() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    fake.deletions
        .lock()
        .unwrap()
        .insert("postgres".into(), Behavior::Panic);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    f.schedule(DB).await;
    let deployment = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert!(
        matches!(outcomes[&d(DB)], Outcome::Unknown(_)),
        "{outcomes:?}"
    );
    assert_eq!(f.active(DB).await.unwrap().status, "unknown");
    assert_eq!(f.status(&deployment).await.0, DeploymentStatus::Failed);
    let next = f.accept_set(Some(WEB_NO_DB), definitions()).await;
    assert!(
        matches!(next, Err(StoreError::RecoveryRequired(_))),
        "{next:?}"
    );
}

#[tokio::test]
async fn scheduling_advances_the_generation_and_waits_for_a_running_deployment() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    planner(&f, &fake, fast()).run(f.accept().await).await;
    let generation = |f: &Fixture| {
        let store = f.store.clone();
        async move {
            store
                .env(ORG, "shop", "development")
                .await
                .unwrap()
                .unwrap()
                .generation
        }
    };
    let before = generation(&f).await;
    f.schedule(DB).await;
    assert_eq!(generation(&f).await, before + 1);
    // A retry changes nothing.
    f.schedule(DB).await;
    assert_eq!(generation(&f).await, before + 1);

    // Accepted but not yet run: the deployment is in progress.
    let running = f.accept().await;
    let refused = f
        .store
        .schedule_deletion(ORG, "shop", "development", ROUTE, audit("DELETE", "/"))
        .await;
    assert!(
        matches!(refused, Err(StoreError::Busy(ref id)) if *id == running.id),
        "{refused:?}"
    );
    assert!(!f.active(ROUTE).await.unwrap().scheduled_deletion);
}

#[tokio::test]
async fn a_confirmed_deletion_is_finished_without_the_driver() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // An earlier attempt of this deployment deleted the database, but its
    // record could not be removed.
    let deployment = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let started = f
        .store
        .begin_operation(OperationStart {
            org: ORG.into(),
            app: "shop".into(),
            env: "development".into(),
            deployment_id: deployment.id.clone(),
            descriptor: d(DB),
            definition_id: None,
            definition_version: None,
            driver_type: None,
            kind: OperationKind::Delete,
            input_fingerprint: "delete".into(),
            recovery: json!({}),
        })
        .await
        .unwrap();
    f.store
        .checkpoint_operation(
            &started.operation_id,
            CheckpointResult::CleanupPending {
                error: "database is locked".into(),
            },
        )
        .await
        .unwrap();
    let db = f.active(DB).await.unwrap();
    assert!(db.cleanup_pending);
    assert_eq!(db.status, "deletion_failed");

    // A restart while the record is being removed keeps it marked for cleanup
    // instead of leaving its outcome unknown.
    let retry = f
        .store
        .begin_operation(OperationStart {
            org: ORG.into(),
            app: "shop".into(),
            env: "development".into(),
            deployment_id: deployment.id.clone(),
            descriptor: d(DB),
            definition_id: None,
            definition_version: None,
            driver_type: None,
            kind: OperationKind::Delete,
            input_fingerprint: "delete".into(),
            recovery: json!({}),
        })
        .await
        .unwrap();
    let report = f.store.recover_interrupted().await.unwrap();
    assert!(report.uncertain.is_empty(), "{report:?}");
    assert!(report.failed_deployments.contains(&deployment.id));
    let db = f.active(DB).await.unwrap();
    assert!(db.cleanup_pending);
    assert_eq!(db.status, "deletion_failed");
    drop(retry);
    let deployment = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();

    // It was never scheduled, yet the deployment removes the record without
    // asking the driver or a permit again.
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert_eq!(outcomes[&d(DB)], Outcome::Deleted);
    assert!(fake.deletions().is_empty());
    assert!(checks.asked.lock().unwrap().is_empty());
    assert!(f.active(DB).await.is_none());
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );
}

/// Opens a deletion of `descriptor` in `deployment` and leaves it open, as when
/// its result could not be checkpointed.
async fn open_deletion(f: &Fixture, deployment: &Deployment, descriptor: &str) {
    f.store
        .begin_operation(OperationStart {
            org: ORG.into(),
            app: "shop".into(),
            env: "development".into(),
            deployment_id: deployment.id.clone(),
            descriptor: d(descriptor),
            definition_id: None,
            definition_version: None,
            driver_type: None,
            kind: OperationKind::Delete,
            input_fingerprint: "delete".into(),
            recovery: json!({}),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn an_operation_left_open_by_a_finished_deployment_is_closed() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    f.schedule(DB).await;
    // The deletion's result was never recorded, so its deployment ends with
    // the operation still open.
    let first = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    open_deletion(&f, &first, DB).await;
    deleting(&f, &fake, &checks).run(first.clone()).await;
    assert_eq!(f.status(&first).await.0, DeploymentStatus::Failed);

    // The next deployment does not wait for a restart: the outcome is unknown,
    // and the resource needs recovery.
    let next = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(next).await;
    let Outcome::Failed(error) = &outcomes[&d(DB)] else {
        panic!("{outcomes:?}");
    };
    assert!(error.contains("recover the resource first"), "{error}");
    assert_eq!(f.active(DB).await.unwrap().status, "unknown");
    assert!(fake.deletions().is_empty());

    // Removing a record whose deletion the driver confirmed is retried instead.
    let f = Fixture::new().await;
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    let first = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    open_deletion(&f, &first, DB).await;
    sqlx::query("UPDATE active_resources SET cleanup_pending = 1 WHERE descriptor = ?")
        .bind(DB)
        .execute(f.store.pool())
        .await
        .unwrap();
    deleting(&f, &fake, &checks).run(first).await;
    let next = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(next.clone()).await;
    assert_eq!(outcomes[&d(DB)], Outcome::Deleted);
    assert!(f.active(DB).await.is_none());
    assert!(fake.deletions().is_empty());
    assert_eq!(f.status(&next).await, (DeploymentStatus::Succeeded, None));
}

#[tokio::test]
async fn a_workload_that_never_provisioned_is_deleted_by_its_identity() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("workload", Behavior::Permanent)]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    let workload = f.active(WORKLOAD).await.unwrap();
    assert_eq!(workload.status, "failed");
    assert!(workload.last_deploy_id.is_none());

    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment).await;
    assert_eq!(outcomes[&d(WORKLOAD)], Outcome::Deleted);
    let deleted = fake.deleted.lock().unwrap().clone();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].job.guresid.as_str(), workload.guresid);
    assert_eq!(deleted[0].job.inputs.values, Value::Null);
}

#[tokio::test]
async fn a_chain_under_a_kept_resource_is_kept() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // The removed workload is adopted and not scheduled, so it stays, and with
    // it the scheduled route it uses and the scheduled DNS the route uses.
    sqlx::query("UPDATE active_resources SET ownership = 'adopted' WHERE descriptor = ?")
        .bind(WORKLOAD)
        .execute(f.store.pool())
        .await
        .unwrap();
    f.schedule(ROUTE).await;
    f.schedule(DNS).await;
    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert!(!outcomes.contains_key(&d(WORKLOAD)));
    assert_eq!(
        outcomes[&d(ROUTE)],
        Outcome::Kept(format!(
            "scheduled for deletion, but still used by `{WORKLOAD}`"
        ))
    );
    assert_eq!(
        outcomes[&d(DNS)],
        Outcome::Kept(format!(
            "scheduled for deletion, but still used by `{ROUTE}`"
        ))
    );
    assert!(fake.deletions().is_empty());
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );
}

#[tokio::test]
async fn a_removed_workload_takes_what_it_owns_with_it() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // Pretend the database was provisioned with the workload and depends on it,
    // as a co-provisioned trigger would.
    let redo = |descriptor: &str, deps: Value| {
        let pool = f.store.pool().clone();
        let descriptor = descriptor.to_owned();
        async move {
            sqlx::query("UPDATE active_resources SET dependencies = ? WHERE descriptor = ?")
                .bind(deps.to_string())
                .bind(descriptor)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    redo(WORKLOAD, json!([ROUTE])).await;
    redo(DB, json!([WORKLOAD])).await;
    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert_eq!(outcomes[&d(DB)], Outcome::Deleted);
    assert_eq!(outcomes[&d(WORKLOAD)], Outcome::Deleted);
    assert_eq!(fake.deletions(), vec![DB.to_owned(), WORKLOAD.to_owned()]);
    assert!(f.active(ROUTE).await.is_some());
    assert_eq!(
        f.status(&deployment).await,
        (DeploymentStatus::Succeeded, None)
    );
}

#[tokio::test]
async fn a_removed_workload_that_something_kept_uses_fails_the_deployment() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    sqlx::query(
        "UPDATE active_resources SET dependencies = ?, ownership = 'adopted' WHERE descriptor = ?",
    )
    .bind(json!([WORKLOAD]).to_string())
    .bind(DB)
    .execute(f.store.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE active_resources SET dependencies = '[]' WHERE descriptor = ?")
        .bind(WORKLOAD)
        .execute(f.store.pool())
        .await
        .unwrap();
    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment.clone()).await;
    assert_eq!(
        outcomes[&d(WORKLOAD)],
        Outcome::Pending(format!(
            "still used by `{DB}`, which stays; schedule it for deletion too"
        ))
    );
    assert!(fake.deletions().is_empty());
    assert_eq!(f.status(&deployment).await.0, DeploymentStatus::Failed);
}

#[tokio::test]
async fn deletion_keeps_the_drivers_recovery_metadata_and_ignores_lookalikes() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    fake.deletions
        .lock()
        .unwrap()
        .insert("postgres".into(), Behavior::Permanent);
    let checks = Arc::new(AllowAll::default());
    deleting(&f, &fake, &checks).run(f.accept().await).await;
    // Driver recovery metadata that happens to look like the cleanup marker.
    let recovery = json!({"cleanup_pending": true, "plan": "tfplan-7"});
    sqlx::query("UPDATE active_resources SET recovery = ? WHERE descriptor = ?")
        .bind(recovery.to_string())
        .bind(DB)
        .execute(f.store.pool())
        .await
        .unwrap();
    assert!(!f.active(DB).await.unwrap().cleanup_pending);
    f.schedule(DB).await;
    let deployment = f.accept_set(Some(WEB_NO_DB), definitions()).await.unwrap();
    let outcomes = deleting(&f, &fake, &checks).run(deployment).await;
    // The driver was asked and failed; nothing was removed without it.
    assert!(
        matches!(outcomes[&d(DB)], Outcome::Failed(_)),
        "{outcomes:?}"
    );
    assert_eq!(fake.deletions(), vec![DB.to_owned()]);
    let db = f.active(DB).await.unwrap();
    assert_eq!(db.status, "deletion_failed");
    let kept: String =
        sqlx::query_scalar("SELECT recovery FROM active_resources WHERE descriptor = ?")
            .bind(DB)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&kept).unwrap(), recovery);
}

#[tokio::test]
async fn a_detached_resource_is_not_provisioned_again() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    planner(&f, &fake, fast()).run(f.accept().await).await;
    let db = f.active(DB).await.unwrap();
    let detach = |actor: &str| {
        let store = f.store.clone();
        let guresid = db.guresid.clone();
        let audit = AuditIntent::accepted(ORG, actor, "POST", "/detach", "resource.detach");
        async move {
            store
                .detach_resource(
                    ORG,
                    "shop",
                    "development",
                    Detach {
                        permits: None,
                        descriptor: DB,
                        guresid: &guresid,
                        reason: "moved to a shared cluster",
                    },
                    audit,
                )
                .await
        }
    };
    let refused = detach("ci").await;
    assert!(
        matches!(refused, Err(StoreError::Forbidden(_))),
        "{refused:?}"
    );

    let tombstone = detach("root").await.unwrap();
    assert_eq!(tombstone.descriptor, DB);
    assert_eq!(tombstone.guresid, db.guresid);
    assert_eq!(tombstone.reason, "detached");
    assert_eq!(tombstone.actor.as_deref(), Some("root"));
    assert_eq!(tombstone.body["reason"], "moved to a shared cluster");
    assert_eq!(tombstone.body["driver_type"], "openporch/echo");
    // It keeps what a later adoption needs: the objects, where their state
    // is, the definition and the driver's recovery data and cookie.
    assert_eq!(
        tombstone.body["backend"],
        format!("s3://state/{}", db.guresid)
    );
    assert_eq!(
        tombstone.body["external_ids"],
        json!([format!("arn:aws:postgres:{}", db.guresid)])
    );
    assert_eq!(tombstone.body["snapshot"], db.snapshot);
    assert_eq!(
        tombstone.body["recovery"],
        json!({"state": "postgres-state"})
    );
    let (cookie,): (Option<Vec<u8>>,) =
        sqlx::query_as("SELECT cookie FROM tombstones WHERE guresid = ? AND reason = 'detached'")
            .bind(&db.guresid)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(cookie, Some(b"postgres-cookie".to_vec()));
    assert!(f.active(DB).await.is_none());
    assert!(fake.deletions().is_empty());

    // The next deployment does not create a second database.
    let next = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(next).await;
    assert!(
        matches!(&outcomes[&d(DB)], Outcome::Failed(e) if e.contains("detached")),
        "{outcomes:?}"
    );
    assert_eq!(fake.attempts(DB), 1);
    assert!(f.active(DB).await.is_none());
}

/// Detaches `descriptor` as the administrator.
async fn detach_as_root(f: &Fixture, descriptor: &str) -> Result<Tombstone, StoreError> {
    let guresid = f.active(descriptor).await.unwrap().guresid;
    let audit = AuditIntent::accepted(ORG, "root", "POST", "/detach", "resource.detach");
    f.store
        .detach_resource(
            ORG,
            "shop",
            "development",
            Detach {
                permits: None,
                descriptor,
                guresid: &guresid,
                reason: "gone",
            },
            audit,
        )
        .await
}

#[tokio::test]
async fn what_a_detached_resource_uses_is_kept() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    planner(&f, &fake, fast()).run(f.accept().await).await;
    let workload = f.active(WORKLOAD).await.unwrap();
    assert!(workload.dependencies.contains(&DB.to_owned()));
    detach_as_root(&f, WORKLOAD).await.unwrap();

    // The workload keeps running outside openporch, so its database stays even
    // when scheduled for deletion.
    f.schedule(DB).await;
    let deployment = f.accept_set(None, definitions()).await.unwrap();
    let checks = Arc::new(AllowAll::default());
    let outcomes = deleting(&f, &fake, &checks).run(deployment).await;
    assert_eq!(
        outcomes[&d(DB)],
        Outcome::Kept(format!(
            "scheduled for deletion, but still used by `{WORKLOAD} (detached)`"
        ))
    );
    assert!(!fake.deletions().contains(&DB.to_owned()));
    assert!(f.active(DB).await.is_some());
}

#[tokio::test]
async fn detach_is_refused_while_anything_is_in_flight_or_changed() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("workload", Behavior::Permanent)]);
    planner(&f, &fake, fast()).run(f.accept().await).await;

    // A resource that never provisioned has nothing to detach.
    let refused = detach_as_root(&f, WORKLOAD).await;
    assert!(
        matches!(&refused, Err(StoreError::Conflict(e)) if e.contains("never provisioned")),
        "{refused:?}"
    );

    // The permit was checked against another resource with the same descriptor.
    let audit = AuditIntent::accepted(ORG, "root", "POST", "/detach", "resource.detach");
    let refused = f
        .store
        .detach_resource(
            ORG,
            "shop",
            "development",
            Detach {
                permits: None,
                descriptor: DB,
                guresid: "older-guresid",
                reason: "gone",
            },
            audit,
        )
        .await;
    assert!(
        matches!(&refused, Err(StoreError::Conflict(e)) if e.contains("replaced")),
        "{refused:?}"
    );

    // A deployment is running.
    let running = f.accept().await;
    let refused = detach_as_root(&f, DB).await;
    assert!(matches!(refused, Err(StoreError::Busy(_))), "{refused:?}");

    // An operation on the resource is still open after its deployment ended.
    open_deletion(&f, &running, DB).await;
    planner(&f, &fake, fast()).run(running).await;
    let refused = detach_as_root(&f, DB).await;
    assert!(
        matches!(&refused, Err(StoreError::Conflict(e)) if e.contains("has not finished")),
        "{refused:?}"
    );
    assert!(f.active(DB).await.is_some());
}

#[tokio::test]
async fn a_deleted_resource_awaiting_cleanup_is_not_detached() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[]);
    planner(&f, &fake, fast()).run(f.accept().await).await;
    sqlx::query(
        "UPDATE active_resources SET cleanup_pending = 1, status = 'deletion_failed' \
         WHERE descriptor = ?",
    )
    .bind(DB)
    .execute(f.store.pool())
    .await
    .unwrap();
    let refused = detach_as_root(&f, DB).await;
    assert!(
        matches!(&refused, Err(StoreError::Conflict(e)) if e.contains("already deleted")),
        "{refused:?}"
    );
    assert!(f.active(DB).await.is_some());
}

#[tokio::test]
async fn a_resource_with_an_uncertain_operation_is_not_detached() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Panic)]);
    planner(&f, &fake, fast()).run(f.accept().await).await;
    let refused = detach_as_root(&f, DNS).await;
    assert!(
        matches!(refused, Err(StoreError::RecoveryRequired(_))),
        "{refused:?}"
    );
    assert!(f.active(DNS).await.is_some());
}

/// Every log line of `deployment`, as `(descriptor, operation, guresid, message)`.
async fn logs(f: &Fixture, deployment: &Deployment) -> Vec<openporch_store::LogLine> {
    f.store
        .deployment_logs(ORG, "shop", f.env, &deployment.id, None, 10_000)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_driver_error_with_a_secret_is_redacted() {
    // A value that JSON and HCL escape, so the escaped forms are covered too.
    const LEAKED: &str = r#"pa"ss\word-73"#;
    let f = Fixture::new().await;
    let secrets = BuiltinSecretStore::new(
        &f.store,
        Arc::new(Keyring::new(MasterKey::from_bytes([7; 32]), [])),
        ORG,
    );
    secrets
        .write("pg-master", &SecretValue::new(LEAKED))
        .await
        .unwrap();
    let fake = Arc::new(Fake {
        secrets: Some(secrets),
        ..Fake::default()
    });
    fake.behavior
        .lock()
        .unwrap()
        .insert("postgres".into(), Behavior::Leak);
    let deployment = f.accept().await;
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert!(
        matches!(outcomes[&d(DB)], Outcome::Failed(_)),
        "{outcomes:?}"
    );

    let lines = logs(&f, &deployment).await;
    let (status, error) = f.status(&deployment).await;
    assert_eq!(status, DeploymentStatus::Failed);
    let (db_error,): (Option<String>,) = sqlx::query_as(
        "SELECT error FROM deployment_resources WHERE deployment_id = ? AND descriptor = ?",
    )
    .bind(&deployment.id)
    .bind(DB)
    .fetch_one(f.store.pool())
    .await
    .unwrap();
    let db_error = db_error.unwrap();
    assert_eq!(db_error, "Error: authentication failed for [REDACTED]");

    // The secret, raw or escaped, is nowhere: not in the log, the resource's
    // error or the deployment's.
    let all = format!("{lines:?} {db_error} {error:?}");
    let escaped = serde_json::to_string(LEAKED).unwrap();
    for form in [
        LEAKED,
        &escaped[1..escaped.len() - 1],
        "pa\\\"ss",
        "ss\\\\word",
    ] {
        assert!(!all.contains(form), "{form} in {all}");
    }

    // What the executor logged is redacted, and the lines name the resource
    // and its operation.
    let db = fake.job(DB);
    let of_db: Vec<_> = lines
        .iter()
        .filter(|l| l.descriptor.as_deref() == Some(DB))
        .collect();
    let messages: Vec<_> = of_db.iter().map(|l| l.message.as_str()).collect();
    for expected in [
        "connecting with [REDACTED]",
        "{\"password\": \"[REDACTED]\"}",
        "Error: authentication failed for [REDACTED]",
    ] {
        assert!(
            messages.iter().any(|m| m.contains(expected)),
            "{expected} in {messages:?}"
        );
    }
    for line in &of_db {
        assert_eq!(
            line.guresid.as_deref(),
            Some(db.guresid.to_string().as_str())
        );
        assert!(line.operation_id.is_some(), "{line:?}");
    }
    assert!(
        messages.iter().any(|m| m.contains("(attempt 1)")),
        "{messages:?}"
    );

    // Lines about the deployment as a whole name no resource.
    let first = &lines[0];
    assert_eq!(first.descriptor, None);
    assert!(first.message.starts_with("deployment"), "{first:?}");
    let last = lines.last().unwrap();
    assert!(last.message.starts_with("deployment failed"), "{last:?}");
    assert!(lines.windows(2).all(|w| w[0].seq < w[1].seq));
}

#[tokio::test]
async fn the_log_stops_at_its_quota_but_keeps_the_outcome() {
    let f = Fixture::new().await;
    let fake = Fake::with(&[("dns", Behavior::Permanent)]);
    let deployment = f.accept().await;
    let limits = Limits {
        log_bytes: 120,
        ..fast()
    };
    planner(&f, &fake, limits).run(deployment.clone()).await;
    let lines = logs(&f, &deployment).await;
    let quota = lines
        .iter()
        .position(|l| l.message.contains("quota of 120 bytes"))
        .unwrap_or_else(|| panic!("{lines:?}"));
    let warning = &lines[quota];
    assert_eq!(warning.level, "warn");
    assert_eq!((&warning.descriptor, &warning.operation_id), (&None, &None));
    // Final status lines neither count against the quota nor are dropped.
    let status =
        |m: &str| m == "provisioned" || m.starts_with("failed, ") || m.starts_with("not attempted");
    let kept: usize = lines[..quota]
        .iter()
        .filter(|l| !status(&l.message))
        .map(|l| l.message.len())
        .sum();
    assert!(kept <= 120, "{lines:?}");
    let messages: Vec<_> = lines.iter().map(|l| l.message.as_str()).collect();
    assert!(
        messages.contains(
            &"failed, nothing changed; fix the cause and deploy again: 400: invalid zone"
        ),
        "{messages:?}"
    );
    assert!(
        messages.last().unwrap().starts_with("deployment failed"),
        "{messages:?}"
    );
    // Only those follow the warning.
    assert!(
        messages[quota + 1..]
            .iter()
            .all(|m| status(m) || m.starts_with("deployment failed")),
        "{messages:?}"
    );
}

#[tokio::test]
async fn nothing_is_logged_after_the_log_closes() {
    let f = Fixture::new().await;
    let deployment = f.accept().await;
    let log = DeploymentLog::start(f.store.clone(), &deployment, 1 << 20);
    let db = log.resource(&d(DB));
    db.info("before");
    log.close().await;
    db.info("after");
    log.deployment().status("info", "after");
    tokio::task::yield_now().await;
    let messages: Vec<_> = logs(&f, &deployment)
        .await
        .into_iter()
        .map(|l| l.message)
        .collect();
    assert_eq!(messages, ["before"]);
}

#[tokio::test]
async fn lost_log_lines_fail_the_deployment() {
    let f = Fixture::new().await;
    let fake = Arc::new(Fake::default());
    let deployment = f.accept().await;
    // Every line but the outcome fails to be stored.
    sqlx::query(
        "CREATE TRIGGER logs_full BEFORE INSERT ON logs \
         WHEN NEW.message NOT LIKE 'deployment failed%' BEGIN SELECT RAISE(ABORT, 'disk full'); END",
    )
    .execute(f.store.pool())
    .await
    .unwrap();
    let outcomes = planner(&f, &fake, fast()).run(deployment.clone()).await;
    assert!(outcomes.values().all(|o| *o == Outcome::Provisioned));
    let (status, error) = f.status(&deployment).await;
    assert_eq!(status, DeploymentStatus::Failed);
    let error = error.unwrap();
    assert!(error.ends_with("log lines could not be stored"), "{error}");
    let lines = logs(&f, &deployment).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].message, format!("deployment failed: {error}"));
}

mod coprovision;
mod pins;
mod recovery;
mod teardown;
mod workloads;
