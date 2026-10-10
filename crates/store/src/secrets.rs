//! The built-in secret store and the [`SecretStore`] trait.
//!
//! Values are encrypted with AES-256-GCM under a master key. Every ciphertext
//! records the ID of the key that sealed it and is bound to its location through
//! the associated data, so rows cannot be swapped. Keys are identified by a digest
//! of their content, so a restored key is recognized without storing it.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use openporch_core::model::SecretRef;
use rand::RngCore;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use zeroize::Zeroizing;

use crate::StoreError;

/// The ID of the built-in store.
pub const BUILTIN_STORE: &str = "openporch";

/// A boxed future, so that secret stores can be used as trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A secret value. It is zeroed on drop, prints as `[REDACTED]` and cannot be
/// serialized, so it does not end up in logs or API responses by accident.
#[derive(Clone)]
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    /// Wraps a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// The plaintext. Only the executor calls this, right before driver use.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Self::new)
    }
}

/// A secret given on input: either a reference or a value to write into the primary
/// store.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SecretInput {
    /// An existing reference.
    Ref(SecretRef),
    /// A value to write; it is replaced by a reference and never returned.
    Value(NewSecret),
}

/// `{value}`: a secret value to write.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSecret {
    /// The value.
    pub value: SecretValue,
}

/// Secret failures. Messages name stores, references and key IDs, never values.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The referenced secret or version does not exist.
    #[error("secret `{reference}`{} not found in store `{store}`", version.as_ref().map(|v| format!(" version {v}")).unwrap_or_default())]
    NotFound {
        /// Store ID.
        store: String,
        /// Reference.
        reference: String,
        /// Requested version.
        version: Option<String>,
    },
    /// The reference is malformed.
    #[error("invalid secret reference `{reference}`: {reason}")]
    InvalidReference {
        /// Reference.
        reference: String,
        /// What is wrong.
        reason: String,
    },
    /// A master key needed to decrypt is not configured.
    #[error("master key `{key_id}` is not available")]
    MissingKey {
        /// The key ID.
        key_id: String,
    },
    /// Decryption failed: wrong key or tampered ciphertext.
    #[error("secret `{reference}` in store `{store}` could not be decrypted")]
    Decrypt {
        /// Store ID.
        store: String,
        /// Reference.
        reference: String,
    },
    /// A master key could not be loaded.
    #[error("invalid master key: {0}")]
    InvalidKey(String),
    /// The store's database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for SecretError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(e.into())
    }
}

/// A secret store.
pub trait SecretStore: Send + Sync {
    /// The store ID used in references.
    fn id(&self) -> &str;

    /// Writes a new version of `name` and returns a reference pinning it.
    fn write<'a>(
        &'a self,
        name: &'a str,
        value: &'a SecretValue,
    ) -> BoxFuture<'a, Result<SecretRef, SecretError>>;

    /// Reads the pinned version, or the latest when the reference has none.
    fn read<'a>(
        &'a self,
        reference: &'a SecretRef,
    ) -> BoxFuture<'a, Result<SecretValue, SecretError>>;
}

/// Checks a reference given on input. References never contain placeholders.
pub fn validate_reference(r: &SecretRef) -> Result<(), SecretError> {
    let invalid = |reason: &str| SecretError::InvalidReference {
        reference: r.reference.clone(),
        reason: reason.to_owned(),
    };
    if r.store.is_empty() || r.reference.is_empty() {
        return Err(invalid("`store` and `ref` are required"));
    }
    let fields = [Some(&r.store), Some(&r.reference), r.version.as_ref()];
    if fields.iter().flatten().any(|s| s.contains("${")) {
        return Err(invalid("placeholders are not allowed in secret references"));
    }
    if fields
        .iter()
        .flatten()
        .any(|s| s.len() > 512 || s.chars().any(char::is_control))
    {
        return Err(invalid(
            "references are at most 512 characters without control characters",
        ));
    }
    Ok(())
}

/// Turns a secret input into a reference: references pass through after
/// validation, values are written to `primary` under `name`.
pub async fn materialize(
    primary: &dyn SecretStore,
    name: &str,
    input: &SecretInput,
) -> Result<SecretRef, SecretError> {
    match input {
        SecretInput::Ref(r) => {
            validate_reference(r)?;
            Ok(r.clone())
        }
        SecretInput::Value(v) => primary.write(name, &v.value).await,
    }
}

/// A 256-bit master key.
pub struct MasterKey {
    id: String,
    key: Zeroizing<[u8; 32]>,
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKey").field("id", &self.id).finish()
    }
}

