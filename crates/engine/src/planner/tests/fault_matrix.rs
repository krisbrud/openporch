//! The fault-injection recovery suite: every durable step of a deployment,
//! an update, an environment deletion and a recovery is made to fail, or the
//! server to crash just before or just after it, one case at a time; so is
//! every driver call, by crashing, hanging, panicking or failing. After each
//! case the server restarts, the resources whose outcome is unknown are
//! recovered, and the work is done again. Every case must end the same way:
//!
//! - each resource keeps one GUResID and has one cloud object;
//! - nothing is deleted twice, or while something that uses it still exists;
//! - a resource with an unknown outcome blocks new deployments until it is
//!   recovered, and recovery never needs an administrator for a driver that
//!   can observe;
//! - no deployment reports success it did not reach, or stays running after a
//!   restart;
//! - the repeated deployment or deletion converges.
//!
//! The fault points are not listed by hand: a clean run records the store
//! calls it makes, and each becomes a case, so a new durable step is covered
//! when it is added.
//!
//! `faults.rs` crashes a run before each of its commits. This matrix also
//! fails calls the server survives, crashes after a write lands, faults reads,
//! recovery itself and the drivers.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Mutex;

use openporch_store::secrets::Sealed;
use openporch_store::{
    Acceptance, ActiveResource, AppDeletion, AppDeletionRequest, AuditEntry, DeltaRecord,
    DeploymentRecord, DeploymentRequest, DeploymentResource, EnvRecord, LogLine, NewEnv,
    OperationStarted, RecoveryReport, Resolution, ResourceRunStatus, Tombstone, UnknownOperation,
};
use tokio::task::JoinSet;

use super::*;

const ENV: &str = "pr-9";
const RECOVER: &str = "/orgs/acme/apps/shop/envs/pr-9/resources/recover";
const DESCRIPTORS: [&str; 5] = [BASE, DB, DNS, ROUTE, WORKLOAD];

/// A store call the planner makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Call {
    BeginOperation,
    RecordInputs,
    Checkpoint,
    KeepCookie,
    Resolve,
    ResourceStatus,
    Finish,
    AppendLogs,
    ActiveResource,
    ActiveResources,
    UnknownOperation,
    Snapshot,
    Env,
    Tombstones,
}

impl Call {
    fn writes(self) -> bool {
        matches!(
            self,
            Self::BeginOperation
                | Self::RecordInputs
                | Self::Checkpoint
                | Self::KeepCookie
                | Self::Resolve
                | Self::ResourceStatus
                | Self::Finish
                | Self::AppendLogs
        )
    }

    /// The faults injected at this call: a read cannot crash after it
    /// committed anything.
    fn faults(self) -> &'static [Fault] {
        if self.writes() {
            &[Fault::Fail, Fault::CrashBefore, Fault::CrashAfter]
        } else {
            &[Fault::Fail, Fault::CrashBefore]
        }
    }
}

/// The `nth` call of a kind about a resource, or about no resource.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    call: Call,
    descriptor: Option<String>,
    nth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The call returns an error; the server keeps running.
    Fail,
    /// The server stops before the call reaches the database.
    CrashBefore,
    /// The call commits, then the server stops before it learns that.
    CrashAfter,
}

#[derive(Default)]
struct FaultState {
    counts: BTreeMap<(Call, Option<String>), usize>,
    trace: Vec<Point>,
    /// The resource each operation this store started is about.
    ops: BTreeMap<String, String>,
    hit: bool,
    /// Once the server has crashed, no call it makes returns.
    dead: bool,
}

enum Action {
    Pass,
    Fail,
    Crash { commit: bool },
    Dead,
}

/// A store that injects one fault at one point, and records the points a run
/// passes.
struct Faulty {
    inner: Arc<SqliteStore>,
    target: Option<(Point, Fault)>,
    state: Mutex<FaultState>,
}

fn injected() -> StoreError {
    StoreError::Database(sqlx::Error::Io(std::io::Error::other("injected fault")))
}

