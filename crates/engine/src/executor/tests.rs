//! The local executor behind the planner, with fake drivers, a fake STS and the
//! built-in secret store: secret-use checks before every read, secret outputs
//! as references, credentials and their target, permits for destructive or
//! unknown plans, cookies and redaction.

use std::sync::Mutex;

use openporch_core::catalog::{ClassRegistry, builtin_types};
use openporch_core::graph::{GraphInput, build_graph};
use openporch_core::ids::{Descriptor, Guresid};
use openporch_core::model::{CloudAccount, ResourceDefinition};
use openporch_core::placeholder::SharedValue;
use openporch_core::sets::DeploymentSet;
use openporch_core::workload::Workload;
use openporch_drivers::{
    Capabilities, DeleteOutcome, DriverFuture, DriverInfo, DriverOutputs, Observation, PutOutcome,
    Targets,
};
use openporch_store::secrets::{BuiltinSecretStore, MasterKey};
use openporch_store::{
    AuditIntent, DeploymentKind, DeploymentRequest, NewGrant, Ownership, SqliteStore, Store,
};
use serde_json::json;

use super::*;
use crate::cloud::{AssumeRole, AwsCredentials, CallerIdentity, Sts};
use crate::planner::{
    Deployment, JobInputs, JobLog, Limits, NoPermits, Observed, Outcome, Planner,
};
use openporch_store::secrets::BoxFuture;

const ORG: &str = "acme";
const DB: &str = "postgres.default#modules.web.externals.db";
const WORKLOAD: &str = "workload.default#modules.web";

/// Plaintexts: the definition's master password, the shared API token, the
/// cloud account's secret key and the password the driver generates.
const MASTER: &str = "master-plaintext-31";
const TOKEN: &str = "token-plaintext-59";
const ACCOUNT_KEY: &str = "account-key-plaintext-73";
const PASSWORD: &str = "password-plaintext-47";

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
      AUTH: "Bearer ${values.API_TOKEN}"
resources:
  db: {type: postgres}
"#;

/// A driver that records its calls and answers as told. It generates a
/// password, returns it as a secret output and counts its cookie up.
#[derive(Debug)]
struct FakeDriver {
    info: DriverInfo,
    plan: Mutex<Plan>,
    plan_delete: Mutex<Plan>,
    /// Fails `put` with this error, after logging the secrets it received.
    fail: Mutex<Option<String>>,
    puts: Mutex<Vec<DriverRequest>>,
    deletes: Mutex<Vec<DriverRequest>>,
    /// `get` cannot tell, and says so with the secrets it received.
    unsure: Mutex<bool>,
}

impl FakeDriver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            info: DriverInfo {
                id: "test/fake".into(),
                protocol_version: "builtin".into(),
                cookie_version: 1,
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                capabilities: Capabilities {
                    targets: Targets {
                        resources: true,
                        workloads: true,
                        triggers: false,
                    },
                    observe: true,
                    recover: true,
                    preview: true,
                    cancel: false,
                },
            },
            plan: Mutex::new(Plan::NoEffects),
            plan_delete: Mutex::new(Plan::NoEffects),
            fail: Mutex::new(None),
            puts: Mutex::new(Vec::new()),
            deletes: Mutex::new(Vec::new()),
            unsure: Mutex::new(false),
        })
    }

    fn puts(&self) -> Vec<DriverRequest> {
        self.puts.lock().unwrap().clone()
    }

    fn outputs(req: &DriverRequest) -> DriverOutputs {
        let mut values = Map::new();
        values.insert("host".into(), json!("db.internal"));
        let mut secrets = Map::new();
        secrets.insert("password".into(), json!(PASSWORD));
        DriverOutputs {
            values,
            secrets: SecretMap::new(secrets),
            manifests: Vec::new(),
            footprint: Box::new(openporch_drivers::Footprint {
                backend: Some(format!("fake://{}", req.guresid)),
                external_ids: vec![format!("arn:fake:{}", req.guresid)],
                // The version it ran: the one it was given, or its own.
                pin: Some(req.pin.clone().unwrap_or_else(|| json!({"commit": "tip"}))),
            }),
        }
    }
}

