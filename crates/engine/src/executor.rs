//! The in-process executor: authorizes and reads each secret a job needs,
//! obtains fresh cloud credentials, checks the driver's plan against the
//! deployment's permits, calls the driver and writes its secret outputs to the
//! primary store, returning only references.
//!
//! Secret values live only here and in the driver call. Each one is registered
//! with the job's [`RedactingLog`] as soon as it is read, so neither the log nor a
//! failure handed back to the planner carries it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use openporch_core::criteria::definition_weight;
use openporch_core::model::{ResourceContext, SecretRef};
use openporch_core::placeholder::Resolved;
use openporch_drivers::{
    Action, Cookie, Driver, DriverContext, DriverError, DriverRegistry, DriverRequest, Operation,
    Plan, SecretMap, call_timeout, converge, remove, with_timeout,
};
use openporch_store::secrets::{Keyring, Sealed, SecretError, SecretStore, SecretValue};
use openporch_store::{AccountStore, CatalogStore, SecretOwner, SecretUseStore, StoreError};
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::cloud::{CloudError, CredentialResolver};
use crate::planner::{
    DeprovisionJob, DestructiveAction, ExecFailure, Executor, PermitCheck, PermitRequest,
    ProvisionJob, ProvisionResult, RedactingLog, SecretSource,
};

/// The version of the sealed cookie format, bound into its associated data.
const COOKIE_AAD: &str = "openporch-cookie:v1";

/// A driver call ready to run.
struct Call {
    driver: Arc<dyn Driver>,
    req: DriverRequest,
    /// The call's timeout, within the deployment's deadline.
    timeout: Duration,
    /// What the job's cloud account resolved to, if it has one.
    target: Option<String>,
}

/// Runs driver calls in the server process.
pub struct LocalExecutor<S> {
    store: Arc<S>,
    drivers: Arc<DriverRegistry>,
    secrets: Arc<dyn SecretStore>,
    keyring: Arc<Keyring>,
    cloud: Arc<CredentialResolver>,
    checks: Arc<dyn PermitCheck>,
}

impl<S> std::fmt::Debug for LocalExecutor<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalExecutor")
            .field("drivers", &self.drivers.ids().collect::<Vec<_>>())
            .field("secrets", &self.secrets.id())
            .finish_non_exhaustive()
    }
}

