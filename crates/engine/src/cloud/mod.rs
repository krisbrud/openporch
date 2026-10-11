//! Cloud credentials for cloud accounts. Only the executor uses them: they are
//! obtained immediately before use, cached in memory for no longer than they are
//! valid, and never stored, logged or returned by the API.
//!
//! Milestone 1 resolves AWS accounts: `aws` from an access key whose secret is a
//! reference, and `aws-role` by assuming a role with an external ID from the host's
//! own identity. Calls to AWS go through [`Sts`], so tests use a fake.

mod aws;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use openporch_core::model::{CloudAccount, CloudAccountType};
use openporch_store::secrets::{BoxFuture, SecretStore, SecretValue};
use serde_json::Value;

pub use aws::AwsSts;

/// Extra validity required beyond an operation's window, for clock skew and the
/// time the call itself takes.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// How long the credentials for an account check must stay valid.
pub const CHECK_WINDOW: Duration = Duration::from_secs(60);

/// The role session length when an `aws-role` account sets none.
pub const DEFAULT_SESSION: Duration = Duration::from_secs(3600);

/// The keys a driver receives credentials under, and nothing else.
pub const DRIVER_CREDENTIAL_KEYS: [&str; 4] =
    ["AccessKeyId", "SecretAccessKey", "SessionToken", "Region"];

/// AWS credentials. Prints without its secrets.
#[derive(Clone)]
pub struct AwsCredentials {
    /// The access key ID.
    pub access_key_id: String,
    /// The secret access key.
    pub secret_access_key: SecretValue,
    /// The session token of temporary credentials.
    pub session_token: Option<SecretValue>,
    /// The region the account works in, if it names one.
    pub region: Option<String>,
    /// When temporary credentials expire.
    pub expires_at: Option<SystemTime>,
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &self.secret_access_key)
            .field("session_token", &self.session_token)
            .field("region", &self.region)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl AwsCredentials {
    /// The credentials as a driver receives them: exactly
    /// [`DRIVER_CREDENTIAL_KEYS`], with an empty session token for static keys.
    pub fn driver_inputs(&self) -> BTreeMap<&'static str, SecretValue> {
        BTreeMap::from([
            ("AccessKeyId", SecretValue::new(self.access_key_id.clone())),
            ("SecretAccessKey", self.secret_access_key.clone()),
            (
                "SessionToken",
                self.session_token
                    .clone()
                    .unwrap_or_else(|| SecretValue::new("")),
            ),
            (
                "Region",
                SecretValue::new(self.region.clone().unwrap_or_default()),
            ),
        ])
    }

    /// Whether the credentials stay valid for `window` (plus [`EXPIRY_MARGIN`]) from
    /// `now`. Static keys do not expire.
    pub fn valid_for(&self, now: SystemTime, window: Duration) -> bool {
        self.expires_at
            .is_none_or(|at| at >= now + window + EXPIRY_MARGIN)
    }

    /// Removes the secret parts from a message, in case a provider echoes them.
    fn redact(&self, message: &str) -> String {
        let mut out = message.to_owned();
        for secret in [Some(&self.secret_access_key), self.session_token.as_ref()]
            .into_iter()
            .flatten()
        {
            if !secret.expose().is_empty() {
                out = out.replace(secret.expose(), "[REDACTED]");
            }
        }
        out
    }
}

/// Who a set of credentials belongs to, as STS `GetCallerIdentity` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    /// The identity ARN.
    pub arn: String,
    /// The AWS account ID.
    pub account: String,
    /// The unique ID of the identity.
    pub user_id: String,
}

/// Credential failures. Messages never contain credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloudError {
    /// The account's configuration cannot produce credentials.
    #[error("{0}")]
    Invalid(String),
    /// AWS refused the request; the message carries its error code and message.
    #[error("{0}")]
    Rejected(String),
    /// AWS or a secret store could not be reached, or the host has no identity.
    #[error("{0}")]
    Unavailable(String),
    /// No credentials can stay valid for the whole operation.
    #[error("{0}")]
    Expiring(String),
}

