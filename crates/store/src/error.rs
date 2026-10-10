/// Store failures. Messages name objects and IDs, never secret values.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Another engine owns the installation.
    #[error("the installation at {path} is owned by another openporch process")]
    AlreadyOwned {
        /// The lock file.
        path: String,
    },
    /// The database was written by a newer, unsupported schema.
    #[error(
        "database schema version {found} is newer than this binary supports (up to {supported}); \
         run a compatible openporch version or restore a compatible backup"
    )]
    SchemaTooNew {
        /// The newest applied migration.
        found: i64,
        /// The newest migration this binary knows.
        supported: i64,
    },
    /// An object does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// An object already exists.
    #[error("{0} already exists")]
    AlreadyExists(String),
    /// The environment generation moved on since the client read it.
    #[error("environment generation is {actual}, not the expected {expected}")]
    StaleGeneration {
        /// The generation in the request.
        expected: i64,
        /// The current generation.
        actual: i64,
    },
    /// Another deployment or deletion is running in the environment.
    #[error("deployment {0} is already running in this environment")]
    Busy(String),
    /// Resources with uncertain outcomes must be resolved first.
    #[error("resources need recovery before new changes: {0}")]
    RecoveryRequired(String),
    /// The idempotency key was used before with different content.
    #[error("idempotency key `{0}` was already used for a different request")]
    IdempotencyMismatch(String),
    /// A conflict with the object's current state.
    #[error("{0}")]
    Conflict(String),
    /// A stored record could not be decoded.
    #[error("corrupt record: {0}")]
    Corrupt(String),
    /// A database error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A migration error.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A filesystem error.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The error.
        source: std::io::Error,
    },
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Corrupt(e.to_string())
    }
}