impl<S> LocalExecutor<S>
where
    S: SecretUseStore + AccountStore + CatalogStore + 'static,
{
    /// An executor calling `drivers`, reading and writing secrets in `secrets`
    /// (the organization's primary store), sealing cookies with `keyring`,
    /// resolving cloud accounts with `cloud` and checking destructive plans with
    /// `checks`.
    pub fn new(
        store: Arc<S>,
        drivers: Arc<DriverRegistry>,
        secrets: Arc<dyn SecretStore>,
        keyring: Arc<Keyring>,
        cloud: Arc<CredentialResolver>,
        checks: Arc<dyn PermitCheck>,
    ) -> Self {
        Self {
            store,
            drivers,
            secrets,
            keyring,
            cloud,
            checks,
        }
    }

    /// Everything a driver call needs, with the secrets the job uses read and
    /// registered with `log`, and the call's timeout.
    async fn request(
        &self,
        job: &ProvisionJob,
        sources: &[(SecretRef, SecretSource)],
        check_criteria: bool,
        log: &mut RedactingLog,
    ) -> Result<Call, ExecFailure> {
        let driver = self.drivers.get(&job.driver_type).ok_or_else(|| {
            ExecFailure::Permanent(format!(
                "driver `{}` is not registered on this server",
                job.driver_type
            ))
        })?;
        let values = match &job.inputs.values {
            Value::Object(values) => values.clone(),
            Value::Null => Map::new(),
            other => {
                return Err(ExecFailure::Permanent(format!(
                    "the driver inputs are not an object: {other}"
                )));
            }
        };
        let remaining = job.deadline.saturating_duration_since(Instant::now());
        let timeout = call_timeout(&values, Some(remaining)).map_err(driver_failure)?;

        let read = self.read_secrets(job, sources, check_criteria, log).await?;
        let (resource, resource_secrets) = split(&job.inputs.resource, &read)?;
        let mut secrets = object(&job.inputs.secrets, &read, "`driver_inputs.secrets`")?;
        secrets.extend(object(
            &job.inputs.secret_refs,
            &read,
            "`driver_inputs.secret_refs`",
        )?);
        let (credentials, target) = match self.credentials(job, timeout, log).await? {
            Some((credentials, target)) => (Some(credentials), Some(target)),
            None => (None, None),
        };
        let cookie = self.open_cookie(job)?;
        let c = &job.context;
        let req = DriverRequest {
            guresid: job.guresid.clone(),
            operation: Operation {
                id: job.operation_id.clone(),
                fingerprint: job.input_fingerprint.clone(),
            },
            resource_type: job.descriptor.ty.to_string(),
            resource,
            resource_secrets: SecretMap::new(resource_secrets),
            values,
            secrets: SecretMap::new(secrets),
            credentials,
            context: DriverContext {
                org: c.org.to_string(),
                app: c.app.to_string(),
                env: c.env.to_string(),
                env_type: c.env_type.to_string(),
                res_id: job.descriptor.id.to_string(),
                class: job.descriptor.class.to_string(),
            },
            cookie,
            permit: None,
            log: log.sink(),
        };
        Ok(Call {
            driver,
            req,
            timeout,
            target,
        })
    }

    /// Authorizes and reads every secret reference in the job's inputs. Each
    /// reference passes [`SecretUseStore::authorize_secret_use`] for the
    /// deploying application and environment immediately before it is read.
    async fn read_secrets(
        &self,
        job: &ProvisionJob,
        sources: &[(SecretRef, SecretSource)],
        check_criteria: bool,
        log: &mut RedactingLog,
    ) -> Result<BTreeMap<SecretRef, SecretValue>, ExecFailure> {
        let mut definition_refs = BTreeSet::new();
        references(&job.inputs.secret_refs, &mut definition_refs);
        let mut input_refs = BTreeSet::new();
        references(&job.inputs.secrets, &mut input_refs);
        references(&job.inputs.resource, &mut input_refs);

        if check_criteria && !definition_refs.is_empty() {
            self.check_criteria(job).await?;
        }
        let definition = job.definition_id.as_str();
        let mut read = BTreeMap::new();
        for r in &definition_refs {
            self.authorize(job, &[SecretOwner::Definition(definition)], r)
                .await?;
            let value = self.read(r).await?;
            log.secret([value.expose()]);
            read.insert(r.clone(), value);
        }
        for r in &input_refs {
            if read.contains_key(r) {
                continue;
            }
            let owners: Vec<SecretOwner<'_>> = sources
                .iter()
                .filter(|(s, _)| s == r)
                .map(|(_, source)| match source {
                    SecretSource::Values => SecretOwner::Values,
                    SecretSource::Output { guresid } => SecretOwner::Resource(guresid.as_str()),
                })
                .collect();
            if owners.is_empty() {
                return Err(ExecFailure::Permanent(format!(
                    "secret `{}` in store `{}` has no recorded source, so its use cannot be \
                     authorized",
                    r.reference, r.store
                )));
            }
            self.authorize(job, &owners, r).await?;
            let value = self.read(r).await?;
            log.secret([value.expose()]);
            read.insert(r.clone(), value);
        }
        Ok(read)
    }

    /// Refuses a job whose definition no longer matches its resource: a
    /// definition's own secrets are read only for deployments its criteria
    /// match.
    async fn check_criteria(&self, job: &ProvisionJob) -> Result<(), ExecFailure> {
        let c = &job.context;
        let record = self
            .store
            .definition(c.org.as_str(), job.definition_id.as_str())
            .await
            .map_err(|e| store_failure(e, "the resource definition could not be read"))?
            .ok_or_else(|| {
                ExecFailure::Permanent(format!(
                    "definition `{}` no longer exists, so its secrets are not read",
                    job.definition_id
                ))
            })?;
        let ctx = ResourceContext {
            org: c.org.clone(),
            app: c.app.clone(),
            env: c.env.clone(),
            env_type: c.env_type.clone(),
            res: job.descriptor.clone(),
        };
        if definition_weight(&record.definition, &ctx).is_none() {
            return Err(ExecFailure::Permanent(format!(
                "definition `{}` no longer matches `{}` in `{}/{}`, so its secrets are not read",
                job.definition_id, job.descriptor, c.app, c.env
            )));
        }
        Ok(())
    }

    /// Succeeds when any of `owners` may use `r` in the job's application and
    /// environment.
    async fn authorize(
        &self,
        job: &ProvisionJob,
        owners: &[SecretOwner<'_>],
        r: &SecretRef,
    ) -> Result<(), ExecFailure> {
        let c = &job.context;
        let mut refused = None;
        for owner in owners {
            match self
                .store
                .authorize_secret_use(c.org.as_str(), c.app.as_str(), c.env.as_str(), *owner, r)
                .await
            {
                Ok(()) => return Ok(()),
                Err(StoreError::Forbidden(e)) => refused = Some(e),
                Err(e) => return Err(store_failure(e, "secret use could not be authorized")),
            }
        }
        Err(ExecFailure::Permanent(refused.unwrap_or_else(|| {
            format!("secret `{}` may not be used here", r.reference)
        })))
    }

    async fn read(&self, r: &SecretRef) -> Result<SecretValue, ExecFailure> {
        if r.store != self.secrets.id() {
            return Err(ExecFailure::Permanent(format!(
                "secret store `{}` cannot be read yet; only `{}` can",
                r.store,
                self.secrets.id()
            )));
        }
        self.secrets.read(r).await.map_err(|e| match e {
            // The store could not be reached: name what was being read.
            SecretError::Store(e) => ExecFailure::Retryable {
                error: format!(
                    "secret `{}` in store `{}` could not be read: {e}",
                    r.reference, r.store
                ),
                retry_after: None,
            },
            other => secret_failure(other),
        })
    }

    /// Fresh credentials for the job's cloud account and what they resolve to,
    /// refusing an account that now resolves to another target than the one
    /// the resource was provisioned in.
    async fn credentials(
        &self,
        job: &ProvisionJob,
        window: Duration,
        log: &mut RedactingLog,
    ) -> Result<Option<(SecretMap, String)>, ExecFailure> {
        let Some(id) = &job.driver_account else {
            return Ok(None);
        };
        let c = &job.context;
        let read_at = self.cloud.generation();
        let account = self
            .store
            .account(c.org.as_str(), id.as_str())
            .await
            .map_err(|e| store_failure(e, "the cloud account could not be read"))?
            .ok_or_else(|| ExecFailure::Permanent(format!("cloud account `{id}` does not exist")))?
            .account;
        for r in account.secret_refs.values() {
            self.authorize(job, &[SecretOwner::Account(id.as_str())], r)
                .await?;
        }
        let credentials = self
            .cloud
            .resolve(&account, read_at, self.secrets.as_ref(), window)
            .await
            .map_err(cloud_failure)?;
        log.secret(
            [
                Some(&credentials.secret_access_key),
                credentials.session_token.as_ref(),
            ]
            .into_iter()
            .flatten()
            .map(SecretValue::expose)
            .filter(|s| !s.is_empty()),
        );
        let identity = self
            .cloud
            .identity(&credentials)
            .await
            .map_err(cloud_failure)?;
        if let Some(previous) = &job.target
            && *previous != identity.account
        {
            return Err(ExecFailure::Permanent(format!(
                "cloud account `{id}` now resolves to AWS account {}, but `{}` was provisioned \
                 in {previous}; moving a resource is an explicit migration",
                identity.account, job.descriptor
            )));
        }
        let map = credentials
            .driver_inputs()
            .into_iter()
            .map(|(k, v)| (k.to_owned(), Value::String(v.expose().to_owned())))
            .collect();
        Ok(Some((SecretMap::new(map), identity.account)))
    }

    /// The cookie the job carries, decrypted.
    fn open_cookie(&self, job: &ProvisionJob) -> Result<Option<Cookie>, ExecFailure> {
        let Some(sealed) = &job.cookie else {
            return Ok(None);
        };
        let opened = self
            .keyring
            .open(cookie_aad(job).as_bytes(), sealed)
            .map_err(secret_failure)?
            .ok_or_else(|| {
                ExecFailure::Permanent(format!(
                    "the driver state of `{}` could not be decrypted",
                    job.descriptor
                ))
            })?;
        serde_json::from_slice(&opened).map(Some).map_err(|_| {
            ExecFailure::Permanent(format!(
                "the driver state of `{}` is unreadable",
                job.descriptor
            ))
        })
    }

    fn seal_cookie(&self, job: &ProvisionJob, cookie: &Cookie) -> Result<Sealed, ExecFailure> {
        let plain = serde_json::to_vec(cookie).map_err(|e| {
            ExecFailure::Uncertain(format!("the driver state could not be encoded: {e}"))
        })?;
        Ok(self.keyring.seal(cookie_aad(job).as_bytes(), &plain))
    }

    /// Checks the permits for a destructive or unknown-impact action.
    async fn permit(
        &self,
        job: &ProvisionJob,
        action: DestructiveAction,
        plan: Option<&str>,
    ) -> Result<(), ExecFailure> {
        let c = &job.context;
        let descriptor = job.descriptor.to_string();
        let check = self.checks.check(PermitRequest {
            org: c.org.as_str(),
            app: c.app.as_str(),
            env: c.env.as_str(),
            descriptor: &descriptor,
            action,
            permits: &c.permits,
            plan,
        });
        match tokio::time::timeout_at(job.deadline, check).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(ExecFailure::Permanent(format!("approval_required: {e}"))),
            Err(_) => Err(ExecFailure::Permanent(
                "the deployment ran out of time before its permits were checked".into(),
            )),
        }
    }

    /// The driver's preview of the call, or [`Plan::Unknown`] when it cannot
    /// preview.
    async fn plan(
        &self,
        driver: &dyn Driver,
        req: &DriverRequest,
        timeout: Duration,
        delete: bool,
    ) -> Result<Plan, ExecFailure> {
        if !driver.info().capabilities.preview {
            return Ok(Plan::Unknown);
        }
        let call = if delete {
            driver.plan_delete(req)
        } else {
            driver.plan(req)
        };
        // Planning changes nothing, so any failure can be retried.
        with_timeout(timeout, call).await.map_err(|e| match e {
            DriverError::Invalid { .. } | DriverError::Unsupported(_) => driver_failure(e),
            other => ExecFailure::Retryable {
                error: format!("planning failed: {other}"),
                retry_after: None,
            },
        })
    }

    async fn run_provision(
        &self,
        job: &ProvisionJob,
        log: &mut RedactingLog,
    ) -> Result<ProvisionResult, ExecFailure> {
        let Call {
            driver,
            mut req,
            timeout,
            target,
        } = self.request(job, &job.inputs.sources, true, log).await?;
        match self.plan(driver.as_ref(), &req, timeout, false).await? {
            Plan::NoEffects => log.info("the driver plans no changes"),
            Plan::Actions {
                actions,
                fingerprint,
            } => {
                log.info(&format!(
                    "the driver plans {} ({fingerprint})",
                    describe(&actions)
                ));
                let destructive = if actions.iter().any(|(a, _)| *a == Action::Replace) {
                    Some(DestructiveAction::Replace)
                } else if actions.iter().any(|(a, _)| *a == Action::Delete) {
                    Some(DestructiveAction::Delete)
                } else {
                    None
                };
                if let Some(action) = destructive {
                    self.permit(job, action, Some(&fingerprint)).await?;
                    req.permit = Some(fingerprint);
                }
            }
            Plan::Unknown if job.existing => {
                log.info("the driver cannot preview this change, so its impact is unknown");
                self.permit(job, DestructiveAction::Unknown, None).await?;
            }
            Plan::Unknown => {}
        }
        let remaining = job.deadline.saturating_duration_since(Instant::now());
        let settled = converge(driver.as_ref(), &req, timeout.min(remaining)).await;
        let outputs = settled.result.map_err(driver_failure)?;
        let mut result = ProvisionResult {
            outputs: outputs.values,
            target,
            backend: outputs.footprint.backend,
            external_ids: outputs.footprint.external_ids,
            ..ProvisionResult::default()
        };
        let secrets = outputs.secrets.expose();
        log.secret(secrets.values().map(secret_text).collect::<Vec<_>>());
        for (key, value) in secrets {
            let name = format!("resources/{}/outputs/{key}", job.guresid);
            let reference = self
                .secrets
                .write(&name, &SecretValue::new(secret_text(value)))
                .await
                .map_err(|e| {
                    ExecFailure::Uncertain(format!(
                        "the resource provisioned, but its secret output `{key}` could not be \
                         stored: {e}"
                    ))
                })?;
            result.secret_outputs.insert(key.clone(), reference);
        }
        if let Some(cookie) = &settled.cookie {
            result.cookie = Some(self.seal_cookie(job, cookie)?);
        }
        Ok(result)
    }

    async fn run_deprovision(
        &self,
        job: &DeprovisionJob,
        log: &mut RedactingLog,
    ) -> Result<(), ExecFailure> {
        let job = &job.job;
        // Inputs recorded before sources were tracked: a resource's own output
        // prefix names the resource, anything else came from shared values.
        let sources = if job.inputs.sources.is_empty() {
            inferred_sources(job)
        } else {
            job.inputs.sources.clone()
        };
        // A deletion uses the definition as it was, whether or not it still
        // matches: the permit, checked by the planner, authorizes it.
        let Call {
            driver,
            mut req,
            timeout,
            ..
        } = self.request(job, &sources, false, log).await?;
        match self.plan(driver.as_ref(), &req, timeout, true).await? {
            Plan::Actions { fingerprint, .. } => {
                self.permit(job, DestructiveAction::Delete, Some(&fingerprint))
                    .await?;
                req.permit = Some(fingerprint);
            }
            Plan::NoEffects | Plan::Unknown => {}
        }
        let remaining = job.deadline.saturating_duration_since(Instant::now());
        remove(driver.as_ref(), &req, timeout.min(remaining))
            .await
            .result
            .map_err(driver_failure)
    }
}

