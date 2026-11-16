//! OpenID Connect login providers managed through the API, beside the ones in
//! the configuration file. Both serve `/auth/oidc/{id}/...`; an ID the file
//! configures cannot be used for one managed here. A provider is built from its
//! row when a login first needs it and rebuilt after the row changes. Its client
//! secret is written to the built-in store under
//! `identity-providers/<id>/client_secret`, which no other object owns.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use openporch_core::model::SecretRef;
use openporch_store::secrets::{BUILTIN_STORE, SecretStore};
use openporch_store::{LoginProviderRecord, LoginProviderStore};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::config::OidcLoginConfig;
use crate::error::{ApiError, ApiResult};
use crate::oidc_login::OidcLogin;
use crate::secret_inputs;
use crate::state::AppState;

/// Providers built from their rows, by ID, with the row's creation time and
/// generation, so a deleted and recreated provider is rebuilt too.
#[derive(Debug, Default)]
pub struct ProviderCache(Mutex<HashMap<String, (RowVersion, Arc<OidcLogin>)>>);

/// A provider row's creation time and generation.
type RowVersion = (i64, i64);

impl ProviderCache {
    fn get(&self, id: &str, version: RowVersion) -> Option<Arc<OidcLogin>> {
        let cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .get(id)
            .filter(|(v, _)| *v == version)
            .map(|(_, p)| p.clone())
    }

    fn put(&self, id: &str, version: RowVersion, provider: Arc<OidcLogin>) {
        let mut cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(id.to_owned(), (version, provider));
    }

    /// Forgets a provider, so the next login builds it from its row.
    pub fn forget(&self, id: &str) {
        let mut cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
        cache.remove(id);
    }
}

/// A provider's settings, as stored and returned: everything but the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The issuer, exactly as its discovery document and ID tokens name it.
    pub issuer: String,
    /// The OAuth client ID.
    pub client_id: String,
    /// This server's `/auth/oidc/<id>/callback` URL.
    pub callback_url: String,
    /// The pages a finished login may return the browser to.
    #[serde(default)]
    pub return_urls: Vec<String>,
    /// Also return to any port of the loopback address.
    #[serde(default)]
    pub loopback_return: bool,
    /// The scopes requested.
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// The ID token signing algorithms accepted.
    #[serde(default = "default_algorithms")]
    pub algorithms: Vec<String>,
}

fn default_scopes() -> Vec<String> {
    ["openid", "email", "profile"].map(String::from).to_vec()
}

fn default_algorithms() -> Vec<String> {
    vec!["RS256".into()]
}

impl Settings {
    fn login_config(&self, id: &str) -> OidcLoginConfig {
        OidcLoginConfig {
            id: id.to_owned(),
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            client_secret_file: None,
            callback_url: self.callback_url.clone(),
            return_urls: self.return_urls.clone(),
            loopback_return: self.loopback_return,
            scopes: self.scopes.clone(),
            algorithms: self.algorithms.clone(),
        }
    }

    fn from_record(record: &LoginProviderRecord) -> ApiResult<Self> {
        serde_json::from_value(Value::Object(record.config.clone())).map_err(|e| {
            ApiError::internal(format!(
                "login provider `{}` has unreadable settings: {e}",
                record.id
            ))
        })
    }

    fn to_map(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }
}

/// The built-in name of a provider's client secret.
pub fn secret_name(id: &str) -> String {
    format!("identity-providers/{id}/client_secret")
}

/// Checks a provider's ID and settings before they are stored.
pub fn check(state: &AppState, id: &str, settings: &Settings) -> ApiResult<()> {
    if id == "github" || !openporch_core::ids::is_valid_object_id(id) {
        return Err(ApiError::invalid_id(format!(
            "identity provider IDs are lowercase IDs other than `github`, not `{id}`"
        )));
    }
    if state.oidc_logins.contains_key(id) {
        return Err(ApiError::conflict(format!(
            "identity provider `{id}` is configured in the server's configuration file"
        )));
    }
    settings
        .login_config(id)
        .check(&|field| field.to_owned())
        .map_err(ApiError::invalid)
}

