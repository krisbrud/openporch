//! Route handlers and the route table.

mod apps;
mod audit;
mod catalog;
mod envs;
mod health;
mod login;
mod members;

#[cfg(test)]
pub(crate) use members::INVITATION_TTL_MS;

use axum::http::Method;
use openporch_core::ids;

use crate::api::Api;
use crate::error::ApiError;
use crate::rbac::{AppPerm, OrgPerm, Permission};
use crate::state::AppState;

/// Every route of the API, each with the permission it requires.
pub fn api(state: AppState) -> Api {
    use Permission::{App, Authenticated, Org, Public};
    let get = || Method::GET;
    let post = || Method::POST;
    let patch = || Method::PATCH;
    let put = || Method::PUT;
    let delete = || Method::DELETE;
    Api::new(state)
        .route(get(), "/livez", Public, health::livez)
        .route(get(), "/readyz", Public, health::readyz)
        .route(get(), "/healthz", Public, health::readyz)
        .route(get(), "/version", Public, health::version)
        // Login.
        .route(post(), "/auth/github/device", Public, login::start)
        .route(post(), "/auth/github/token", Public, login::poll)
        // Members.
        .route(
            get(),
            "/orgs/{org}/users",
            Org(OrgPerm::ReadMembers),
            members::list,
        )
        .route(
            post(),
            "/orgs/{org}/users",
            Org(OrgPerm::ManageMembers),
            members::create,
        )
        .route(
            get(),
            "/orgs/{org}/users/{user}",
            Org(OrgPerm::ReadMembers),
            members::get,
        )
        .route(
            patch(),
            "/orgs/{org}/users/{user}",
            Org(OrgPerm::ManageMembers),
            members::update,
        )
        .route(
            delete(),
            "/orgs/{org}/users/{user}",
            Org(OrgPerm::ManageMembers),
            members::remove,
        )
        .route(
            get(),
            "/orgs/{org}/current-user",
            Authenticated,
            members::current,
        )
        .route(
            delete(),
            "/orgs/{org}/sessions/current",
            Authenticated,
            members::logout,
        )
        // Tokens: the handlers check self-service and the delegation ceiling.
        .route(
            get(),
            "/orgs/{org}/users/{user}/tokens",
            Authenticated,
            members::tokens,
        )
        .route(
            post(),
            "/orgs/{org}/users/{user}/tokens",
            Authenticated,
            members::create_token,
        )
        .route(
            get(),
            "/orgs/{org}/users/{user}/tokens/{token}",
            Authenticated,
            members::token,
        )
        .route(
            delete(),
            "/orgs/{org}/users/{user}/tokens/{token}",
            Authenticated,
            members::revoke_token,
        )
        // Invitations.
        .route(
            get(),
            "/orgs/{org}/invitations",
            Org(OrgPerm::ManageMembers),
            members::invitations,
        )
        .route(
            post(),
            "/orgs/{org}/invitations",
            Org(OrgPerm::ManageMembers),
            members::invite,
        )
        // Applications.
        .route(
            get(),
            "/orgs/{org}/apps",
            Org(OrgPerm::ReadMembers),
            apps::list,
        )
        .route(
            post(),
            "/orgs/{org}/apps",
            Org(OrgPerm::CreateApp),
            apps::create,
        )
        .route(
            get(),
            "/orgs/{org}/apps/{app}",
            App(AppPerm::Read),
            apps::get,
        )
        .route(
            delete(),
            "/orgs/{org}/apps/{app}",
            App(AppPerm::Manage),
            apps::delete,
        )
        .route(
            get(),
            "/orgs/{org}/apps/{app}/users",
            App(AppPerm::Read),
            apps::users,
        )
        .route(
            post(),
            "/orgs/{org}/apps/{app}/users",
            App(AppPerm::Manage),
            apps::add_user,
        )
        .route(
            patch(),
            "/orgs/{org}/apps/{app}/users/{user}",
            App(AppPerm::Manage),
            apps::update_user,
        )
        .route(
            delete(),
            "/orgs/{org}/apps/{app}/users/{user}",
            App(AppPerm::Manage),
            apps::remove_user,
        )
        // Environments.
        .route(
            get(),
            "/orgs/{org}/apps/{app}/envs",
            App(AppPerm::Read),
            envs::list,
        )
        .route(
            post(),
            "/orgs/{org}/apps/{app}/envs",
            App(AppPerm::Configure),
            envs::create,
        )
        .route(
            get(),
            "/orgs/{org}/apps/{app}/envs/{env}",
            App(AppPerm::Read),
            envs::get,
        )
        .route(
            delete(),
            "/orgs/{org}/apps/{app}/envs/{env}",
            App(AppPerm::Deploy),
            envs::delete,
        )
        // Environment types.
        .route(
            get(),
            "/orgs/{org}/env-types",
            Org(OrgPerm::ReadMembers),
            apps::env_types,
        )
        .route(
            post(),
            "/orgs/{org}/env-types",
            Org(OrgPerm::Administer),
            apps::create_env_type,
        )
        .route(
            get(),
            "/orgs/{org}/env-types/{type}",
            Org(OrgPerm::ReadMembers),
            apps::env_type,
        )
        .route(
            delete(),
            "/orgs/{org}/env-types/{type}",
            Org(OrgPerm::Administer),
            apps::delete_env_type,
        )
        .route(
            get(),
            "/orgs/{org}/env-types/{type}/users",
            Org(OrgPerm::ReadMembers),
            apps::env_type_users,
        )
        .route(
            post(),
            "/orgs/{org}/env-types/{type}/users",
            Org(OrgPerm::Administer),
            apps::add_env_type_user,
        )
        .route(
            delete(),
            "/orgs/{org}/env-types/{type}/users/{user}",
            Org(OrgPerm::Administer),
            apps::remove_env_type_user,
        )
        // Resource catalog: members read, administrators change.
        .route(
            get(),
            "/orgs/{org}/resources/types",
            Org(OrgPerm::ReadMembers),
            catalog::types,
        )
        .route(
            post(),
            "/orgs/{org}/resources/types",
            Org(OrgPerm::Administer),
            catalog::create_type,
        )
        .route(
            get(),
            "/orgs/{org}/resources/types/{type}",
            Org(OrgPerm::ReadMembers),
            catalog::get_type,
        )
        .route(
            put(),
            "/orgs/{org}/resources/types/{type}",
            Org(OrgPerm::Administer),
            catalog::update_type,
        )
        .route(
            delete(),
            "/orgs/{org}/resources/types/{type}",
            Org(OrgPerm::Administer),
            catalog::delete_type,
        )
        .route(
            get(),
            "/orgs/{org}/resources/types/{type}/classes",
            Org(OrgPerm::ReadMembers),
            catalog::classes,
        )
        .route(
            post(),
            "/orgs/{org}/resources/types/{type}/classes",
            Org(OrgPerm::Administer),
            catalog::create_class,
        )
        .route(
            delete(),
            "/orgs/{org}/resources/types/{type}/classes/{class}",
            Org(OrgPerm::Administer),
            catalog::delete_class,
        )
        .route(
            get(),
            "/orgs/{org}/resources/classes",
            Org(OrgPerm::ReadMembers),
            catalog::all_classes,
        )
        .route(
            get(),
            "/orgs/{org}/resources/drivers",
            Org(OrgPerm::ReadMembers),
            catalog::drivers,
        )
        .route(
            post(),
            "/orgs/{org}/resources/drivers",
            Org(OrgPerm::Administer),
            catalog::create_driver,
        )
        .route(
            get(),
            "/orgs/{org}/resources/drivers/{driver}",
            Org(OrgPerm::ReadMembers),
            catalog::get_driver,
        )
        .route(
            delete(),
            "/orgs/{org}/resources/drivers/{driver}",
            Org(OrgPerm::Administer),
            catalog::delete_driver,
        )
        .route(
            get(),
            "/orgs/{org}/resources/defs",
            Org(OrgPerm::ReadMembers),
            catalog::definitions,
        )
        .route(
            post(),
            "/orgs/{org}/resources/defs",
            Org(OrgPerm::Administer),
            catalog::create_definition,
        )
        .route(
            get(),
            "/orgs/{org}/resources/defs/{def}",
            Org(OrgPerm::ReadMembers),
            catalog::get_definition,
        )
        .route(
            put(),
            "/orgs/{org}/resources/defs/{def}",
            Org(OrgPerm::Administer),
            catalog::update_definition,
        )
        .route(
            delete(),
            "/orgs/{org}/resources/defs/{def}",
            Org(OrgPerm::Administer),
            catalog::delete_definition,
        )
        .route(
            post(),
            "/orgs/{org}/resources/defs/{def}/criteria",
            Org(OrgPerm::Administer),
            catalog::add_criterion,
        )
        .route(
            delete(),
            "/orgs/{org}/resources/defs/{def}/criteria/{criterion}",
            Org(OrgPerm::Administer),
            catalog::delete_criterion,
        )
        // Audit log.
        .route(
            get(),
            "/orgs/{org}/audit-logs",
            Org(OrgPerm::ReadAll),
            audit::query,
        )
}

/// Checks an object ID against the ID rule.
fn check_id(what: &str, id: &str) -> Result<(), ApiError> {
    if ids::is_valid_object_id(id) {
        Ok(())
    } else {
        Err(ApiError::invalid_id(format!(
            "{what} ID `{id}` is invalid: {}",
            ids::OBJECT_ID_RULE
        )))
    }
}