impl Faulty {
    fn new(inner: Arc<SqliteStore>, target: Option<(Point, Fault)>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            target,
            state: Mutex::default(),
        })
    }

    fn hit(&self) -> bool {
        self.state.lock().unwrap().hit
    }

    fn dead(&self) -> bool {
        self.state.lock().unwrap().dead
    }

    /// Stops the server, as a crash while a driver runs does.
    fn kill(&self) {
        self.state.lock().unwrap().dead = true;
    }

    fn trace(&self) -> Vec<Point> {
        self.state.lock().unwrap().trace.clone()
    }

    async fn op_descriptor(&self, operation_id: &str) -> Option<String> {
        if let Some(d) = self.state.lock().unwrap().ops.get(operation_id) {
            return Some(d.clone());
        }
        sqlx::query_scalar(
            "SELECT r.descriptor FROM operations o \
             JOIN active_resources r ON r.guresid = o.guresid WHERE o.id = ?",
        )
        .bind(operation_id)
        .fetch_optional(self.inner.pool())
        .await
        .ok()
        .flatten()
    }

    async fn gate<T>(
        &self,
        call: Call,
        descriptor: Option<String>,
        run: impl Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        let action = {
            let mut s = self.state.lock().unwrap();
            if s.dead {
                Action::Dead
            } else {
                let n = s.counts.entry((call, descriptor.clone())).or_default();
                *n += 1;
                let point = Point {
                    call,
                    descriptor,
                    nth: *n,
                };
                s.trace.push(point.clone());
                match &self.target {
                    Some((target, fault)) if *target == point => {
                        s.hit = true;
                        match fault {
                            Fault::Fail => Action::Fail,
                            Fault::CrashBefore => {
                                s.dead = true;
                                Action::Crash { commit: false }
                            }
                            Fault::CrashAfter => {
                                s.dead = true;
                                Action::Crash { commit: true }
                            }
                        }
                    }
                    _ => Action::Pass,
                }
            }
        };
        match action {
            Action::Pass => run.await,
            Action::Fail => Err(injected()),
            Action::Crash { commit } => {
                if commit {
                    let _ = run.await;
                }
                std::future::pending().await
            }
            Action::Dead => std::future::pending().await,
        }
    }
}

impl Store for Faulty {
    async fn record_audit(&self, intent: AuditIntent) -> Result<String, StoreError> {
        self.inner.record_audit(intent).await
    }

    async fn audit_entries(&self, org: &str, limit: u32) -> Result<Vec<AuditEntry>, StoreError> {
        self.inner.audit_entries(org, limit).await
    }

    async fn create_env_type(
        &self,
        org: &str,
        id: &str,
        description: &str,
        protected: bool,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner
            .create_env_type(org, id, description, protected, audit)
            .await
    }

    async fn create_app(
        &self,
        org: &str,
        id: &str,
        name: &str,
        owner: &str,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner.create_app(org, id, name, owner, audit).await
    }

    async fn create_env(
        &self,
        org: &str,
        app: &str,
        env: NewEnv,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner.create_env(org, app, env, audit).await
    }

    async fn envs(&self, org: &str, app: &str) -> Result<Vec<EnvRecord>, StoreError> {
        self.inner.envs(org, app).await
    }

    async fn update_env_type(
        &self,
        org: &str,
        id: &str,
        description: Option<&str>,
        protected: Option<bool>,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner
            .update_env_type(org, id, description, protected, audit)
            .await
    }

    async fn update_app(
        &self,
        org: &str,
        id: &str,
        name: &str,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner.update_app(org, id, name, audit).await
    }

    async fn update_env(
        &self,
        org: &str,
        app: &str,
        env: &str,
        name: &str,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner.update_env(org, app, env, name, audit).await
    }

    async fn delete_env_type(
        &self,
        org: &str,
        id: &str,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner.delete_env_type(org, id, audit).await
    }

    async fn latest_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> Result<Option<DeploymentRecord>, StoreError> {
        self.inner.latest_deployment(org, app, env).await
    }

    async fn deployments(
        &self,
        org: &str,
        app: &str,
        env: &str,
        limit: i64,
        before_seq: Option<i64>,
    ) -> Result<Vec<DeploymentRecord>, StoreError> {
        self.inner
            .deployments(org, app, env, limit, before_seq)
            .await
    }