impl<S> Executor for LocalExecutor<S>
where
    S: SecretUseStore + AccountStore + CatalogStore + 'static,
{
    async fn provision(&self, job: ProvisionJob) -> Result<ProvisionResult, ExecFailure> {
        let mut log = job.log.redacting();
        // The planner logs the failure it records.
        self.run_provision(&job, &mut log)
            .await
            .map_err(|f| log.failure(f))
    }

    async fn deprovision(&self, job: DeprovisionJob) -> Result<(), ExecFailure> {
        let mut log = job.job.log.redacting();
        // The planner logs the failure it records.
        self.run_deprovision(&job, &mut log)
            .await
            .map_err(|f| log.failure(f))
    }
}

fn cookie_aad(job: &ProvisionJob) -> String {
    format!("{COOKIE_AAD}:{}:{}", job.context.org, job.guresid)
}

fn describe(actions: &[(Action, String)]) -> String {
    let count = |action: Action| actions.iter().filter(|(a, _)| *a == action).count();
    format!(
        "{} to create, {} to update, {} to replace and {} to delete",
        count(Action::Create),
        count(Action::Update),
        count(Action::Replace),
        count(Action::Delete)
    )
}

/// The sources of a job recorded before they were tracked.
fn inferred_sources(job: &ProvisionJob) -> Vec<(SecretRef, SecretSource)> {
    let mut refs = BTreeSet::new();
    references(&job.inputs.secrets, &mut refs);
    references(&job.inputs.resource, &mut refs);
    refs.into_iter()
        .map(|r| {
            let source = r
                .reference
                .strip_prefix("resources/")
                .and_then(|rest| rest.split_once('/'))
                .and_then(|(guresid, _)| guresid.parse().ok())
                .map_or(SecretSource::Values, |guresid| SecretSource::Output {
                    guresid,
                });
            (r, source)
        })
        .collect()
}