impl MasterKey {
    /// A key from 32 raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        let mut h = Sha256::new();
        h.update(b"openporch master key v1\0");
        h.update(bytes);
        let digest = h.finalize();
        let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        Self {
            id: format!("mk-{id}"),
            key: Zeroizing::new(bytes),
        }
    }

    /// A key from its standard base64 encoding, as read from the environment or a file.
    pub fn from_base64(s: &str) -> Result<Self, SecretError> {
        let bytes = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(s.trim())
                .map_err(|_| SecretError::InvalidKey("not valid base64".into()))?,
        );
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| SecretError::InvalidKey("expected 32 bytes".into()))?;
        Ok(Self::from_bytes(arr))
    }

    /// A new random key.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let key = Self::from_bytes(bytes);
        zeroize::Zeroize::zeroize(&mut bytes);
        key
    }

    /// The key's standard base64 encoding, for writing a newly generated key to its file.
    pub fn to_base64(&self) -> Zeroizing<String> {
        Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(*self.key))
    }

    /// The key ID, derived from the key.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Ciphertext with the ID of the key that sealed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    /// The sealing key.
    pub key_id: String,
    /// The 96-bit nonce.
    pub nonce: Vec<u8>,
    /// Ciphertext with the authentication tag.
    pub ciphertext: Vec<u8>,
}

/// The configured master keys: one active key for new ciphertext and retired keys
/// that still decrypt older entries.
#[derive(Debug)]
pub struct Keyring {
    active: String,
    keys: BTreeMap<String, MasterKey>,
}

impl Keyring {
    /// A keyring that seals with `active` and also opens with `retired`.
    pub fn new(active: MasterKey, retired: impl IntoIterator<Item = MasterKey>) -> Self {
        let active_id = active.id.clone();
        let mut keys: BTreeMap<String, MasterKey> =
            retired.into_iter().map(|k| (k.id.clone(), k)).collect();
        keys.insert(active_id.clone(), active);
        Self {
            active: active_id,
            keys,
        }
    }

    /// Parses keys from text: one base64 key per line, the first active and the
    /// rest retired. Blank lines and lines starting with `#` are ignored.
    pub fn from_text(text: &str) -> Result<Self, SecretError> {
        let mut keys = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(MasterKey::from_base64);
        let active = keys
            .next()
            .ok_or_else(|| SecretError::InvalidKey("no key found".into()))??;
        Ok(Self::new(active, keys.collect::<Result<Vec<_>, _>>()?))
    }

    /// Reads keys from the environment variable `var`, in the [`Keyring::from_text`]
    /// format. Returns `None` when the variable is unset.
    pub fn from_env(var: &str) -> Result<Option<Self>, SecretError> {
        match std::env::var(var) {
            Ok(text) => Self::from_text(&text).map(Some),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(SecretError::InvalidKey(format!(
                "`{var}` is not valid UTF-8"
            ))),
        }
    }