/// Every secret value a request carries.
fn secret_values(req: &DriverRequest) -> Vec<String> {
    let mut out = Vec::new();
    for map in [&req.secrets, &req.resource_secrets] {
        for v in map.expose().values() {
            out.push(v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        }
    }
    out
}

impl Driver for FakeDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn plan<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        let plan = self.plan.lock().unwrap().clone();
        Box::pin(async move { Ok(plan) })
    }

    fn plan_delete<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        let plan = self.plan_delete.lock().unwrap().clone();
        Box::pin(async move { Ok(plan) })
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        self.puts.lock().unwrap().push(req.clone());
        let fail = self.fail.lock().unwrap().clone();
        Box::pin(async move {
            let secrets = secret_values(req).join(", ");
            if let Some(fail) = fail {
                // As a careless driver does: the secrets in its log and error.
                req.log.line(&format!("connecting with {secrets}"));
                return Err(DriverError::Invalid {
                    code: Some("FAKE-1".into()),
                    message: format!("{fail}: {secrets}"),
                });
            }
            let n = req
                .cookie
                .as_ref()
                .and_then(|c| c.data.strip_prefix("n=")?.parse::<u32>().ok())
                .unwrap_or(0);
            Ok(PutOutcome::Ready {
                outputs: Self::outputs(req),
                cookie: Some(Cookie::new(1, format!("n={}", n + 1))?),
            })
        })
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        let unsure = *self.unsure.lock().unwrap();
        Box::pin(async move {
            if unsure {
                return Ok(Observation::Unknown {
                    reason: format!("cannot tell with {}", secret_values(req).join(", ")),
                });
            }
            Ok(Observation::Ready {
                outputs: Self::outputs(req),
                cookie: None,
            })
        })
    }

    fn delete<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        self.deletes.lock().unwrap().push(req.clone());
        Box::pin(async { Ok(DeleteOutcome::Deleted) })
    }
}

/// An STS whose caller identity reports `account`.
struct FakeSts {
    account: Mutex<String>,
}

impl Sts for FakeSts {
    fn assume_role(
        &self,
        _request: AssumeRole,
    ) -> BoxFuture<'_, Result<AwsCredentials, CloudError>> {
        Box::pin(async { Err(CloudError::Unavailable("no roles in tests".into())) })
    }

    fn caller_identity<'a>(
        &'a self,
        _credentials: &'a AwsCredentials,
    ) -> BoxFuture<'a, Result<CallerIdentity, CloudError>> {
        let account = self.account.lock().unwrap().clone();
        Box::pin(async move {
            Ok(CallerIdentity {
                arn: format!("arn:aws:iam::{account}:user/openporch"),
                account,
                user_id: "AIDATEST".into(),
            })
        })
    }
}

/// Records each permit check and answers it.
#[derive(Debug, Default)]
struct Permits {
    allow: bool,
    asked: Mutex<Vec<(DestructiveAction, Option<String>)>>,
}

