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
use openporch_core::ids::{AccountId, DefinitionId, Descriptor};
use openporch_core::model::{ResourceContext, SecretRef};
use openporch_core::placeholder::Resolved;
use openporch_drivers::{
    Action, Cookie, Driver, DriverContext, DriverError, DriverOutputs, DriverRegistry,
    DriverRequest, Observation, Operation, Plan, SecretMap, call_timeout, converge, remove,
    with_timeout,
};
use openporch_store::secrets::{Keyring, Sealed, SecretError, SecretStore, SecretValue};
use openporch_store::{AccountStore, CatalogStore, SecretOwner, SecretUseStore, StoreError};
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::cloud::{
    AwsCredentials, CHECK_WINDOW, CloudError, CredentialResolver, EcrDigest, EcrImage, Latest,
    S3Object, S3Version, pinned_image,
};
use crate::planner::{
    ArtifactCheck, ArtifactPin, DeprovisionJob, DestructiveAction, ExecFailure, Executor, ImagePin,
    JobContext, JobLog, Observed, PermitCheck, PermitRequest, ProvisionJob, ProvisionResult,
    RedactingLog, SecretSource,
};
use crate::registry::{Lookup, OciImage, Reference, Registry, pinned_from_tag};

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
    /// Reads images in registries other than ECR; none when they are not
    /// pinned or checked.
    registry: Option<Arc<dyn Registry>>,
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
            registry: None,
        }
    }

    /// Pins and checks images in registries other than ECR with `registry`.
    pub fn with_registry(mut self, registry: Arc<dyn Registry>) -> Self {
        self.registry = Some(registry);
        self
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
            resource_secret_paths: secret_paths(&job.inputs.resource),
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
            pin: job.pin.clone(),
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

        // The definition's own secrets and its cloud account are the
        // definition's to give, so only to deployments its criteria match.
        if check_criteria && (!definition_refs.is_empty() || job.driver_account.is_some()) {
            self.check_criteria(job).await?;
        }
        let definition = job.definition_id.as_str();
        let mut read = BTreeMap::new();
        for r in &definition_refs {
            self.authorize(&job.context, &[SecretOwner::Definition(definition)], r)
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
            self.authorize(&job.context, &owners, r).await?;
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
        self.definition_matches(&job.context, &job.definition_id, &job.descriptor)
            .await
    }

    /// Succeeds when definition `definition_id` still matches `descriptor` in
    /// `c`'s application and environment.
    async fn definition_matches(
        &self,
        c: &JobContext,
        definition_id: &DefinitionId,
        descriptor: &Descriptor,
    ) -> Result<(), ExecFailure> {
        let record = self
            .store
            .definition(c.org.as_str(), definition_id.as_str())
            .await
            .map_err(|e| store_failure(e, "the resource definition could not be read"))?
            .ok_or_else(|| {
                ExecFailure::Permanent(format!(
                    "definition `{definition_id}` no longer exists, so its secrets are not read"
                ))
            })?;
        let ctx = ResourceContext {
            org: c.org.clone(),
            app: c.app.clone(),
            env: c.env.clone(),
            env_type: c.env_type.clone(),
            res: descriptor.clone(),
        };
        if definition_weight(&record.definition, &ctx).is_none() {
            return Err(ExecFailure::Permanent(format!(
                "definition `{definition_id}` no longer matches `{descriptor}` in `{}/{}`, so \
                 its secrets are not read",
                c.app, c.env
            )));
        }
        Ok(())
    }

    /// Succeeds when any of `owners` may use `r` in the job's application and
    /// environment.
    async fn authorize(
        &self,
        c: &JobContext,
        owners: &[SecretOwner<'_>],
        r: &SecretRef,
    ) -> Result<(), ExecFailure> {
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
        let credentials = self
            .account_credentials(&job.context, id, window, log)
            .await?;
        let identity = self
            .cloud
            .identity(&credentials)
            .await
            .map_err(cloud_failure)?;
        if job.target.is_none() && job.existing {
            log.warn(&format!(
                "`{}` has no recorded AWS account; it is now recorded as {}",
                job.descriptor, identity.account
            ));
        }
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

    /// Credentials for cloud account `id`, used in `c`'s scope, valid for
    /// `window`. Their secrets are registered with `log`.
    async fn account_credentials(
        &self,
        c: &JobContext,
        id: &AccountId,
        window: Duration,
        log: &mut RedactingLog,
    ) -> Result<AwsCredentials, ExecFailure> {
        let read_at = self.cloud.generation();
        let account = self
            .store
            .account(c.org.as_str(), id.as_str())
            .await
            .map_err(|e| store_failure(e, "the cloud account could not be read"))?
            .ok_or_else(|| ExecFailure::Permanent(format!("cloud account `{id}` does not exist")))?
            .account;
        for r in account.secret_refs.values() {
            self.authorize(c, &[SecretOwner::Account(id.as_str())], r)
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
        Ok(credentials)
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
        plan: Option<(&str, &[(Action, String)])>,
    ) -> Result<(), ExecFailure> {
        let planned: Vec<(String, String)> = plan
            .into_iter()
            .flat_map(|(_, actions)| actions)
            .map(|(a, address)| (action_name(a).to_owned(), address.clone()))
            .collect();
        let c = &job.context;
        let descriptor = job.descriptor.to_string();
        let check = self.checks.check(PermitRequest {
            org: c.org.as_str(),
            app: c.app.as_str(),
            env: c.env.as_str(),
            descriptor: &descriptor,
            guresid: Some(job.guresid.as_str()),
            deployment: Some(&c.deployment_id),
            action,
            permits: &c.permits,
            plan: plan.map(|(fingerprint, _)| fingerprint),
            planned: &planned,
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
                    self.permit(job, action, Some((&fingerprint, &actions)))
                        .await?;
                    req.permit = Some(fingerprint);
                }
            }
            Plan::Unknown if job.existing => {
                log.info("the driver cannot preview this change, so its impact is unknown");
                self.permit(job, DestructiveAction::Unknown, None).await?;
            }
            Plan::Unknown => {}
        }
        let timeout = timeout.min(job.deadline.saturating_duration_since(Instant::now()));
        let target = match self.refresh(job, &mut req, timeout, log).await? {
            Some(refreshed) => Some(refreshed),
            None => target,
        };
        let settled = converge(driver.as_ref(), &req, timeout).await;
        let accepted = settled.cookie != req.cookie;
        let outputs = settled.result.map_err(|e| mutation_failure(e, accepted))?;
        // The driver made the change, so a result that cannot be kept, such as
        // outputs its type does not allow, leaves the resource for recovery to
        // observe rather than as if nothing changed.
        self.provisioned(job, outputs, settled.cookie.as_ref(), target, log)
            .await
            .map_err(|f| match f {
                ExecFailure::Permanent(error) | ExecFailure::Retryable { error, .. } => {
                    ExecFailure::Uncertain(format!("the driver made the change, but {error}"))
                }
                f => f,
            })
    }

    /// What a resource that provisioned returns: its outputs, with secret
    /// outputs written to the secret store and the cookie sealed.
    async fn provisioned(
        &self,
        job: &ProvisionJob,
        outputs: DriverOutputs,
        cookie: Option<&Cookie>,
        target: Option<String>,
        log: &mut RedactingLog,
    ) -> Result<ProvisionResult, ExecFailure> {
        let mut result = ProvisionResult {
            outputs: outputs.values,
            target,
            backend: outputs.footprint.backend,
            external_ids: outputs.footprint.external_ids,
            pin: outputs.footprint.pin,
            ..ProvisionResult::default()
        };
        let secrets = outputs.secrets.expose();
        // Outputs that do not match the type would reach the resources that use
        // them, so the resource fails, naming the output. Messages name outputs
        // and types, never values.
        if let Some(ty) = &job.output_schema {
            let names = secrets.keys().cloned().collect();
            ty.validate_outputs(&result.outputs, &names)
                .and_then(|()| {
                    secrets
                        .iter()
                        .try_for_each(|(name, value)| ty.validate_secret_output(name, value))
                })
                .map_err(|e| {
                    ExecFailure::Permanent(format!("the driver's outputs are invalid: {e}"))
                })?;
        }
        let mut texts = Vec::new();
        for value in secrets.values() {
            secret_texts(value, &mut texts);
        }
        log.secret(texts);
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
        if let Some(cookie) = cookie {
            result.cookie = Some(self.seal_cookie(job, cookie)?);
        }
        Ok(result)
    }

    async fn run_observe(
        &self,
        job: &ProvisionJob,
        deleting: bool,
        log: &mut RedactingLog,
    ) -> Result<Observed, ExecFailure> {
        if let Some(driver) = self.drivers.get(&job.driver_type)
            && !driver.info().capabilities.observe
        {
            return Ok(Observed::Unknown(format!(
                "driver `{}` cannot observe resources",
                job.driver_type
            )));
        }
        let sources = if job.inputs.sources.is_empty() {
            inferred_sources(job)
        } else {
            job.inputs.sources.clone()
        };
        // The operation being recovered was started with this definition, so
        // its secrets are read whether or not it still matches, as for a
        // deletion.
        let Call {
            driver,
            req,
            timeout,
            target,
        } = self.request(job, &sources, false, log).await?;
        let observation =
            with_timeout(timeout, driver.get(&req))
                .await
                .map_err(|e| ExecFailure::Retryable {
                    error: format!("the resource could not be observed: {e}"),
                    retry_after: None,
                })?;
        let repeatable = driver.info().capabilities.recover;
        Ok(match observation {
            Observation::Absent => Observed::Absent { repeatable },
            Observation::InProgress { .. } => Observed::InProgress,
            Observation::Failed { error } => Observed::Failed {
                error: error.to_string(),
                repeatable,
            },
            Observation::Unknown { reason } => Observed::Unknown(reason),
            // Only a creation's result is kept: for a resource that existed
            // before, nothing is stored.
            Observation::Ready { .. } if deleting || job.existing => Observed::Ready {
                result: Box::default(),
                repeatable,
            },
            Observation::Ready { outputs, cookie } => {
                // A driver that returns no cookie leaves the last one in place.
                let cookie = cookie.or(req.cookie.clone());
                Observed::Ready {
                    result: Box::new(
                        self.provisioned(job, outputs, cookie.as_ref(), target, log)
                            .await?,
                    ),
                    repeatable,
                }
            }
        })
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
            Plan::Actions {
                fingerprint,
                actions,
            } => {
                self.permit(
                    job,
                    DestructiveAction::Delete,
                    Some((&fingerprint, &actions)),
                )
                .await?;
                req.permit = Some(fingerprint);
            }
            Plan::NoEffects | Plan::Unknown => {}
        }
        let timeout = timeout.min(job.deadline.saturating_duration_since(Instant::now()));
        self.refresh(job, &mut req, timeout, log).await?;
        let settled = remove(driver.as_ref(), &req, timeout).await;
        let accepted = settled.cookie != req.cookie;
        settled.result.map_err(|e| mutation_failure(e, accepted))
    }

    /// Replaces the request's credentials, obtained for the plan, with ones
    /// that last for the whole mutation, checking their target again.
    async fn refresh(
        &self,
        job: &ProvisionJob,
        req: &mut DriverRequest,
        window: Duration,
        log: &mut RedactingLog,
    ) -> Result<Option<String>, ExecFailure> {
        let Some((credentials, target)) = self.credentials(job, window, log).await? else {
            return Ok(None);
        };
        req.credentials = Some(credentials);
        Ok(Some(target))
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

    async fn check_artifacts(&self, check: ArtifactCheck) -> Result<(), ExecFailure> {
        // Only an artifact pinned to a version, or an image pinned to a
        // digest, names something that can be gone; acceptance pins S3
        // objects to versions and ECR tags to digests.
        let versions: Vec<S3Version> = check
            .artifacts
            .iter()
            .filter_map(|a| S3Version::parse(a))
            .collect();
        let images: Vec<EcrDigest> = check
            .artifacts
            .iter()
            .filter_map(|a| EcrDigest::parse(a))
            .collect();
        if versions.is_empty() && images.is_empty() {
            return Ok(());
        }
        let mut log = JobLog::discard().redacting();
        self.definition_matches(&check.context, &check.definition_id, &check.descriptor)
            .await?;
        let credentials = self
            .account_credentials(&check.context, &check.account, CHECK_WINDOW, &mut log)
            .await
            .map_err(|f| log.failure(f))?;
        for version in &versions {
            match self.cloud.object_exists(Some(&credentials), version).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(ExecFailure::Permanent(format!(
                        "artifact `{version}` of `{}` no longer exists; nothing changed",
                        check.descriptor
                    )));
                }
                Err(e) => return Err(log.failure(cloud_failure(e))),
            }
        }
        for image in &images {
            match self.cloud.image_exists(&credentials, image).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(ExecFailure::Permanent(format!(
                        "image `{image}` of `{}` no longer exists; nothing changed",
                        check.descriptor
                    )));
                }
                Err(e) => return Err(log.failure(cloud_failure(e))),
            }
        }
        Ok(())
    }

    async fn check_pin(&self, job: ProvisionJob) -> Result<(), ExecFailure> {
        let mut log = job.log.redacting();
        let checked = async {
            // Reading the source's credentials needs the definition to still
            // match, as provisioning does.
            let Call {
                driver,
                req,
                timeout,
                ..
            } = self
                .request(&job, &job.inputs.sources, true, &mut log)
                .await?;
            // A check changes nothing, so running out of time is not uncertain.
            with_timeout(timeout, driver.check_pin(&req))
                .await
                .map_err(|e| match e {
                    DriverError::Timeout(_) => ExecFailure::Retryable {
                        error: format!("checking the pinned version failed: {e}"),
                        retry_after: None,
                    },
                    e => driver_failure(e),
                })
        }
        .await;
        checked.map_err(|f| log.failure(f))
    }

    async fn check_retained(&self, job: DeprovisionJob) -> Result<(), ExecFailure> {
        let job = &job.job;
        let mut log = job.log.redacting();
        let checked = async {
            // As for the deletion: the definition as it was, whether or not
            // it still matches.
            let sources = if job.inputs.sources.is_empty() {
                inferred_sources(job)
            } else {
                job.inputs.sources.clone()
            };
            let Call {
                driver,
                req,
                timeout,
                ..
            } = self.request(job, &sources, false, &mut log).await?;
            // A check changes nothing, so running out of time is not uncertain.
            with_timeout(timeout, driver.check_retained(&req))
                .await
                .map_err(|e| match e {
                    DriverError::Timeout(_) => ExecFailure::Retryable {
                        error: format!("checking the retained version failed: {e}"),
                        retry_after: None,
                    },
                    e => driver_failure(e),
                })
        }
        .await;
        checked.map_err(|f| log.failure(f))
    }

    async fn pin_artifact(&self, pin: ArtifactPin) -> Result<String, ExecFailure> {
        let object = S3Object::parse(&pin.artifact).ok_or_else(|| {
            ExecFailure::Permanent(format!(
                "artifact `{}` of `{}` is not an S3 object (`s3://<bucket>/<key>`)",
                pin.artifact, pin.descriptor
            ))
        })?;
        let mut log = JobLog::discard().redacting();
        self.definition_matches(&pin.context, &pin.definition_id, &pin.descriptor)
            .await?;
        let credentials = self
            .account_credentials(&pin.context, &pin.account, CHECK_WINDOW, &mut log)
            .await
            .map_err(|f| log.failure(f))?;
        match self.cloud.latest_version(Some(&credentials), &object).await {
            Ok(Latest::Version(v)) => Ok(object.at(v).to_string()),
            Ok(Latest::Unversioned) => Err(ExecFailure::Permanent(format!(
                "artifact `{object}` of `{}` needs an immutable identity, but its bucket does \
                 not keep versions: enable versioning on the bucket, or name an object version \
                 with `?versionId=`",
                pin.descriptor
            ))),
            Ok(Latest::Missing) => Err(ExecFailure::Permanent(format!(
                "artifact `{object}` of `{}` does not exist",
                pin.descriptor
            ))),
            Err(e) => Err(log.failure(cloud_failure(e))),
        }
    }

    async fn pin_image(&self, pin: ImagePin) -> Result<String, ExecFailure> {
        let image = EcrImage::parse(&pin.image).ok_or_else(|| {
            ExecFailure::Permanent(format!(
                "image `{}` of `{}` is not an ECR image named by tag",
                pin.image, pin.descriptor
            ))
        })?;
        let mut log = JobLog::discard().redacting();
        self.definition_matches(&pin.context, &pin.definition_id, &pin.descriptor)
            .await?;
        let credentials = self
            .account_credentials(&pin.context, &pin.account, CHECK_WINDOW, &mut log)
            .await
            .map_err(|f| log.failure(f))?;
        match self.cloud.image_digest(&credentials, &image).await {
            Ok(Some(digest)) if pinned_image(&image.at(&digest)) => Ok(image.at(&digest)),
            Ok(Some(digest)) => Err(ExecFailure::Permanent(format!(
                "image `{image}` of `{}` has digest `{digest}`, which is not a sha256 digest",
                pin.descriptor
            ))),
            Ok(None) => Err(ExecFailure::Permanent(format!(
                "image `{image}` of `{}` does not exist, or cloud account `{}` cannot see it",
                pin.descriptor, pin.account
            ))),
            Err(e) => Err(log.failure(cloud_failure(e))),
        }
    }

    async fn pin_registry_image(
        &self,
        descriptor: Descriptor,
        image: String,
    ) -> Result<String, ExecFailure> {
        let Some(registry) = &self.registry else {
            return Ok(image);
        };
        // What is not a reference to a tag is left to the driver, as before.
        let parsed = match OciImage::parse(&image) {
            Some(parsed) if matches!(parsed.reference, Reference::Tag(_)) => parsed,
            _ => return Ok(image),
        };
        match registry.lookup(&parsed).await {
            Ok(Lookup::Found(digest)) if pinned_image(&parsed.tagged_at(&digest)) => {
                Ok(parsed.tagged_at(&digest))
            }
            Ok(Lookup::Found(digest)) => Err(ExecFailure::Permanent(format!(
                "image `{image}` of `{descriptor}` has digest `{digest}`, which is not a sha256 \
                 digest"
            ))),
            Ok(Lookup::Missing) => Err(ExecFailure::Permanent(format!(
                "image `{image}` of `{descriptor}` does not exist in registry `{}`",
                parsed.host
            ))),
            Ok(Lookup::Private) => Err(ExecFailure::Permanent(format!(
                "image `{image}` of `{descriptor}` cannot be read without credentials, so its \
                 tag cannot be pinned; name it by digest as `<image>@sha256:<digest>`"
            ))),
            Ok(Lookup::Unlisted) => Err(ExecFailure::Permanent(format!(
                "image `{image}` of `{descriptor}` is in registry `{}` on a private address, so \
                 its tag cannot be pinned; list the registry in `images.private_registries`, or \
                 name the image by digest as `<image>@sha256:<digest>`",
                parsed.host
            ))),
            Ok(Lookup::Unresolved) => Err(ExecFailure::Retryable {
                error: format!(
                    "registry `{}` of image `{image}` does not resolve",
                    parsed.host
                ),
                retry_after: None,
            }),
            Err(e) => Err(cloud_failure(e)),
        }
    }

    async fn check_registry_images(
        &self,
        descriptor: Descriptor,
        images: Vec<String>,
    ) -> Result<(), ExecFailure> {
        let Some(registry) = &self.registry else {
            return Ok(());
        };
        for named in &images {
            let Some(image) = OciImage::parse(named) else {
                continue;
            };
            if !matches!(image.reference, Reference::Digest(_)) {
                continue;
            }
            match registry.lookup(&image).await {
                Ok(Lookup::Found(_)) => {}
                // One pinned here from a public tag must stay readable.
                Ok(Lookup::Private | Lookup::Unlisted) if pinned_from_tag(named) => {
                    return Err(ExecFailure::Permanent(format!(
                        "image `{named}` of `{descriptor}` was pinned from a public tag, but its \
                         registry no longer shows it without credentials; nothing changed"
                    )));
                }
                // One named by digest that needs credentials, or is in a
                // registry on a private address not listed, is not checked
                // here.
                Ok(Lookup::Private | Lookup::Unlisted) => {}
                Ok(Lookup::Unresolved) if pinned_from_tag(named) => {
                    return Err(ExecFailure::Retryable {
                        error: format!(
                            "registry `{}` of image `{named}` of `{descriptor}` does not resolve",
                            image.host
                        ),
                        retry_after: None,
                    });
                }
                // One named by digest in a registry the server cannot resolve
                // is left to its driver.
                Ok(Lookup::Unresolved) => {}
                Ok(Lookup::Missing) => {
                    return Err(ExecFailure::Permanent(format!(
                        "image `{named}` of `{descriptor}` no longer exists; nothing changed"
                    )));
                }
                Err(e) => return Err(cloud_failure(e)),
            }
        }
        Ok(())
    }

    async fn deprovision(&self, job: DeprovisionJob) -> Result<(), ExecFailure> {
        let mut log = job.job.log.redacting();
        // The planner logs the failure it records.
        self.run_deprovision(&job, &mut log)
            .await
            .map_err(|f| log.failure(f))
    }

    async fn observe(&self, job: ProvisionJob, deleting: bool) -> Result<Observed, ExecFailure> {
        let mut log = job.log.redacting();
        let observed = self
            .run_observe(&job, deleting, &mut log)
            .await
            .map_err(|f| log.failure(f))?;
        // What the driver said goes into records, so it is redacted too.
        Ok(match observed {
            Observed::Failed { error, repeatable } => Observed::Failed {
                error: log.redact(&error),
                repeatable,
            },
            Observed::Unknown(reason) => Observed::Unknown(log.redact(&reason)),
            other => other,
        })
    }
}