/// The record to store: `settings`, and the client secret's reference.
pub fn record(
    id: &str,
    settings: &Settings,
    client_secret: Option<SecretRef>,
) -> LoginProviderRecord {
    LoginProviderRecord {
        id: id.to_owned(),
        config: settings.to_map(),
        client_secret,
        generation: 0,
        created_at: 0,
    }
}

/// A provider as the API shows it.
#[derive(Debug, Serialize)]
pub struct View {
    /// The provider ID.
    pub id: String,
    /// `api` for providers managed through the API, `config` for the ones in the
    /// configuration file, which the API cannot change.
    pub source: &'static str,
    /// The settings.
    #[serde(flatten)]
    pub settings: Settings,
    /// The client secret's reference, for a confidential client managed through
    /// the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<SecretRef>,
    /// Whether the client authenticates with a secret.
    pub confidential: bool,
    /// Creation time, for providers managed through the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

impl View {
    /// A provider managed through the API.
    pub fn api(record: &LoginProviderRecord) -> ApiResult<Self> {
        Ok(Self {
            id: record.id.clone(),
            source: "api",
            settings: Settings::from_record(record)?,
            client_secret: record.client_secret.clone(),
            confidential: record.client_secret.is_some(),
            created_at: Some(record.created_at),
        })
    }

    /// A provider of the configuration file.
    pub fn config(config: &OidcLoginConfig) -> Self {
        Self {
            id: config.id.clone(),
            source: "config",
            settings: Settings {
                issuer: config.issuer.clone(),
                client_id: config.client_id.clone(),
                callback_url: config.callback_url.clone(),
                return_urls: config.return_urls.clone(),
                loopback_return: config.loopback_return,
                scopes: config.scopes.clone(),
                algorithms: config.algorithms.clone(),
            },
            client_secret: None,
            confidential: config.client_secret_file.is_some(),
            created_at: None,
        }
    }
}

/// Every provider people can sign in with: the configuration file's, then the
/// API's.
pub async fn all(state: &AppState) -> ApiResult<Vec<View>> {
    let mut views: Vec<View> = state.config.oidc_login.iter().map(View::config).collect();
    for record in state.store.login_providers(state.org()).await? {
        views.push(View::api(&record)?);
    }
    Ok(views)
}

/// The provider `id` for a login: one of the configuration file, or one managed
/// through the API, built from its row as it is now.
pub async fn provider(state: &AppState, id: &str) -> ApiResult<Arc<OidcLogin>> {
    if let Some(p) = state.oidc_logins.get(id) {
        return Ok(p.clone());
    }
    let not_found = || {
        ApiError::not_found(format!(
            "no OpenID Connect provider `{id}` is configured on this server"
        ))
    };
    if !openporch_core::ids::is_valid_object_id(id) {
        return Err(not_found());
    }
    let record = state
        .store
        .login_provider(state.org(), id)
        .await?
        .ok_or_else(not_found)?;
    let version = (record.created_at, record.generation);
    if let Some(p) = state.api_logins.get(id, version) {
        return Ok(p);
    }
    let settings = Settings::from_record(&record)?;
    let client_secret = match &record.client_secret {
        Some(r) => {
            let builtin = secret_inputs::builtin(state)?;
            let value = builtin.read(r).await.map_err(|e| {
                ApiError::internal(format!(
                    "the client secret of identity provider `{id}` cannot be read: {e}"
                ))
            })?;
            Some(Zeroizing::new(value.expose().to_owned()))
        }
        None => None,
    };
    let built = OidcLogin::with_secret(&settings.login_config(id), client_secret)
        .map_err(ApiError::internal)?;
    let built = Arc::new(built);
    state.api_logins.put(id, version, built.clone());
    Ok(built)
}

/// Whether `r` is the client secret provider `id` holds now, which a
/// replacement may keep by naming it.
pub fn is_current_secret(current: Option<&SecretRef>, id: &str, r: &SecretRef) -> bool {
    r.store == BUILTIN_STORE && r.reference == secret_name(id) && current.is_some_and(|c| c == r)
}