impl PermitCheck for Permits {
    fn check<'a>(
        &'a self,
        req: PermitRequest<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        self.asked
            .lock()
            .unwrap()
            .push((req.action, req.plan.map(str::to_owned)));
        let allow = self.allow;
        Box::pin(async move {
            if allow {
                Ok(())
            } else {
                Err("no permit covers this action".into())
            }
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<SqliteStore>,
    keyring: Arc<Keyring>,
    secrets: Arc<BuiltinSecretStore>,
    driver: Arc<FakeDriver>,
    sts: Arc<FakeSts>,
    cloud: Arc<CredentialResolver>,
    drivers: Arc<DriverRegistry>,
    /// Where the shared value `API_TOKEN` points.
    token: Mutex<String>,
}

fn audit(method: &str, path: &str) -> AuditIntent {
    AuditIntent::accepted(ORG, "root", method, path, "test")
}

fn def(id: &str, ty: &str, driver: &str, inputs: Value) -> ResourceDefinition {
    serde_json::from_value(json!({
        "id": id, "type": ty, "driver_type": driver,
        "driver_inputs": inputs, "criteria": [{}],
    }))
    .unwrap()
}

impl Fixture {
    async fn new() -> Self {
        Self::with_drivers(DriverRegistry::with_builtins()).await
    }

    async fn with_drivers(mut drivers: DriverRegistry) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ownership = Ownership::acquire(dir.path()).unwrap();
        let store = SqliteStore::open(ownership, ORG).await.unwrap();
        for (id, role) in [("alice", "manager"), ("root", "administrator")] {
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
        sqlx::query(
            "INSERT INTO drivers (org_id, id, body, created_at) VALUES (?, 'test/fake', '{}', 0)",
        )
        .bind(ORG)
        .execute(store.pool())
        .await
        .unwrap();
        store
            .create_app(
                ORG,
                "shop",
                "Shop",
                "alice",
                audit("POST", "/orgs/acme/apps"),
            )
            .await
            .unwrap();
        let keyring = Arc::new(Keyring::new(MasterKey::from_bytes([7; 32]), []));
        let secrets = Arc::new(BuiltinSecretStore::new(&store, keyring.clone(), ORG));
        for (name, value) in [
            ("defs/pg/master", MASTER),
            ("apps/shop/values/API_TOKEN", TOKEN),
            ("accounts/aws-main/secret_access_key", ACCOUNT_KEY),
            ("shared/pg-master", MASTER),
        ] {
            secrets.write(name, &SecretValue::new(value)).await.unwrap();
        }
        let driver = FakeDriver::new();
        drivers.register(driver.clone()).unwrap();
        let sts = Arc::new(FakeSts {
            account: Mutex::new("111122223333".into()),
        });
        let cloud = Arc::new(CredentialResolver::new(sts.clone()));
        Self {
            _dir: dir,
            store: Arc::new(store),
            keyring,
            secrets,
            driver,
            sts,
            cloud,
            drivers: Arc::new(drivers),
            token: Mutex::new("apps/shop/values/API_TOKEN".into()),
        }
    }

    /// The definitions: the database on the fake driver with `pg_inputs`, the
    /// workload on `workload_driver`.
    async fn define(
        &self,
        pg: ResourceDefinition,
        workload_driver: &str,
    ) -> Vec<ResourceDefinition> {
        let defs = vec![
            def("workloads", "workload", workload_driver, json!({})),
            def("base-env", "base-env", "openporch/echo", json!({})),
            pg,
        ];
        for d in &defs {
            self.store
                .create_definition(ORG, d, audit("POST", "/orgs/acme/resources/defs"))
                .await
                .unwrap();
        }
        defs
    }

    fn executor(&self, checks: Arc<dyn PermitCheck>) -> LocalExecutor<SqliteStore> {
        LocalExecutor::new(
            self.store.clone(),
            self.drivers.clone(),
            self.secrets.clone(),
            self.keyring.clone(),
            self.cloud.clone(),
            checks,
        )
    }

    /// Accepts and runs a deployment of `web` with `defs`, checking permits
    /// with `checks`. Returns each resource's outcome.
    async fn deploy(
        &self,
        defs: &[ResourceDefinition],
        checks: Arc<dyn PermitCheck>,
    ) -> (Deployment, BTreeMap<Descriptor, Outcome>) {
        let mut set = DeploymentSet::default();
        let c = Workload::parse(WEB, None)
            .unwrap()
            .to_module(&builtin_types())
            .unwrap();
        set.modules.insert(c.id, c.module);
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
            definitions: defs,
            types: &builtin_types(),
            classes: &ClassRegistry::new([]),
        })
        .unwrap();
        let snapshot = json!({
            "version": 2,
            "mode": "full",
            "permits": ["permit-1"],
            "graph": graph,
            "definitions": defs
                .iter()
                .map(|d| (d.id.to_string(), json!({"version": 1, "definition": d})))
                .collect::<Map<_, _>>(),
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
                permits: Default::default(),
                org: ORG.into(),
                app: "shop".into(),
                env: "development".into(),
                actor: "root".into(),
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
            .await
            .unwrap();
        let mut deployment = Deployment::from_snapshot(
            org,
            app,
            env,
            env_type,
            accepted.deployment().id.clone(),
            set_id,
            &snapshot,
        )
        .unwrap();
        deployment.values.insert(
            "API_TOKEN".into(),
            SharedValue::Secret(SecretRef {
                store: "openporch".into(),
                reference: self.token.lock().unwrap().clone(),
                version: None,
            }),
        );
        let limits = Limits {
            attempts: 1,
            ..Limits::default()
        };
        let planner = Planner::new(
            self.store.clone(),
            Arc::new(self.executor(checks.clone())),
            limits,
        )
        .with_permit_check(checks);
        let outcomes = planner.run(deployment.clone()).await;
        (deployment, outcomes)
    }

    async fn active(&self, descriptor: &str) -> openporch_store::ActiveResource {
        self.store
            .active_resource(ORG, "shop", "development", descriptor)
            .await
            .unwrap()
            .unwrap()
    }

    /// The deployment's log and every recorded error, as one text.
    async fn recorded(&self, deployment: &Deployment) -> String {
        let lines = self
            .store
            .deployment_logs(ORG, "shop", "development", &deployment.id, None, 1000)
            .await
            .unwrap();
        let mut text: String = lines.iter().map(|l| format!("{}\n", l.message)).collect();
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT COALESCE(outputs, '') || COALESCE(error, '') FROM deployment_resources",
        )
        .fetch_all(self.store.pool())
        .await
        .unwrap();
        text.extend(rows.into_iter().map(|(r,)| r));
        text
    }
}

fn plain_pg() -> ResourceDefinition {
    def("pg", "postgres", "test/fake", json!({}))
}

fn allow() -> Arc<Permits> {
    Arc::new(Permits {
        allow: true,
        ..Permits::default()
    })
}

fn failed(outcomes: &BTreeMap<Descriptor, Outcome>, descriptor: &str) -> String {
    match &outcomes[&Descriptor::parse(descriptor).unwrap()] {
        Outcome::Failed(e) => e.clone(),
        other => panic!("{descriptor} did not fail: {other:?}"),
    }
}

fn all_provisioned(outcomes: &BTreeMap<Descriptor, Outcome>) {
    assert!(
        outcomes.values().all(|o| *o == Outcome::Provisioned),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn a_secret_output_is_stored_and_returned_as_a_reference() {
    let f = Fixture::new().await;
    let defs = f.define(plain_pg(), "openporch/none").await;
    let (deployment, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);

    // The driver's password is in the primary store; the resource records a
    // pinned reference under its own GUResID, and only non-secret outputs.
    let db = f.active(DB).await;
    let password = &db.secret_outputs["password"];
    assert_eq!(password.store, "openporch");
    assert_eq!(
        password.reference,
        format!("resources/{}/outputs/password", db.guresid)
    );
    assert!(password.version.is_some());
    assert_eq!(f.secrets.read(password).await.unwrap().expose(), PASSWORD);
    assert_eq!(db.outputs, json!({"host": "db.internal"}));
    let (backend,): (Option<String>,) =
        sqlx::query_as("SELECT backend FROM active_resources WHERE guresid = ?")
            .bind(&db.guresid)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(backend, Some(format!("fake://{}", db.guresid)));

    // The workload used it as the database's output: `openporch/none`
    // received the rendered spec, and its secret outputs became references
    // under the workload's own GUResID.
    let workload = f.active(WORKLOAD).await;
    let spec = &workload.secret_outputs["spec"];
    assert_eq!(
        spec.reference,
        format!("resources/{}/outputs/spec", workload.guresid)
    );
    let rendered: Value =
        serde_json::from_str(f.secrets.read(spec).await.unwrap().expose()).unwrap();
    let variables = &rendered["containers"]["web"]["variables"];
    assert_eq!(variables["DB_PASSWORD"], PASSWORD);
    assert_eq!(variables["DB_HOST"], "db.internal");

    let recorded = f.recorded(&deployment).await;
    for plaintext in [PASSWORD, TOKEN] {
        assert!(!recorded.contains(plaintext), "{plaintext}: {recorded}");
    }
}

#[tokio::test]
async fn a_secret_in_a_composite_string_is_rendered_only_for_the_driver() {
    let f = Fixture::new().await;
    let defs = f.define(plain_pg(), "test/fake").await;
    let (deployment, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);

    // The planner kept `Bearer ${values.API_TOKEN}` as an expression; the
    // workload's driver got the complete value, among its secret inputs only.
    let puts = f.driver.puts();
    let workload = puts.iter().find(|r| r.resource_type == "workload").unwrap();
    let spec = &workload.resource_secrets.expose()["spec"];
    assert_eq!(
        spec["containers"]["web"]["variables"]["AUTH"],
        format!("Bearer {TOKEN}")
    );
    assert!(!workload.resource.contains_key("spec"));
    assert_eq!(workload.resource["id"], "web");
    let recorded = f.recorded(&deployment).await;
    assert!(!recorded.contains(TOKEN), "{recorded}");
}

#[tokio::test]
async fn a_reference_whose_grant_was_removed_fails_before_any_driver_call() {
    let f = Fixture::new().await;
    let grant = f
        .store
        .create_grant(
            ORG,
            &NewGrant {
                store: "openporch".into(),
                reference: "shared/pg-master".into(),
                app_id: Some("shop".into()),
                env_id: None,
                org_wide: false,
            },
            audit("POST", "/orgs/acme/secretstores/openporch/grants"),
        )
        .await
        .unwrap();
    let pg = def(
        "pg",
        "postgres",
        "test/fake",
        json!({"secret_refs": {"master": {"store": "openporch", "ref": "shared/pg-master"}}}),
    );
    let defs = f.define(pg, "openporch/none").await;
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);
    assert_eq!(f.driver.puts()[0].secrets.expose()["master"], MASTER);

    f.store
        .delete_grant(
            ORG,
            "openporch",
            &grant.id,
            audit("DELETE", "/orgs/acme/secretstores/openporch/grants"),
        )
        .await
        .unwrap();
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    let error = failed(&outcomes, DB);
    assert!(error.contains("no grant"), "{error}");
    assert_eq!(f.driver.puts().len(), 1, "the driver was called again");
}

#[tokio::test]
async fn a_definition_that_no_longer_matches_does_not_get_its_secrets() {
    let f = Fixture::new().await;
    let pg = def(
        "pg",
        "postgres",
        "test/fake",
        json!({"secret_refs": {"master": {"store": "openporch", "ref": "defs/pg/master"}}}),
    );
    let defs = f.define(pg, "openporch/none").await;
    // The deployment's snapshot still selects `pg`, but its criteria now
    // name another application.
    sqlx::query("UPDATE criteria SET app_id = 'billing' WHERE definition_id = 'pg'")
        .execute(f.store.pool())
        .await
        .unwrap();
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    let error = failed(&outcomes, DB);
    assert!(error.contains("no longer matches"), "{error}");
    assert!(f.driver.puts().is_empty());
}

impl Fixture {
    /// Creates the `aws-main` account with static keys.
    async fn account(&self) {
        let account: CloudAccount = serde_json::from_value(json!({
            "id": "aws-main", "type": "aws",
            "config": {"access_key_id": "AKIATEST", "region": "eu-north-1"},
            "secret_refs": {"secret_access_key": {"store": "openporch", "ref": "accounts/aws-main/secret_access_key"}},
        }))
        .unwrap();
        self.store
            .create_account(ORG, &account, audit("POST", "/orgs/acme/accounts"))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_definition_that_no_longer_matches_does_not_get_its_cloud_account() {
    let f = Fixture::new().await;
    f.account().await;
    let mut pg = plain_pg();
    pg.driver_account = Some("aws-main".into());
    let defs = f.define(pg, "openporch/none").await;
    sqlx::query("UPDATE criteria SET app_id = 'billing' WHERE definition_id = 'pg'")
        .execute(f.store.pool())
        .await
        .unwrap();
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    let error = failed(&outcomes, DB);
    assert!(error.contains("no longer matches"), "{error}");
    assert!(f.driver.puts().is_empty());
}

#[tokio::test]
async fn a_shared_value_without_a_grant_is_not_read() {
    let f = Fixture::new().await;
    // `API_TOKEN` points at a secret written for no value of `shop`.
    *f.token.lock().unwrap() = "shared/pg-master".into();
    let defs = f.define(plain_pg(), "test/fake").await;
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    let error = failed(&outcomes, WORKLOAD);
    assert!(error.contains("no grant"), "{error}");
    assert!(
        f.driver
            .puts()
            .iter()
            .all(|r| r.resource_type != "workload"),
        "the workload's driver was called"
    );
}

#[tokio::test]
async fn a_resource_is_not_provisioned_in_another_aws_account() {
    let f = Fixture::new().await;
    f.account().await;
    let mut pg = plain_pg();
    pg.driver_account = Some("aws-main".into());
    let defs = f.define(pg, "openporch/none").await;
    let (deployment, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);

    // The driver got fresh credentials, and the resource records its target.
    let credentials = f.driver.puts()[0].credentials.clone().unwrap();
    assert_eq!(credentials.expose()["AccessKeyId"], "AKIATEST");
    assert_eq!(credentials.expose()["SecretAccessKey"], ACCOUNT_KEY);
    assert_eq!(credentials.expose()["Region"], "eu-north-1");
    assert_eq!(f.active(DB).await.target.as_deref(), Some("111122223333"));
    assert!(!f.recorded(&deployment).await.contains(ACCOUNT_KEY));

    // The same account now resolves elsewhere: refused before the driver runs.
    *f.sts.account.lock().unwrap() = "444455556666".into();
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    let error = failed(&outcomes, DB);
    assert!(error.contains("explicit migration"), "{error}");
    assert!(error.contains("444455556666"), "{error}");
    assert_eq!(f.driver.puts().len(), 1);
}

#[tokio::test]
async fn a_replacement_needs_a_permit_for_its_plan() {
    let f = Fixture::new().await;
    let defs = f.define(plain_pg(), "openporch/none").await;
    *f.driver.plan.lock().unwrap() = Plan::Actions {
        actions: vec![(Action::Replace, "db".into())],
        fingerprint: "plan-1".into(),
    };

    // Without a permit, nothing is applied.
    let refuse = Arc::new(Permits::default());
    let (_, outcomes) = f.deploy(&defs, refuse.clone()).await;
    let error = failed(&outcomes, DB);
    assert!(error.starts_with("approval_required"), "{error}");
    assert!(f.driver.puts().is_empty());
    assert_eq!(
        refuse.asked.lock().unwrap()[..],
        [(DestructiveAction::Replace, Some("plan-1".into()))]
    );

    // A permit for that plan reaches the driver with it.
    let (_, outcomes) = f.deploy(&defs, allow()).await;
    all_provisioned(&outcomes);
    assert_eq!(f.driver.puts()[0].permit.as_deref(), Some("plan-1"));
}

#[tokio::test]
async fn an_unknown_change_to_an_existing_resource_needs_a_permit() {
    let f = Fixture::new().await;
    let defs = f.define(plain_pg(), "openporch/none").await;
    *f.driver.plan.lock().unwrap() = Plan::Unknown;

    // A first creation destroys nothing.
    let refuse = Arc::new(Permits::default());
    let (_, outcomes) = f.deploy(&defs, refuse.clone()).await;
    all_provisioned(&outcomes);
    assert!(refuse.asked.lock().unwrap().is_empty());

    // Changing it with unknown impact needs a permit.
    let (_, outcomes) = f.deploy(&defs, refuse.clone()).await;
    let error = failed(&outcomes, DB);
    assert!(error.starts_with("approval_required"), "{error}");
    assert_eq!(
        refuse.asked.lock().unwrap()[..],
        [(DestructiveAction::Unknown, None)]
    );
    assert_eq!(f.driver.puts().len(), 1);
}

#[tokio::test]
async fn the_driver_cookie_survives_between_deployments_sealed() {
    let f = Fixture::new().await;
    let defs = f.define(plain_pg(), "openporch/none").await;
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);
    assert_eq!(f.driver.puts()[0].cookie, None);
    let sealed = f.active(DB).await.cookie.unwrap();
    assert!(
        !String::from_utf8_lossy(&sealed.ciphertext).contains("n=1"),
        "the cookie is stored in the clear"
    );

    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);
    assert_eq!(
        f.driver.puts()[1].cookie,
        Some(Cookie::new(1, "n=1").unwrap())
    );

    // A cookie sealed for another resource does not open for this one.
    let db = f.active(DB).await;
    let other = Guresid::new("0123456789abcdef0123456789abcdef").unwrap();
    let mut job = provision_job(&db, other);
    job.cookie = db.cookie.clone();
    let failure = f
        .executor(Arc::new(NoPermits))
        .provision(job)
        .await
        .unwrap_err();
    assert!(
        matches!(&failure, ExecFailure::Permanent(e) if e.contains("could not be decrypted")),
        "{failure:?}"
    );
}

#[tokio::test]
async fn a_driver_failure_is_redacted() {
    let f = Fixture::new().await;
    let pg = def(
        "pg",
        "postgres",
        "test/fake",
        json!({"secret_refs": {"master": {"store": "openporch", "ref": "defs/pg/master"}}}),
    );
    let defs = f.define(pg, "openporch/none").await;
    *f.driver.fail.lock().unwrap() = Some("authentication failed".into());
    let (deployment, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    let error = failed(&outcomes, DB);
    assert!(error.contains("FAKE-1: authentication failed"), "{error}");
    assert!(error.contains("[REDACTED]"), "{error}");

    let recorded = f.recorded(&deployment).await;
    assert!(
        recorded.contains("connecting with [REDACTED]"),
        "{recorded}"
    );
    assert!(!recorded.contains(MASTER), "{recorded}");
}

#[tokio::test]
async fn an_observation_is_redacted_and_keeps_nothing_for_an_existing_resource() {
    let f = Fixture::new().await;
    let pg = def(
        "pg",
        "postgres",
        "test/fake",
        json!({"secret_refs": {"master": {"store": "openporch", "ref": "defs/pg/master"}}}),
    );
    let defs = f.define(pg, "openporch/none").await;
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);
    let db = f.active(DB).await;
    let job = provision_job(&db, db.guresid.parse().unwrap());
    let executor = f.executor(allow());

    // The resource existed before, so its state proves nothing about an
    // update and nothing is stored for it.
    match executor.observe(job.clone(), false).await.unwrap() {
        Observed::Ready { result, repeatable } => {
            assert!(repeatable);
            assert!(result.secret_outputs.is_empty(), "{result:?}");
            assert!(result.outputs.is_empty(), "{result:?}");
        }
        other => panic!("{other:?}"),
    }
    // What a driver says while observing is redacted like any failure.
    *f.driver.unsure.lock().unwrap() = true;
    match executor.observe(job, false).await.unwrap() {
        Observed::Unknown(reason) => {
            assert!(reason.contains("[REDACTED]"), "{reason}");
            assert!(!reason.contains(MASTER), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

/// A job for `db`'s inputs under another identity, outside a deployment.
fn provision_job(db: &openporch_store::ActiveResource, guresid: Guresid) -> ProvisionJob {
    ProvisionJob {
        operation_id: "op-test".into(),
        attempt: 1,
        descriptor: Descriptor::parse(DB).unwrap(),
        guresid,
        context: crate::planner::JobContext {
            org: ORG.parse().unwrap(),
            app: "shop".parse().unwrap(),
            env: "development".parse().unwrap(),
            env_type: "development".parse().unwrap(),
            deployment_id: "d-test".into(),
            permits: Vec::new(),
        },
        definition_id: "pg".parse().unwrap(),
        definition_version: 1,
        driver_type: "test/fake".into(),
        driver_account: None,
        inputs: serde_json::from_value(db.inputs.clone()).unwrap(),
        input_fingerprint: "f".into(),
        cookie: None,
        target: None,
        pin: None,
        existing: true,
        deadline: Instant::now() + Duration::from_secs(60),
        log: JobLog::discard(),
    }
}

#[tokio::test]
async fn a_deletion_runs_with_the_permit_for_its_destroy_plan() {
    let f = Fixture::new().await;
    let defs = f.define(plain_pg(), "openporch/none").await;
    let (_, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);
    let db = f.active(DB).await;
    *f.driver.plan_delete.lock().unwrap() = Plan::Actions {
        actions: vec![(Action::Delete, "db".into())],
        fingerprint: "destroy-1".into(),
    };
    let mut job = provision_job(&db, db.guresid.parse().unwrap());
    job.cookie = db.cookie.clone();
    let deprovision = DeprovisionJob {
        job,
        snapshot: db.snapshot.clone(),
        recovery: db.recovery.clone(),
    };

    let refuse = f.executor(Arc::new(Permits::default()));
    let failure = refuse.deprovision(deprovision.clone()).await.unwrap_err();
    assert!(
        matches!(&failure, ExecFailure::Permanent(e) if e.starts_with("approval_required")),
        "{failure:?}"
    );
    assert!(f.driver.deletes.lock().unwrap().is_empty());

    let permits = allow();
    f.executor(permits.clone())
        .deprovision(deprovision)
        .await
        .unwrap();
    let deletes = f.driver.deletes.lock().unwrap().clone();
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0].permit.as_deref(), Some("destroy-1"));
    assert_eq!(deletes[0].cookie, Some(Cookie::new(1, "n=1").unwrap()));
    assert_eq!(
        permits.asked.lock().unwrap()[..],
        [(DestructiveAction::Delete, Some("destroy-1".into()))]
    );
}

#[test]
fn inputs_recorded_without_sources_get_them_from_their_references() {
    let output = SecretRef {
        store: "openporch".into(),
        reference: "resources/0123456789abcdef0123456789abcdef/outputs/password".into(),
        version: Some("1".into()),
    };
    let value = SecretRef {
        store: "openporch".into(),
        reference: "apps/shop/values/API_TOKEN".into(),
        version: None,
    };
    let inputs: JobInputs = serde_json::from_value(json!({
        "values": {},
        "secrets": {"object": [["auth", {"secret": {"parts": [{"secret": value}]}}]]},
        "secret_refs": {"value": {}},
        "resource": {"object": [["password", {"secret": {"parts": [{"secret": output}]}}]]},
    }))
    .unwrap();
    assert!(inputs.sources.is_empty());
    let job = ProvisionJob {
        inputs,
        ..provision_job_empty()
    };
    let sources = inferred_sources(&job);
    assert_eq!(
        sources,
        [
            (value, SecretSource::Values),
            (
                output,
                SecretSource::Output {
                    guresid: "0123456789abcdef0123456789abcdef".parse().unwrap()
                }
            ),
        ]
    );
}

fn provision_job_empty() -> ProvisionJob {
    ProvisionJob {
        operation_id: String::new(),
        attempt: 1,
        descriptor: Descriptor::parse(DB).unwrap(),
        guresid: Guresid::new("0123456789abcdef0123456789abcdef").unwrap(),
        context: crate::planner::JobContext {
            org: ORG.parse().unwrap(),
            app: "shop".parse().unwrap(),
            env: "development".parse().unwrap(),
            env_type: "development".parse().unwrap(),
            deployment_id: String::new(),
            permits: Vec::new(),
        },
        definition_id: "pg".parse().unwrap(),
        definition_version: 1,
        driver_type: "test/fake".into(),
        driver_account: None,
        inputs: JobInputs {
            values: json!({}),
            secrets: Resolved::Value(json!({})),
            secret_refs: Resolved::Value(json!({})),
            resource: Resolved::Value(json!({})),
            sources: Vec::new(),
        },
        input_fingerprint: String::new(),
        cookie: None,
        target: None,
        pin: None,
        existing: false,
        deadline: Instant::now(),
        log: JobLog::discard(),
    }
}

/// The `Redacted failure` scenario with the real OpenTofu: a failing plan
/// prints a secret variable, which shows as `[REDACTED]` in the deployment's
/// log and the resource's error.
#[tokio::test]
#[ignore = "needs OpenTofu"]
async fn opentofu_failure_is_redacted_in_the_deployment_log() {
    let root = tempfile::tempdir().unwrap();
    let mut config = openporch_drivers::OpenTofuConfig::new(root.path());
    if let Ok(tofu) = std::env::var("OPENPORCH_TEST_TOFU") {
        config.tofu = tofu.into();
    }
    let f = Fixture::with_drivers(DriverRegistry::with_builtins().with_opentofu(config)).await;
    let script = r#"
variable "token" {
  type      = string
  sensitive = true
}
output "n" { value = tonumber(nonsensitive(var.token)) }
"#;
    f.secrets
        .write("defs/pg/token", &SecretValue::new("hunter2-not-a-number"))
        .await
        .unwrap();
    let pg = def(
        "pg",
        "postgres",
        "openporch/opentofu",
        json!({
            "values": {"script": script, "append_logs_to_error": true},
            "secret_refs": {"variables": {"token": {"store": "openporch", "ref": "defs/pg/token"}}},
        }),
    );
    let defs = f.define(pg, "openporch/none").await;
    let (deployment, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    let error = failed(&outcomes, DB);
    assert!(error.contains("TOFU-001"), "{error}");
    let recorded = f.recorded(&deployment).await;
    assert!(recorded.contains("$ tofu plan"), "{recorded}");
    assert!(recorded.contains("[REDACTED]"), "{recorded}");
    assert!(!recorded.contains("hunter2-not-a-number"), "{recorded}");
}

#[test]
fn a_failure_after_the_driver_accepted_a_change_is_uncertain() {
    let transient = || DriverError::Transient {
        message: "503 while polling".into(),
        retry_after: None,
    };
    assert!(matches!(
        mutation_failure(transient(), false),
        ExecFailure::Retryable { .. }
    ));
    assert!(matches!(
        mutation_failure(transient(), true),
        ExecFailure::Uncertain(e) if e.contains("503 while polling")
    ));
    assert!(matches!(
        mutation_failure(DriverError::Conflict("state is locked".into()), false),
        ExecFailure::Permanent(_)
    ));
}

#[tokio::test]
async fn a_pin_reaches_the_driver_and_the_version_it_ran_comes_back() {
    let f = Fixture::new().await;
    let defs = f.define(plain_pg(), "openporch/none").await;
    let (deployment, outcomes) = f.deploy(&defs, Arc::new(NoPermits)).await;
    all_provisioned(&outcomes);
    // Unpinned, the driver ran its own version, which the deployment records.
    assert_eq!(f.driver.puts()[0].pin, None);
    let snapshot = f
        .store
        .deployment_snapshot(ORG, "shop", "development", &deployment.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot["pins"][DB], json!({"commit": "tip"}));

    let db = f.active(DB).await;
    let other = Guresid::new("0123456789abcdef0123456789abcdef").unwrap();
    let mut job = provision_job(&db, other);
    job.pin = Some(json!({"commit": "recorded"}));
    let result = f
        .executor(Arc::new(NoPermits))
        .provision(job)
        .await
        .unwrap();
    assert_eq!(
        f.driver.puts().last().unwrap().pin,
        Some(json!({"commit": "recorded"}))
    );
    assert_eq!(result.pin, Some(json!({"commit": "recorded"})));
}
