//! The openporch HTTP API server.

pub mod api;
pub mod auth;
pub mod config;
pub mod error;
mod handlers;
pub mod identity;
pub mod login;
pub mod rbac;
mod secret_inputs;
pub mod state;

#[cfg(test)]
mod tests;

use axum::Router;

pub use api::RouteSpec;
pub use config::Config;
pub use state::{AppState, StartupError, start, start_with};

/// The version reported by the `openporch` binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The router and the table of registered routes with their permissions.
pub fn router(state: AppState) -> (Router, Vec<RouteSpec>) {
    let api = handlers::api(state);
    let specs = api.specs().to_vec();
    (api.into_router(), specs)
}