    async fn idempotent_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        actor: &str,
        key: &str,
    ) -> Result<Option<(DeploymentRecord, String)>, StoreError> {
        self.inner
            .idempotent_deployment(org, app, env, actor, key)
            .await
    }

    async fn deployment_snapshot(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> Result<Option<Value>, StoreError> {
        self.gate(
            Call::Snapshot,
            None,
            self.inner.deployment_snapshot(org, app, env, id),
        )
        .await
    }

    async fn app_deployment(
        &self,
        org: &str,
        app: &str,
        id: &str,
    ) -> Result<Option<DeploymentRecord>, StoreError> {
        self.inner.app_deployment(org, app, id).await
    }

    async fn delta(
        &self,
        org: &str,
        app: &str,
        id: &str,
    ) -> Result<Option<DeltaRecord>, StoreError> {
        self.inner.delta(org, app, id).await
    }

    async fn deltas(&self, org: &str, app: &str) -> Result<Vec<DeltaRecord>, StoreError> {
        self.inner.deltas(org, app).await
    }

    async fn set(
        &self,
        org: &str,
        app: &str,
        id: &str,
    ) -> Result<Option<DeploymentSet>, StoreError> {
        self.inner.set(org, app, id).await
    }

    async fn env(&self, org: &str, app: &str, env: &str) -> Result<Option<EnvRecord>, StoreError> {
        self.gate(Call::Env, None, self.inner.env(org, app, env))
            .await
    }

    async fn put_set(
        &self,
        org: &str,
        app: &str,
        set: &DeploymentSet,
    ) -> Result<String, StoreError> {
        self.inner.put_set(org, app, set).await
    }

    async fn put_set_audited(
        &self,
        org: &str,
        app: &str,
        set: &DeploymentSet,
        audit: AuditIntent,
    ) -> Result<String, StoreError> {
        self.inner.put_set_audited(org, app, set, audit).await
    }

    async fn put_delta(
        &self,
        org: &str,
        app: &str,
        id: &str,
        content: &Value,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner.put_delta(org, app, id, content, audit).await
    }

    async fn put_env_delta(
        &self,
        org: &str,
        app: &str,
        id: &str,
        draft: (&str, i64),
        content: &Value,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner
            .put_env_delta(org, app, id, draft, content, audit)
            .await
    }

    async fn accept_deployment(&self, req: DeploymentRequest) -> Result<Acceptance, StoreError> {
        self.inner.accept_deployment(req).await
    }

    async fn accept_app_deletion(
        &self,
        req: AppDeletionRequest,
    ) -> Result<AppDeletion, StoreError> {
        self.inner.accept_app_deletion(req).await
    }

    async fn deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> Result<Option<DeploymentRecord>, StoreError> {
        self.inner.deployment(org, app, env, id).await
    }

    async fn deployment_resources(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
    ) -> Result<Vec<DeploymentResource>, StoreError> {
        self.inner.deployment_resources(org, app, env, id).await
    }

    async fn append_logs(
        &self,
        org: &str,
        app: &str,
        env: &str,
        deployment_id: &str,
        lines: Vec<LogLine>,
    ) -> Result<(), StoreError> {
        self.gate(
            Call::AppendLogs,
            None,
            self.inner.append_logs(org, app, env, deployment_id, lines),
        )
        .await
    }

    async fn deployment_logs(
        &self,
        org: &str,
        app: &str,
        env: &str,
        deployment_id: &str,
        after: Option<i64>,
        limit: i64,
    ) -> Result<Vec<LogLine>, StoreError> {
        self.inner
            .deployment_logs(org, app, env, deployment_id, after, limit)
            .await
    }

    async fn begin_operation(&self, start: OperationStart) -> Result<OperationStarted, StoreError> {
        let descriptor = start.descriptor.to_string();
        let started = self
            .gate(
                Call::BeginOperation,
                Some(descriptor.clone()),
                self.inner.begin_operation(start),
            )
            .await?;
        self.state
            .lock()
            .unwrap()
            .ops
            .insert(started.operation_id.clone(), descriptor);
        Ok(started)
    }

    async fn checkpoint_operation(
        &self,
        operation_id: &str,
        result: CheckpointResult,
    ) -> Result<(), StoreError> {
        let descriptor = self.op_descriptor(operation_id).await;
        self.gate(
            Call::Checkpoint,
            descriptor,
            self.inner.checkpoint_operation(operation_id, result),
        )
        .await
    }

    async fn record_operation_inputs(
        &self,
        operation_id: &str,
        recovery: Value,
    ) -> Result<(), StoreError> {
        let descriptor = self.op_descriptor(operation_id).await;
        self.gate(
            Call::RecordInputs,
            descriptor,
            self.inner.record_operation_inputs(operation_id, recovery),
        )
        .await
    }

    async fn unknown_operation(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
    ) -> Result<Option<UnknownOperation>, StoreError> {
        self.gate(
            Call::UnknownOperation,
            Some(descriptor.to_owned()),
            self.inner.unknown_operation(org, app, env, descriptor),
        )
        .await
    }

    async fn resolve_operation(
        &self,
        operation_id: &str,
        resolution: Resolution,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        let descriptor = self.op_descriptor(operation_id).await;
        self.gate(
            Call::Resolve,
            descriptor,
            self.inner
                .resolve_operation(operation_id, resolution, audit),
        )
        .await
    }

    async fn keep_recovery_cookie(
        &self,
        operation_id: &str,
        observed_with: Option<&Sealed>,
        cookie: &Sealed,
    ) -> Result<(), StoreError> {
        let descriptor = self.op_descriptor(operation_id).await;
        self.gate(
            Call::KeepCookie,
            descriptor,
            self.inner
                .keep_recovery_cookie(operation_id, observed_with, cookie),
        )
        .await
    }

    async fn record_resource_status(
        &self,
        org: &str,
        app: &str,
        env: &str,
        deployment_id: &str,
        descriptor: &str,
        status: ResourceRunStatus,
        error: Option<String>,
    ) -> Result<(), StoreError> {
        self.gate(
            Call::ResourceStatus,
            Some(descriptor.to_owned()),
            self.inner.record_resource_status(
                org,
                app,
                env,
                deployment_id,
                descriptor,
                status,
                error,
            ),
        )
        .await
    }

    async fn active_resource(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
    ) -> Result<Option<ActiveResource>, StoreError> {
        self.gate(
            Call::ActiveResource,
            Some(descriptor.to_owned()),
            self.inner.active_resource(org, app, env, descriptor),
        )
        .await
    }

    async fn active_resources(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> Result<Vec<ActiveResource>, StoreError> {
        self.gate(
            Call::ActiveResources,
            None,
            self.inner.active_resources(org, app, env),
        )
        .await
    }

    async fn definition_resources(
        &self,
        org: &str,
        definition: &str,
    ) -> Result<Vec<ActiveResource>, StoreError> {
        self.inner.definition_resources(org, definition).await
    }

    async fn schedule_deletion(
        &self,
        org: &str,
        app: &str,
        env: &str,
        descriptor: &str,
        audit: AuditIntent,
    ) -> Result<(), StoreError> {
        self.inner
            .schedule_deletion(org, app, env, descriptor, audit)
            .await
    }

    async fn detach_resource(
        &self,
        org: &str,
        app: &str,
        env: &str,
        detach: Detach<'_>,
        audit: AuditIntent,
    ) -> Result<Tombstone, StoreError> {
        self.inner
            .detach_resource(org, app, env, detach, audit)
            .await
    }

    async fn tombstones(
        &self,
        org: &str,
        app: &str,
        env: &str,
    ) -> Result<Vec<Tombstone>, StoreError> {
        self.gate(Call::Tombstones, None, self.inner.tombstones(org, app, env))
            .await
    }

    async fn finish_deployment(
        &self,
        org: &str,
        app: &str,
        env: &str,
        id: &str,
        error: Option<String>,
    ) -> Result<(), StoreError> {
        self.gate(
            Call::Finish,
            None,
            self.inner.finish_deployment(org, app, env, id, error),
        )
        .await
    }

    async fn recover_interrupted(&self) -> Result<RecoveryReport, StoreError> {
        self.inner.recover_interrupted().await
    }

    async fn required_key_ids(&self) -> Result<Vec<String>, StoreError> {
        self.inner.required_key_ids().await
    }
}

/// What the faulted step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// The first deployment of the environment.
    Create,
    /// A deployment of the same set over provisioned resources.
    Update,
    /// The environment's deletion.
    Teardown,
    /// Recovering a database whose creation crashed.
    Recover,
}