    /// Reads keys from a file in the [`Keyring::from_text`] format. On Unix the file
    /// must not be readable by group or others.
    pub fn from_file(path: &std::path::Path) -> Result<Self, SecretError> {
        let invalid = |what: String| SecretError::InvalidKey(format!("{}: {what}", path.display()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|e| invalid(e.to_string()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(invalid(format!(
                    "permissions {:o} allow access by other users; use 0600",
                    mode & 0o777
                )));
            }
        }
        let text =
            Zeroizing::new(std::fs::read_to_string(path).map_err(|e| invalid(e.to_string()))?);
        Self::from_text(&text)
    }

    /// The ID of the key used for new ciphertext.
    pub fn active_id(&self) -> &str {
        &self.active
    }

    /// Fails with the first required key ID that is not configured.
    pub fn ensure_keys<S: AsRef<str>>(&self, required: &[S]) -> Result<(), SecretError> {
        match required
            .iter()
            .find(|id| !self.keys.contains_key(id.as_ref()))
        {
            Some(id) => Err(SecretError::MissingKey {
                key_id: id.as_ref().to_owned(),
            }),
            None => Ok(()),
        }
    }

    /// Encrypts `plaintext` bound to `aad`.
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Sealed {
        let key = &self.keys[&self.active];
        let cipher = Aes256Gcm::new_from_slice(key.key.as_slice()).expect("32-byte key");
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("AES-GCM encryption of in-memory data does not fail");
        Sealed {
            key_id: self.active.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        }
    }

    /// Decrypts `sealed`, which must have been sealed with the same `aad`. Returns
    /// `None` when authentication fails.
    pub fn open(
        &self,
        aad: &[u8],
        sealed: &Sealed,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, SecretError> {
        let key = self
            .keys
            .get(&sealed.key_id)
            .ok_or_else(|| SecretError::MissingKey {
                key_id: sealed.key_id.clone(),
            })?;
        if sealed.nonce.len() != 12 {
            return Ok(None);
        }
        let cipher = Aes256Gcm::new_from_slice(key.key.as_slice()).expect("32-byte key");
        Ok(cipher
            .decrypt(
                Nonce::from_slice(&sealed.nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad,
                },
            )
            .ok()
            .map(Zeroizing::new))
    }
}

/// The built-in store: versioned AES-256-GCM entries in the orchestrator database.
#[derive(Clone)]
pub struct BuiltinSecretStore {
    pool: SqlitePool,
    keyring: std::sync::Arc<Keyring>,
    org: String,
}

impl BuiltinSecretStore {
    /// The built-in store of `org` in `store`'s database.
    pub fn new(store: &crate::SqliteStore, keyring: std::sync::Arc<Keyring>, org: &str) -> Self {
        Self {
            pool: store.pool().clone(),
            keyring,
            org: org.to_owned(),
        }
    }

    fn aad(&self, reference: &str, version: i64) -> Vec<u8> {
        format!("openporch-secret:v1:{}:{reference}:{version}", self.org).into_bytes()
    }

    fn not_found(reference: &SecretRef) -> SecretError {
        SecretError::NotFound {
            store: BUILTIN_STORE.to_owned(),
            reference: reference.reference.clone(),
            version: reference.version.clone(),
        }
    }

    async fn write_impl(&self, name: &str, value: &SecretValue) -> Result<SecretRef, SecretError> {
        let reference = SecretRef {
            store: BUILTIN_STORE.to_owned(),
            reference: name.to_owned(),
            version: None,
        };
        validate_reference(&reference)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let latest: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(version) FROM secret_versions WHERE org_id = ? AND ref = ?",
        )
        .bind(&self.org)
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        let version = latest.unwrap_or(0) + 1;
        let sealed = self
            .keyring
            .seal(&self.aad(name, version), value.expose().as_bytes());
        sqlx::query(
            "INSERT INTO secret_versions (org_id, ref, version, key_id, nonce, ciphertext, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&self.org)
        .bind(name)
        .bind(version)
        .bind(&sealed.key_id)
        .bind(&sealed.nonce)
        .bind(&sealed.ciphertext)
        .bind(crate::now_ms())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SecretRef {
            version: Some(version.to_string()),
            ..reference
        })
    }

    async fn read_impl(&self, reference: &SecretRef) -> Result<SecretValue, SecretError> {
        validate_reference(reference)?;
        if reference.store != BUILTIN_STORE {
            return Err(SecretError::InvalidReference {
                reference: reference.reference.clone(),
                reason: format!("store `{}` is not the built-in store", reference.store),
            });
        }
        let version: Option<i64> = match &reference.version {
            Some(v) => Some(v.parse().map_err(|_| Self::not_found(reference))?),
            None => None,
        };
        let row: Option<(i64, String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT version, key_id, nonce, ciphertext FROM secret_versions \
             WHERE org_id = ? AND ref = ? AND (? IS NULL OR version = ?) \
             ORDER BY version DESC LIMIT 1",
        )
        .bind(&self.org)
        .bind(&reference.reference)
        .bind(version)
        .bind(version)
        .fetch_optional(&self.pool)
        .await?;
        let (version, key_id, nonce, ciphertext) = row.ok_or_else(|| Self::not_found(reference))?;
        let sealed = Sealed {
            key_id,
            nonce,
            ciphertext,
        };
        let plain = self
            .keyring
            .open(&self.aad(&reference.reference, version), &sealed)?
            .ok_or_else(|| SecretError::Decrypt {
                store: BUILTIN_STORE.to_owned(),
                reference: reference.reference.clone(),
            })?;
        let text = String::from_utf8(plain.to_vec()).map_err(|e| {
            zeroize::Zeroize::zeroize(&mut e.into_bytes());
            SecretError::Decrypt {
                store: BUILTIN_STORE.to_owned(),
                reference: reference.reference.clone(),
            }
        })?;
        Ok(SecretValue::new(text))
    }
}

impl SecretStore for BuiltinSecretStore {
    fn id(&self) -> &str {
        BUILTIN_STORE
    }

    fn write<'a>(
        &'a self,
        name: &'a str,
        value: &'a SecretValue,
    ) -> BoxFuture<'a, Result<SecretRef, SecretError>> {
        Box::pin(self.write_impl(name, value))
    }

    fn read<'a>(
        &'a self,
        reference: &'a SecretRef,
    ) -> BoxFuture<'a, Result<SecretValue, SecretError>> {
        Box::pin(self.read_impl(reference))
    }
}

#[cfg(test)]
mod tests;