/// Every secret reference in `r`.
fn references(r: &Resolved, out: &mut BTreeSet<SecretRef>) {
    match r {
        Resolved::Value(_) => {}
        Resolved::Secret(e) => out.extend(e.references().cloned()),
        Resolved::Array(items) => items.iter().for_each(|i| references(i, out)),
        Resolved::Object(fields) => fields.iter().for_each(|(_, f)| references(f, out)),
    }
}

/// `r` with every secret rendered from `read`.
fn render(r: &Resolved, read: &BTreeMap<SecretRef, SecretValue>) -> Result<Value, ExecFailure> {
    Ok(match r {
        Resolved::Value(v) => v.clone(),
        Resolved::Secret(e) => Value::String(e.render(|r| {
            read.get(r).map(|v| v.expose().to_owned()).ok_or_else(|| {
                ExecFailure::Permanent(format!("secret `{}` was not read", r.reference))
            })
        })?),
        Resolved::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| render(i, read))
                .collect::<Result<_, _>>()?,
        ),
        Resolved::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, f)| Ok((k.clone(), render(f, read)?)))
                .collect::<Result<_, ExecFailure>>()?,
        ),
    })
}

/// A rendered input object.
fn object(
    r: &Resolved,
    read: &BTreeMap<SecretRef, SecretValue>,
    what: &str,
) -> Result<Map<String, Value>, ExecFailure> {
    match render(r, read)? {
        Value::Object(map) => Ok(map),
        Value::Null => Ok(Map::new()),
        _ => Err(ExecFailure::Permanent(format!("{what} is not an object"))),
    }
}