/// A driver fault: how the driver of resources of type `ty` behaves.
#[derive(Clone)]
struct DriverFault {
    ty: &'static str,
    behavior: Behavior,
    label: &'static str,
}

#[derive(Clone)]
struct Case {
    scenario: Scenario,
    store: Option<(Point, Fault)>,
    driver: Option<DriverFault>,
}

impl Case {
    fn label(&self) -> String {
        let mut label = format!("{:?}", self.scenario);
        if let Some((p, fault)) = &self.store {
            label.push_str(&format!(
                ": {fault:?} at {:?} #{} of {}",
                p.call,
                p.nth,
                p.descriptor.as_deref().unwrap_or("the deployment")
            ));
        }
        if let Some(driver) = &self.driver {
            label.push_str(&format!(": {} driver {}", driver.ty, driver.label));
        }
        label
    }
}

fn ty_of(descriptor: &str) -> String {
    d(descriptor).ty.to_string()
}

fn limits(case: &Case) -> Limits {
    let hangs = matches!(
        case.driver,
        Some(DriverFault {
            behavior: Behavior::Hang,
            ..
        })
    );
    Limits {
        deadline: Duration::from_millis(if hangs { 500 } else { 30_000 }),
        ..fast()
    }
}

/// Every resource's status and GUResID in the environment.
async fn resources(f: &Fixture) -> BTreeMap<String, ActiveResource> {
    f.store
        .active_resources(ORG, "shop", ENV)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.descriptor.clone(), r))
        .collect()
}

