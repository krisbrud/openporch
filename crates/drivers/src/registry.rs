//! The drivers a server can call, by ID.

use std::collections::BTreeMap;
use std::sync::Arc;

use openporch_core::catalog::BUILTIN_DRIVER_NAMESPACE;
use openporch_core::ids;

use crate::{Driver, Echo, NoneDriver, Template};

/// Why a driver could not be registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("driver ID `{0}` is invalid: driver IDs are `<namespace>/<name>`")]
    InvalidId(String),
    #[error("driver ID `{0}` is in the reserved `openporch` namespace")]
    Reserved(String),
    #[error("driver `{0}` is already registered")]
    Duplicate(String),
}

/// Drivers by ID. Built-ins use the reserved `openporch` namespace.
#[derive(Debug, Clone, Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<String, Arc<dyn Driver>>,
}

impl DriverRegistry {
    /// A registry holding the built-in drivers.
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        for driver in [
            Arc::new(Echo::new()) as Arc<dyn Driver>,
            Arc::new(NoneDriver::new()),
            Arc::new(Template::new()),
        ] {
            registry.drivers.insert(driver.info().id.clone(), driver);
        }
        registry
    }

    /// Registers a non-built-in driver.
    pub fn register(&mut self, driver: Arc<dyn Driver>) -> Result<(), RegistryError> {
        let id = driver.info().id.clone();
        let Some((ns, name)) = id.split_once('/') else {
            return Err(RegistryError::InvalidId(id));
        };
        if !ids::is_valid_object_id(ns) || !ids::is_valid_object_id(name) {
            return Err(RegistryError::InvalidId(id));
        }
        if ns == BUILTIN_DRIVER_NAMESPACE {
            return Err(RegistryError::Reserved(id));
        }
        if self.drivers.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.drivers.insert(id, driver);
        Ok(())
    }

    /// The driver with `id`.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Driver>> {
        self.drivers.get(id).cloned()
    }

    /// The registered IDs, sorted.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.drivers.keys().map(String::as_str)
    }
}