/// An STS `AssumeRole` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssumeRole {
    /// The role to assume.
    pub role_arn: String,
    /// The external ID the role's trust policy requires.
    pub external_id: String,
    /// The session name, which shows in CloudTrail.
    pub session_name: String,
    /// The requested session length.
    pub duration: Duration,
    /// The region to call STS in.
    pub region: Option<String>,
}

/// The AWS Security Token Service.
pub trait Sts: Send + Sync {
    /// Assumes a role from the host's own identity.
    fn assume_role(&self, request: AssumeRole)
    -> BoxFuture<'_, Result<AwsCredentials, CloudError>>;

    /// Reports who `credentials` belong to.
    fn caller_identity<'a>(
        &'a self,
        credentials: &'a AwsCredentials,
    ) -> BoxFuture<'a, Result<CallerIdentity, CloudError>>;
}

/// A source of the current time, replaceable in tests.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// A role session kept for reuse while it stays valid.
struct Session {
    /// The account configuration the session was obtained for.
    fingerprint: String,
    credentials: AwsCredentials,
}

/// Resolves cloud accounts into credentials, reusing role sessions while they stay
/// valid for the requested window and assuming the role again otherwise.
pub struct CredentialResolver {
    sts: Arc<dyn Sts>,
    clock: Clock,
    sessions: Mutex<HashMap<String, Session>>,
}

impl fmt::Debug for CredentialResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialResolver").finish_non_exhaustive()
    }
}

impl CredentialResolver {
    /// A resolver using the system clock.
    pub fn new(sts: Arc<dyn Sts>) -> Self {
        Self::with_clock(sts, Arc::new(SystemTime::now))
    }

    /// A resolver using `clock`.
    pub fn with_clock(sts: Arc<dyn Sts>, clock: Clock) -> Self {
        Self {
            sts,
            clock,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Credentials for `account` that stay valid for `window`. Secret references are
    /// read from `secrets`. Fails before returning anything that would expire within
    /// the window.
    pub async fn resolve(
        &self,
        account: &CloudAccount,
        secrets: &dyn SecretStore,
        window: Duration,
    ) -> Result<AwsCredentials, CloudError> {
        match account.ty {
            CloudAccountType::Aws => static_keys(account, secrets).await,
            CloudAccountType::AwsRole => self.role_session(account, window).await,
            CloudAccountType::AwsIdentity => Err(CloudError::Invalid(format!(
                "cloud account `{}` is of type `aws-identity`, which needs the openporch \
                 OIDC provider that this server does not offer yet",
                account.id
            ))),
        }
    }

    /// Obtains credentials for `account` and asks STS who they belong to.
    pub async fn check(
        &self,
        account: &CloudAccount,
        secrets: &dyn SecretStore,
    ) -> Result<CallerIdentity, CloudError> {
        let credentials = self.resolve(account, secrets, CHECK_WINDOW).await?;
        self.sts
            .caller_identity(&credentials)
            .await
            .map_err(|e| redacted(e, &credentials))
    }

    async fn role_session(
        &self,
        account: &CloudAccount,
        window: Duration,
    ) -> Result<AwsCredentials, CloudError> {
        let request = role_request(account)?;
        if window + EXPIRY_MARGIN > request.duration {
            return Err(CloudError::Expiring(format!(
                "the operation needs credentials for {}s, but sessions of cloud account `{}` \
                 last {}s; raise `config.session_duration`",
                (window + EXPIRY_MARGIN).as_secs(),
                account.id,
                request.duration.as_secs()
            )));
        }
        let key = account.id.to_string();
        let fingerprint = format!(
            "{}\n{}\n{}\n{:?}",
            request.role_arn,
            request.external_id,
            request.duration.as_secs(),
            request.region
        );
        let now = (self.clock)();
        if let Some(s) = self.sessions().get(&key)
            && s.fingerprint == fingerprint
            && s.credentials.valid_for(now, window)
        {
            return Ok(s.credentials.clone());
        }
        let mut credentials = self.sts.assume_role(request.clone()).await?;
        credentials.region = request.region;
        let Some(expires_at) = credentials.expires_at else {
            return Err(CloudError::Unavailable(
                "STS returned role credentials without an expiry".into(),
            ));
        };
        if !credentials.valid_for((self.clock)(), window) {
            return Err(CloudError::Expiring(format!(
                "the role session for cloud account `{}` expires at {} seconds after the \
                 epoch, before the operation's {}s window ends",
                account.id,
                expires_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                window.as_secs()
            )));
        }
        self.sessions().insert(
            key,
            Session {
                fingerprint,
                credentials: credentials.clone(),
            },
        );
        Ok(credentials)
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        // A panic while holding the lock leaves only a cache behind.
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn redacted(e: CloudError, credentials: &AwsCredentials) -> CloudError {
    match e {
        CloudError::Invalid(m) => CloudError::Invalid(credentials.redact(&m)),
        CloudError::Rejected(m) => CloudError::Rejected(credentials.redact(&m)),
        CloudError::Unavailable(m) => CloudError::Unavailable(credentials.redact(&m)),
        CloudError::Expiring(m) => CloudError::Expiring(credentials.redact(&m)),
    }
}

fn config_str<'a>(account: &'a CloudAccount, key: &str) -> Result<&'a str, CloudError> {
    account
        .config
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            CloudError::Invalid(format!(
                "cloud account `{}` has no `config.{key}`",
                account.id
            ))
        })
}