/// Whether the driver fault has reached the point where a crash leaves its
/// effect behind: the object exists, or is gone for a deletion.
fn driver_crashed(fake: &Fake, driver: &DriverFault, scenario: Scenario, before: usize) -> bool {
    if scenario == Scenario::Teardown {
        let deleted = fake.deleted.lock().unwrap();
        let ours: Vec<_> = deleted
            .iter()
            .filter(|j| j.job.descriptor.ty.as_str() == driver.ty)
            .collect();
        return ours.len() > before
            && ours.iter().all(|j| {
                !fake
                    .objects
                    .lock()
                    .unwrap()
                    .contains_key(j.job.guresid.as_str())
            });
    }
    let jobs = fake.jobs.lock().unwrap();
    let ours: Vec<_> = jobs
        .iter()
        .filter(|j| j.descriptor.ty.as_str() == driver.ty)
        .collect();
    ours.len() > before
        && ours.iter().all(|j| {
            fake.objects
                .lock()
                .unwrap()
                .contains_key(j.guresid.as_str())
        })
}

fn driver_calls(fake: &Fake, driver: &DriverFault, scenario: Scenario) -> usize {
    if scenario == Scenario::Teardown {
        let deleted = fake.deleted.lock().unwrap();
        deleted
            .iter()
            .filter(|j| j.job.descriptor.ty.as_str() == driver.ty)
            .count()
    } else {
        let jobs = fake.jobs.lock().unwrap();
        jobs.iter()
            .filter(|j| j.descriptor.ty.as_str() == driver.ty)
            .count()
    }
}

