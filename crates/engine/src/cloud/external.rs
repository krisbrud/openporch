//! External secret stores: AWS Secrets Manager and SSM Parameter Store, read
//! and written with the credentials of the store's cloud account.

use std::sync::Arc;

use openporch_core::model::SecretRef;
use openporch_store::secrets::{BoxFuture, SecretError, SecretStore, SecretValue};
use openporch_store::{AccountStore, SecretStoreRecord, StoreError};
use serde_json::Value;

use super::{AwsCredentials, AwsSecret, CloudError, CredentialResolver, SecretService};

/// The AWS secret that `r` names in `store`.
pub fn external_secret(store: &SecretStoreRecord, r: &SecretRef) -> Result<AwsSecret, String> {
    let service = SecretService::of_store_type(&store.ty).ok_or_else(|| {
        format!(
            "secret store `{}` has type `{}`, which cannot be read",
            store.id, store.ty
        )
    })?;
    let region = store
        .config
        .get("region")
        .and_then(Value::as_str)
        .filter(|r| {
            !r.is_empty()
                && r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .ok_or_else(|| format!("secret store `{}` has no valid `config.region`", store.id))?;
    if r.reference.is_empty() {
        return Err(format!(
            "a reference into secret store `{}` names no secret",
            store.id
        ));
    }
    if service == SecretService::Ssm
        && r.version
            .as_deref()
            .is_some_and(|v| v.parse::<u64>().is_err())
    {
        return Err(format!(
            "secret `{}` in store `{}` pins version `{}`, but SSM parameter versions are numbers",
            r.reference,
            store.id,
            r.version.as_deref().unwrap_or_default()
        ));
    }
    Ok(AwsSecret {
        service,
        region: region.to_owned(),
        name: r.reference.clone(),
        version: r.version.clone(),
    })
}

/// Where a secret openporch writes under `name` lives in a store of `service`:
/// `openporch/<org>/<name>` in Secrets Manager, `/openporch/<org>/<name>` in
/// SSM. Names use the characters both services allow.
pub fn external_name(service: SecretService, org: &str, name: &str) -> Result<String, String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.');
    if name.is_empty() || !name.chars().all(allowed) || name.contains("//") {
        return Err(format!(
            "secret `{name}` cannot be written to an AWS store: names use letters, digits, `/`, `_`, `-` and `.`"
        ));
    }
    Ok(match service {
        SecretService::SecretsManager => format!("openporch/{org}/{name}"),
        SecretService::Ssm => format!("/openporch/{org}/{name}"),
    })
}

/// The record of which secrets openporch created in external stores.
pub trait CreatedSecrets: Send + Sync {
    /// `Some(true)` when openporch created `name` in `store`, `Some(false)`
    /// while a create of it is pending, `None` otherwise.
    fn state<'a>(
        &'a self,
        org: &'a str,
        store: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<bool>, StoreError>>;

    /// Records a pending (`confirmed` false) or finished create of `name`.
    fn record<'a>(
        &'a self,
        org: &'a str,
        store: &'a str,
        name: &'a str,
        confirmed: bool,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
}

impl<S: AccountStore> CreatedSecrets for S {
    fn state<'a>(
        &'a self,
        org: &'a str,
        store: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<bool>, StoreError>> {
        Box::pin(self.external_secret_state(org, store, name))
    }

    fn record<'a>(
        &'a self,
        org: &'a str,
        store: &'a str,
        name: &'a str,
        confirmed: bool,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(self.record_external_secret(org, store, name, confirmed))
    }
}

/// An AWS store, opened with fresh credentials of its cloud account.
///
/// A write records a pending create, creates the secret at its generated name
/// and confirms the record; only then do later writes add versions to it. A
/// secret someone else put at that name is refused, never written over. A
/// create retried while its record is pending, because its answer or the
/// confirmation was lost, adopts the existing secret as the one it created.
pub struct ExternalStore {
    cloud: Arc<CredentialResolver>,
    credentials: AwsCredentials,
    record: SecretStoreRecord,
    org: String,
    created: Arc<dyn CreatedSecrets>,
}

impl ExternalStore {
    /// Opens `record` with `credentials` of its cloud account.
    pub fn new(
        cloud: Arc<CredentialResolver>,
        credentials: AwsCredentials,
        record: SecretStoreRecord,
        org: impl Into<String>,
        created: Arc<dyn CreatedSecrets>,
    ) -> Self {
        Self {
            cloud,
            credentials,
            record,
            org: org.into(),
            created,
        }
    }

    /// The store's cloud account, from its configuration.
    pub fn account_of(record: &SecretStoreRecord) -> Option<&str> {
        record.config.get("cloud_account").and_then(Value::as_str)
    }

    fn failed(&self, e: CloudError) -> SecretError {
        let store = self.record.id.clone();
        match e {
            CloudError::Unavailable(reason)
            | CloudError::Throttled {
                message: reason, ..
            } => SecretError::Unavailable { store, reason },
            CloudError::Invalid(reason)
            | CloudError::Rejected(reason)
            | CloudError::Expiring(reason) => SecretError::Refused { store, reason },
        }
    }
}

impl SecretStore for ExternalStore {
    fn id(&self) -> &str {
        &self.record.id
    }

    fn write<'a>(
        &'a self,
        name: &'a str,
        value: &'a SecretValue,
    ) -> BoxFuture<'a, Result<SecretRef, SecretError>> {
        Box::pin(async move {
            let refused = |reason: String| SecretError::Refused {
                store: self.record.id.clone(),
                reason,
            };
            let mut secret = external_secret(
                &self.record,
                &SecretRef {
                    store: self.record.id.clone(),
                    reference: name.to_owned(),
                    version: None,
                },
            )
            .map_err(refused)?;
            secret.name = external_name(secret.service, &self.org, name).map_err(refused)?;
            let store = self.record.id.as_str();
            let org = self.org.as_str();
            let state = self.created.state(org, store, &secret.name).await?;
            let write = |create| {
                self.cloud
                    .write_secret(&self.credentials, &secret, value, org, create)
            };
            let version = match state {
                Some(true) => write(false).await,
                None | Some(false) => {
                    if state.is_none() {
                        self.created.record(org, store, &secret.name, false).await?;
                    }
                    match write(true).await {
                        Ok(Some(version)) => Ok(Some(version)),
                        // The pending create may have happened: add a version.
                        Ok(None) if state == Some(false) => write(false).await,
                        other => other,
                    }
                }
            }
            .map_err(|e| self.failed(e))?
            .ok_or_else(|| {
                refused(format!(
                    "{secret} already exists and was not created by openporch, so it is not \
                     written over"
                ))
            })?;
            if state != Some(true) {
                self.created.record(org, store, &secret.name, true).await?;
            }
            Ok(SecretRef {
                store: self.record.id.clone(),
                reference: secret.name,
                version: Some(version),
            })
        })
    }

    fn read<'a>(
        &'a self,
        reference: &'a SecretRef,
    ) -> BoxFuture<'a, Result<SecretValue, SecretError>> {
        Box::pin(async move {
            let secret = external_secret(&self.record, reference).map_err(|reason| {
                SecretError::Refused {
                    store: self.record.id.clone(),
                    reason,
                }
            })?;
            self.cloud
                .read_secret(&self.credentials, &secret)
                .await
                .map_err(|e| self.failed(e))?
                .ok_or_else(|| SecretError::NotFound {
                    store: self.record.id.clone(),
                    reference: reference.reference.clone(),
                    version: reference.version.clone(),
                })
        })
    }
}