/// Rendered values that resolved from secrets.
type Secrets = Map<String, Value>;

/// The resource inputs, split into fields with no secret inside and fields
/// that resolved from secrets.
fn split(
    r: &Resolved,
    read: &BTreeMap<SecretRef, SecretValue>,
) -> Result<(Map<String, Value>, Secrets), ExecFailure> {
    match r {
        Resolved::Value(Value::Object(plain)) => Ok((plain.clone(), Map::new())),
        Resolved::Value(Value::Null) => Ok((Map::new(), Map::new())),
        Resolved::Object(fields) => {
            let mut plain = Map::new();
            let mut secret = Map::new();
            for (k, f) in fields {
                if f.is_tainted() {
                    secret.insert(k.clone(), render(f, read)?);
                } else {
                    plain.insert(k.clone(), render(f, read)?);
                }
            }
            Ok((plain, secret))
        }
        _ => Err(ExecFailure::Permanent(
            "the resource inputs are not an object".into(),
        )),
    }
}

/// A secret output's value as stored: strings as they are, anything else as
/// JSON.
fn secret_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn driver_failure(e: DriverError) -> ExecFailure {
    match e {
        DriverError::Invalid { .. } | DriverError::Unsupported(_) => {
            ExecFailure::Permanent(e.to_string())
        }
        DriverError::Transient {
            message,
            retry_after,
        } => ExecFailure::Retryable {
            error: message,
            retry_after,
        },
        // The driver refused to start: nothing changed, and the conflicting
        // operation may finish.
        DriverError::Conflict(_) => ExecFailure::Retryable {
            error: e.to_string(),
            retry_after: None,
        },
        DriverError::Timeout(_) | DriverError::Uncertain(_) => {
            ExecFailure::Uncertain(e.to_string())
        }
    }
}

fn store_failure(e: StoreError, what: &str) -> ExecFailure {
    match e {
        StoreError::Forbidden(m) => ExecFailure::Permanent(m),
        StoreError::NotFound(_) => ExecFailure::Permanent(format!("{what}: {e}")),
        other => ExecFailure::Retryable {
            error: format!("{what}: {other}"),
            retry_after: None,
        },
    }
}

fn secret_failure(e: SecretError) -> ExecFailure {
    match e {
        SecretError::Store(e) => store_failure(e, "the secret could not be read"),
        other => ExecFailure::Permanent(other.to_string()),
    }
}

fn cloud_failure(e: CloudError) -> ExecFailure {
    match e {
        CloudError::Unavailable(m) => ExecFailure::Retryable {
            error: m,
            retry_after: None,
        },
        CloudError::Invalid(m) | CloudError::Rejected(m) | CloudError::Expiring(m) => {
            ExecFailure::Permanent(m)
        }
    }
}

#[cfg(test)]
mod tests;