fn cookie_aad(job: &ProvisionJob) -> String {
    format!("{COOKIE_AAD}:{}:{}", job.context.org, job.guresid)
}

/// An action's name, as previews and approval requests show it.
fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Create => "create",
        Action::Update => "update",
        Action::Replace => "replace",
        Action::Delete => "delete",
    }
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

/// The path of every secret value inside `r`.
fn secret_paths(r: &Resolved) -> Vec<Vec<String>> {
    fn walk(r: &Resolved, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        let mut child = |key: String, r: &Resolved, out: &mut Vec<Vec<String>>| {
            path.push(key);
            walk(r, path, out);
            path.pop();
        };
        match r {
            Resolved::Value(_) => {}
            Resolved::Secret(_) => out.push(path.clone()),
            Resolved::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    child(i.to_string(), item, out);
                }
            }
            Resolved::Object(fields) => {
                for (k, f) in fields {
                    child(k.clone(), f, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(r, &mut Vec::new(), &mut out);
    out
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

/// Every text a secret output holds: the value as stored, and each string
/// inside a structured one.
fn secret_texts(v: &Value, out: &mut Vec<String>) {
    out.push(secret_text(v));
    if matches!(v, Value::Array(_) | Value::Object(_)) {
        secret_leaves(v, out);
    }
}

fn secret_leaves(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|i| secret_leaves(i, out)),
        Value::Object(fields) => fields.values().for_each(|f| secret_leaves(f, out)),
        _ => {}
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
        // Another operation holds the resource: observe it before mutating
        // again rather than retrying into it.
        DriverError::Conflict(_) => ExecFailure::Permanent(e.to_string()),
        DriverError::Timeout(_) | DriverError::Uncertain(_) => {
            ExecFailure::Uncertain(e.to_string())
        }
    }
}

/// A failed `put` or `delete`. Once the driver accepted the mutation, which
/// shows as a new cookie, any failure may have left changes behind.
fn mutation_failure(e: DriverError, accepted: bool) -> ExecFailure {
    match driver_failure(e) {
        f @ ExecFailure::Uncertain(_) => f,
        ExecFailure::Permanent(error) | ExecFailure::Retryable { error, .. } if accepted => {
            ExecFailure::Uncertain(format!(
                "the driver accepted the change, then failed: {error}"
            ))
        }
        f => f,
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
        CloudError::Throttled {
            message,
            retry_after,
        } => ExecFailure::Retryable {
            error: message,
            retry_after,
        },
        CloudError::Invalid(m) | CloudError::Rejected(m) | CloudError::Expiring(m) => {
            ExecFailure::Permanent(m)
        }
    }
}

#[cfg(test)]
mod tests;