fn region(account: &CloudAccount) -> Option<String> {
    account
        .config
        .get("region")
        .and_then(Value::as_str)
        .filter(|r| !r.trim().is_empty())
        .map(str::to_owned)
}

/// Reads the secret access key of an `aws` account from its reference.
async fn static_keys(
    account: &CloudAccount,
    secrets: &dyn SecretStore,
) -> Result<AwsCredentials, CloudError> {
    let access_key_id = config_str(account, "access_key_id")?.to_owned();
    let reference = account
        .secret_refs
        .get("secret_access_key")
        .ok_or_else(|| {
            CloudError::Invalid(format!(
                "cloud account `{}` has no `secret_access_key` reference",
                account.id
            ))
        })?;
    if reference.store != secrets.id() {
        return Err(CloudError::Unavailable(format!(
            "secret store `{}` cannot be read yet; only `{}` can",
            reference.store,
            secrets.id()
        )));
    }
    let secret_access_key = secrets.read(reference).await.map_err(|e| {
        CloudError::Unavailable(format!(
            "the secret access key of cloud account `{}` could not be read: {e}",
            account.id
        ))
    })?;
    Ok(AwsCredentials {
        access_key_id,
        secret_access_key,
        session_token: None,
        region: region(account),
        expires_at: None,
    })
}

/// The `AssumeRole` request for an `aws-role` account.
fn role_request(account: &CloudAccount) -> Result<AssumeRole, CloudError> {
    let duration = match account.config.get("session_duration") {
        None => DEFAULT_SESSION,
        Some(v) => Duration::from_secs(v.as_u64().ok_or_else(|| {
            CloudError::Invalid(format!(
                "cloud account `{}` has a non-numeric `config.session_duration`",
                account.id
            ))
        })?),
    };
    Ok(AssumeRole {
        role_arn: config_str(account, "role_arn")?.to_owned(),
        external_id: config_str(account, "external_id")?.to_owned(),
        // Account IDs only use characters STS allows in session names.
        session_name: format!("openporch-{}", account.id)
            .chars()
            .take(64)
            .collect(),
        duration,
        region: region(account),
    })
}

#[cfg(test)]
mod tests;