/// Runs `step` until it finishes or the server crashes, then stops it the way
/// a crash does.
async fn until_crash<T: Send + 'static>(
    faulty: &Faulty,
    step: impl Future<Output = T> + Send + 'static,
    crashed: impl Fn() -> bool,
) {
    let run = tokio::spawn(step);
    let waited = tokio::time::timeout(Duration::from_secs(30), async {
        while !run.is_finished() && !faulty.dead() {
            if crashed() {
                faulty.kill();
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "the step neither finished nor crashed");
    if run.is_finished() {
        if let Err(e) = run.await {
            std::panic::resume_unwind(e.into_panic());
        }
    } else {
        run.abort();
        let _ = run.await;
    }
}

/// Runs one case and checks how it ended; returns the store calls the faulted
/// step made.
async fn run(case: Case) -> Vec<Point> {
    let f = Fixture::in_env(ENV).await;
    let fake = Fake::with(&[]);
    let checks = Arc::new(AllowAll::default());
    let clean = Arc::new(planner(&f, &fake, fast()).with_permit_check(checks.clone()));
    let recover = |planner: Arc<Planner<SqliteStore, Fake>>, descriptor: String| async move {
        planner
            .recover(ORG, "shop", ENV, &descriptor, audit("POST", RECOVER))
            .await
    };

    // What exists before the faulted step.
    let mut faulted: Option<Deployment> = None;
    if case.scenario != Scenario::Create {
        if case.scenario == Scenario::Recover {
            // The database's creation takes effect, then the server stops.
            fake.behavior
                .lock()
                .unwrap()
                .insert("postgres".into(), Behavior::Crash);
            let crashed = Faulty::new(f.store.clone(), None);
            let planner = Arc::new(
                Planner::new(crashed.clone(), fake.clone(), fast())
                    .with_permit_check(checks.clone()),
            );
            let deployment = f.accept().await;
            let fake2 = fake.clone();
            until_crash(
                &crashed,
                async move { planner.run(deployment).await },
                || fake2.objects.lock().unwrap().values().any(|o| o == DB),
            )
            .await;
            fake.behavior.lock().unwrap().clear();
            f.store.recover_interrupted().await.unwrap();
            assert_eq!(resources(&f).await[DB].status, "unknown");
        } else {
            let deployment = f.accept().await;
            let outcomes = clean.run(deployment.clone()).await;
            assert!(
                outcomes.values().all(|o| *o == Outcome::Provisioned),
                "{outcomes:?}"
            );
        }
    }
    let before = resources(&f).await;
    let dependents: BTreeMap<String, Vec<String>> = before
        .values()
        .map(|r| (r.descriptor.clone(), r.dependencies.clone()))
        .collect();

    // The faulted step.
    if let Some(driver) = &case.driver {
        let mut behaviors = if case.scenario == Scenario::Teardown {
            fake.deletions.lock().unwrap()
        } else {
            fake.behavior.lock().unwrap()
        };
        behaviors.insert(driver.ty.into(), driver.behavior.clone());
    }
    let faulty = Faulty::new(f.store.clone(), case.store.clone());
    let planner = Arc::new(
        Planner::new(faulty.clone(), fake.clone(), limits(&case)).with_permit_check(checks.clone()),
    );
    let calls_before = case
        .driver
        .as_ref()
        .map_or(0, |dr| driver_calls(&fake, dr, case.scenario));
    let crashed = {
        let fake = fake.clone();
        let driver = case.driver.clone();
        let scenario = case.scenario;
        move || match &driver {
            Some(dr) if matches!(dr.behavior, Behavior::Crash) => {
                driver_crashed(&fake, dr, scenario, calls_before)
            }
            _ => false,
        }
    };
    match case.scenario {
        Scenario::Create | Scenario::Update | Scenario::Teardown => {
            let deployment = if case.scenario == Scenario::Teardown {
                f.accept_teardown().await.unwrap()
            } else {
                f.accept().await
            };
            faulted = Some(deployment.clone());
            let planner = planner.clone();
            until_crash(
                &faulty,
                async move { planner.run(deployment).await },
                crashed,
            )
            .await;
        }
        Scenario::Recover => {
            let planner = planner.clone();
            until_crash(
                &faulty,
                async move {
                    planner
                        .recover(ORG, "shop", ENV, DB, audit("POST", RECOVER))
                        .await
                },
                crashed,
            )
            .await;
        }
    }
    if case.store.is_some() {
        assert!(faulty.hit(), "the fault point was never reached");
    }

    // A deployment that says it succeeded converged.
    let record = |id: String| {
        let store = f.store.clone();
        async move { store.deployment(ORG, "shop", ENV, &id).await.unwrap() }
    };
    if let Some(deployment) = &faulted {
        match record(deployment.id.clone()).await {
            Some(r) if r.status == DeploymentStatus::Succeeded => {
                assert_ne!(case.scenario, Scenario::Teardown);
                for (descriptor, r) in resources(&f).await {
                    assert_eq!(r.status, "provisioned", "{descriptor}");
                    assert_eq!(r.last_deploy_id.as_ref(), Some(&deployment.id));
                }
            }
            Some(_) => {}
            // A finished deletion takes its deployments with it.
            None => {
                assert_eq!(case.scenario, Scenario::Teardown);
                assert_eq!(f.env_state().await, None);
                assert!(fake.objects.lock().unwrap().is_empty());
            }
        }
    }

    // The server restarts with a healthy driver.
    let trace = faulty.trace();
    fake.behavior.lock().unwrap().clear();
    fake.deletions.lock().unwrap().clear();
    f.store.recover_interrupted().await.unwrap();
    if let Some(deployment) = &faulted
        && let Some(r) = record(deployment.id.clone()).await
    {
        assert_ne!(r.status, DeploymentStatus::InProgress);
    }

    // A resource whose outcome is unknown blocks every deployment, until it
    // is recovered without an administrator.
    let uncertain: Vec<String> = resources(&f)
        .await
        .into_values()
        .filter(|r| r.status == "unknown" || r.status == "recovery_required")
        .map(|r| r.descriptor)
        .collect();
    if !uncertain.is_empty() {
        // An environment whose deletion failed stays deletion-failed, and
        // refuses deployments for that reason first.
        let accepted = f.try_accept().await;
        assert!(
            matches!(accepted, Err(StoreError::RecoveryRequired(_)))
                || (case.scenario == Scenario::Teardown
                    && matches!(accepted, Err(StoreError::Conflict(_)))),
            "a deployment was accepted over {uncertain:?}: {accepted:?}"
        );
        let deletion = f.accept_teardown().await;
        assert!(
            matches!(deletion, Err(StoreError::RecoveryRequired(_))),
            "a deletion was accepted over {uncertain:?}: {}",
            deletion.is_ok()
        );
    }
    for descriptor in &uncertain {
        let recovered = recover(clean.clone(), descriptor.clone()).await;
        assert!(
            matches!(
                recovered,
                Ok(Recovered::Provisioned | Recovered::Deleted | Recovered::Repeatable(_))
            ),
            "{descriptor}: {recovered:?}"
        );
    }
    for r in resources(&f).await.into_values() {
        assert!(
            r.status != "unknown" && r.status != "recovery_required",
            "{} is still {}",
            r.descriptor,
            r.status
        );
    }

    // The work is done again, and converges.
    if case.scenario == Scenario::Teardown {
        for _ in 0..2 {
            if f.env_state().await.is_none() {
                break;
            }
            let teardown = f.accept_teardown().await.unwrap();
            clean.run(teardown).await;
        }
        assert_eq!(f.env_state().await, None, "the environment was not deleted");
        assert!(
            fake.objects.lock().unwrap().is_empty(),
            "{:?}",
            fake.objects.lock().unwrap()
        );
        let tombstones = f.tombstones().await;
        for descriptor in DESCRIPTORS {
            let guresid = &before[descriptor].guresid;
            assert!(
                tombstones.contains(&(descriptor.to_owned(), guresid.clone(), "deleted".into())),
                "{descriptor}: {tombstones:?}"
            );
        }
    } else {
        let deployment = f.accept().await;
        let outcomes = clean.run(deployment.clone()).await;
        assert!(
            outcomes.values().all(|o| *o == Outcome::Provisioned),
            "{outcomes:?}"
        );
        assert_eq!(f.status(&deployment).await.0, DeploymentStatus::Succeeded);
        let after = resources(&f).await;
        assert_eq!(
            after.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(DESCRIPTORS)
        );
        no_duplicates(&f, &fake).await;
    }

    // One identity and one object per resource, however often it was tried.
    for descriptor in DESCRIPTORS {
        let mut guresids: BTreeSet<String> = fake
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.descriptor.to_string() == descriptor)
            .map(|j| j.guresid.to_string())
            .collect();
        guresids.extend(
            fake.deleted
                .lock()
                .unwrap()
                .iter()
                .filter(|j| j.job.descriptor.to_string() == descriptor)
                .map(|j| j.job.guresid.to_string()),
        );
        assert_eq!(guresids.len(), 1, "{descriptor}: {guresids:?}");
        if let Some(r) = before.get(descriptor) {
            assert!(
                guresids.contains(&r.guresid),
                "{descriptor} changed identity"
            );
        }
        assert_eq!(fake.creations(descriptor), 1, "{descriptor}");
    }

    // Nothing was deleted twice, or while something using it still existed.
    let deleted = fake.deleted.lock().unwrap().clone();
    let existing = fake.deleted_while.lock().unwrap().clone();
    for (job, objects) in deleted.iter().zip(&existing) {
        let descriptor = job.job.descriptor.to_string();
        assert!(
            objects.contains_key(job.job.guresid.as_str()),
            "{descriptor} was deleted again after it was gone"
        );
        for user in objects.values() {
            assert!(
                !dependents
                    .get(user)
                    .is_some_and(|deps| deps.contains(&descriptor)),
                "{descriptor} was deleted while {user} still used it"
            );
        }
    }
    trace
}

async fn no_duplicates(f: &Fixture, fake: &Fake) {
    for r in f.store.active_resources(ORG, "shop", ENV).await.unwrap() {
        let objects = fake.objects.lock().unwrap();
        assert_eq!(
            objects.values().filter(|d| **d == r.descriptor).count(),
            1,
            "{}",
            r.descriptor
        );
        assert_eq!(objects.get(&r.guresid), Some(&r.descriptor));
    }
}

/// Runs every case at once, and reports each that failed by its label.
async fn run_all(cases: Vec<Case>) {
    let total = cases.len();
    let mut set = JoinSet::new();
    let mut failed = Vec::new();
    let mut pending = cases.into_iter();
    let mut start = |set: &mut JoinSet<_>| {
        if let Some(case) = pending.next() {
            let label = case.label();
            set.spawn(async move { (label, tokio::spawn(run(case)).await) });
        }
    };
    for _ in 0..8 {
        start(&mut set);
    }
    while let Some(done) = set.join_next().await {
        let (label, result) = done.unwrap();
        if let Err(e) = result {
            let panic = e.into_panic();
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            failed.push(format!("{label}: {message}"));
        }
        start(&mut set);
    }
    assert!(
        failed.is_empty(),
        "{} of {total} cases failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// A case for every store call a clean run of `scenario` makes and every
/// fault that call can meet. Log writes are batched in the background, so
/// their order is not repeatable; they are covered by failing every one.
async fn store_cases(scenario: Scenario) -> Vec<Case> {
    let trace = run(Case {
        scenario,
        store: None,
        driver: None,
    })
    .await;
    let points: BTreeSet<Point> = trace
        .into_iter()
        .filter(|p| p.call != Call::AppendLogs)
        .collect();
    assert!(!points.is_empty());
    points
        .into_iter()
        .flat_map(|p| {
            p.call.faults().iter().map(move |fault| Case {
                scenario,
                store: Some((p.clone(), *fault)),
                driver: None,
            })
        })
        .collect()
}

fn driver_cases(scenario: Scenario) -> Vec<Case> {
    let faults = [
        (Behavior::Crash, "crashes after taking effect"),
        (Behavior::Hang, "hangs past the deadline"),
        (Behavior::Panic, "panics"),
        (Behavior::Retryable(None), "keeps failing retryably"),
        (Behavior::Permanent, "fails permanently"),
    ];
    DESCRIPTORS
        .iter()
        .flat_map(|descriptor| {
            faults.iter().map(move |(behavior, label)| Case {
                scenario,
                store: None,
                driver: Some(DriverFault {
                    ty: Box::leak(ty_of(descriptor).into_boxed_str()),
                    behavior: behavior.clone(),
                    label,
                }),
            })
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_store_fault_while_creating_recovers() {
    run_all(store_cases(Scenario::Create).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_store_fault_while_updating_recovers() {
    run_all(store_cases(Scenario::Update).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_store_fault_while_deleting_recovers() {
    run_all(store_cases(Scenario::Teardown).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_store_fault_while_recovering_recovers() {
    run_all(store_cases(Scenario::Recover).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_driver_fault_recovers() {
    let cases = [Scenario::Create, Scenario::Update, Scenario::Teardown]
        .into_iter()
        .flat_map(driver_cases)
        .collect();
    run_all(cases).await;
}
