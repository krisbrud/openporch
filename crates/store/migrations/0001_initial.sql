-- Initial openporch schema. Timestamps are Unix milliseconds. JSON columns hold
-- serialized core types; secret values never appear outside `secret_versions`,
-- where they are AES-256-GCM ciphertext.

-- One row describing the installation and its current owner.
CREATE TABLE installation (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    org_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    owner_id TEXT,
    owner_pid INTEGER,
    owner_acquired_at INTEGER
);

-- Identity --------------------------------------------------------------------

CREATE TABLE principals (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    type TEXT NOT NULL CHECK (type IN ('user', 'service')),
    name TEXT NOT NULL,
    email TEXT,
    org_role TEXT NOT NULL CHECK (org_role IN ('administrator', 'manager', 'member', 'orgViewer')),
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);

-- External identities such as `github:<numeric id>`. One per user.
CREATE TABLE identities (
    org_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    login TEXT,
    email TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, provider, subject),
    UNIQUE (org_id, principal_id),
    FOREIGN KEY (org_id, principal_id) REFERENCES principals (org_id, id) ON DELETE CASCADE
);

CREATE TABLE invitations (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    org_role TEXT NOT NULL,
    email TEXT,
    provider TEXT,
    subject TEXT,
    login TEXT,
    code_hash TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    consumed_at INTEGER,
    consumed_by TEXT,
    PRIMARY KEY (org_id, id)
);

-- API tokens are stored only as SHA-256 hashes.
CREATE TABLE api_tokens (
    org_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    id TEXT NOT NULL,
    description TEXT,
    token_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    last_used_at INTEGER,
    revoked_at INTEGER,
    PRIMARY KEY (org_id, principal_id, id),
    FOREIGN KEY (org_id, principal_id) REFERENCES principals (org_id, id) ON DELETE CASCADE
);

-- Issued sessions, so that logout and removal revoke them by `jti`.
CREATE TABLE sessions (
    jti TEXT PRIMARY KEY,
    org_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    issued_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    FOREIGN KEY (org_id, principal_id) REFERENCES principals (org_id, id) ON DELETE CASCADE
);

-- Session and future OIDC signing keys, encrypted under the master key.
CREATE TABLE signing_keys (
    id TEXT PRIMARY KEY,
    purpose TEXT NOT NULL,
    algorithm TEXT NOT NULL,
    key_id TEXT NOT NULL,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    retired_at INTEGER
);

-- Audit -----------------------------------------------------------------------

CREATE TABLE audit_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    at INTEGER NOT NULL,
    org_id TEXT NOT NULL,
    actor_id TEXT,
    request_id TEXT,
    operation_id TEXT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN ('accepted', 'succeeded', 'failed', 'denied')),
    status_code INTEGER,
    reason TEXT,
    correlates TEXT
);
CREATE INDEX audit_log_at ON audit_log (org_id, at);

-- Secrets ---------------------------------------------------------------------

CREATE TABLE secret_stores (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    type TEXT NOT NULL,
    config TEXT NOT NULL DEFAULT '{}',
    is_primary INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);
CREATE UNIQUE INDEX secret_stores_primary ON secret_stores (org_id) WHERE is_primary = 1;

-- Versioned entries of the built-in store.
CREATE TABLE secret_versions (
    org_id TEXT NOT NULL,
    ref TEXT NOT NULL,
    version INTEGER NOT NULL,
    key_id TEXT NOT NULL,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, ref, version)
);

CREATE TABLE secret_grants (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    store_id TEXT NOT NULL,
    ref TEXT NOT NULL,
    app_id TEXT,
    env_id TEXT,
    org_wide INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    PRIMARY KEY (org_id, id),
    CHECK (org_wide = 1 OR app_id IS NOT NULL)
);

-- Applications and environments -----------------------------------------------

CREATE TABLE env_types (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);

CREATE TABLE apps (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);

CREATE TABLE envs (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    generation INTEGER NOT NULL DEFAULT 0,
    desired_set_id TEXT,
    last_successful_set_id TEXT,
    last_successful_deploy_id TEXT,
    state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'deleting', 'deletion_failed')),
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, app_id, id),
    FOREIGN KEY (org_id, app_id) REFERENCES apps (org_id, id),
    FOREIGN KEY (org_id, type) REFERENCES env_types (org_id, id)
);

CREATE TABLE app_roles (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('owner', 'developer', 'viewer')),
    PRIMARY KEY (org_id, app_id, principal_id),
    FOREIGN KEY (org_id, app_id) REFERENCES apps (org_id, id) ON DELETE CASCADE,
    FOREIGN KEY (org_id, principal_id) REFERENCES principals (org_id, id) ON DELETE CASCADE
);

CREATE TABLE env_type_roles (
    org_id TEXT NOT NULL,
    env_type_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('deployer')),
    PRIMARY KEY (org_id, env_type_id, principal_id),
    FOREIGN KEY (org_id, env_type_id) REFERENCES env_types (org_id, id) ON DELETE CASCADE,
    FOREIGN KEY (org_id, principal_id) REFERENCES principals (org_id, id) ON DELETE CASCADE
);

-- Shared values. `env_id` is empty for application-level values.
CREATE TABLE app_values (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL DEFAULT '',
    key TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    value TEXT,
    secret_ref TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, app_id, env_id, key),
    CHECK ((value IS NULL) <> (secret_ref IS NULL))
);

CREATE TABLE value_set_versions (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL DEFAULT '',
    id TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    UNIQUE (org_id, app_id, env_id, id)
);

-- Resource catalog ------------------------------------------------------------

-- Custom resource types; built-in types live in code.
CREATE TABLE resource_types (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);

CREATE TABLE resource_classes (
    org_id TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    id TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, resource_type, id)
);

CREATE TABLE drivers (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);

CREATE TABLE cloud_accounts (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    config TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);

-- Definitions keep secret inputs only as references.
CREATE TABLE definitions (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    type TEXT NOT NULL,
    driver_type TEXT NOT NULL,
    driver_account TEXT,
    driver_inputs TEXT NOT NULL DEFAULT '{}',
    provision TEXT NOT NULL DEFAULT '{}',
    version INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, id)
);

CREATE TABLE criteria (
    org_id TEXT NOT NULL,
    definition_id TEXT NOT NULL,
    id TEXT NOT NULL,
    env_type TEXT,
    app_id TEXT,
    env_id TEXT,
    res_id TEXT,
    class TEXT,
    PRIMARY KEY (org_id, definition_id, id),
    FOREIGN KEY (org_id, definition_id) REFERENCES definitions (org_id, id) ON DELETE CASCADE
);

-- Sets, deltas and deployments ------------------------------------------------

CREATE TABLE sets (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    id TEXT NOT NULL,
    algorithm TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, app_id, id)
);

CREATE TABLE deltas (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    id TEXT NOT NULL,
    env_id TEXT,
    content TEXT NOT NULL,
    base_generation INTEGER,
    frozen_at INTEGER,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, app_id, id)
);

-- A deployment is both the job and its record. `seq` orders attempts in an environment.
CREATE TABLE deployments (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL DEFAULT 'deploy' CHECK (kind IN ('deploy', 'delete_env')),
    actor_id TEXT NOT NULL,
    set_id TEXT,
    delta_id TEXT,
    comment TEXT NOT NULL DEFAULT '',
    generation INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('in_progress', 'succeeded', 'failed')),
    idempotency_key TEXT,
    request_fingerprint TEXT,
    snapshot TEXT NOT NULL DEFAULT '{}',
    error TEXT,
    created_at INTEGER NOT NULL,
    finished_at INTEGER,
    PRIMARY KEY (org_id, app_id, env_id, id),
    UNIQUE (org_id, app_id, env_id, seq),
    UNIQUE (org_id, app_id, env_id, actor_id, idempotency_key),
    FOREIGN KEY (org_id, app_id, env_id) REFERENCES envs (org_id, app_id, id)
);
-- At most one running deployment or deletion per environment.
CREATE UNIQUE INDEX deployments_one_running ON deployments (org_id, app_id, env_id)
    WHERE status = 'in_progress';

CREATE TABLE deployment_resources (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    deployment_id TEXT NOT NULL,
    descriptor TEXT NOT NULL,
    guresid TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'provisioning', 'provisioned', 'failed', 'skipped', 'unknown')),
    attempts INTEGER NOT NULL DEFAULT 0,
    outputs TEXT,
    error TEXT,
    observed_at INTEGER,
    PRIMARY KEY (org_id, app_id, env_id, deployment_id, descriptor),
    FOREIGN KEY (org_id, app_id, env_id, deployment_id) REFERENCES deployments (org_id, app_id, env_id, id)
);

CREATE TABLE active_resources (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    descriptor TEXT NOT NULL,
    guresid TEXT NOT NULL UNIQUE,
    definition_id TEXT,
    definition_version INTEGER,
    driver_type TEXT,
    inputs TEXT NOT NULL DEFAULT '{}',
    outputs TEXT NOT NULL DEFAULT '{}',
    secret_outputs TEXT NOT NULL DEFAULT '{}',
    cookie_key_id TEXT,
    cookie_nonce BLOB,
    cookie BLOB,
    status TEXT NOT NULL CHECK (status IN ('pending', 'provisioning', 'provisioned', 'failed', 'unknown', 'recovery_required', 'deleting', 'deletion_failed')),
    recovery TEXT NOT NULL DEFAULT '{}',
    snapshot TEXT NOT NULL DEFAULT '{}',
    backend TEXT,
    external_ids TEXT NOT NULL DEFAULT '[]',
    ownership TEXT NOT NULL DEFAULT 'owned' CHECK (ownership IN ('owned', 'adopted', 'external')),
    dependencies TEXT NOT NULL DEFAULT '[]',
    last_deploy_id TEXT,
    observed_at INTEGER,
    scheduled_deletion INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, app_id, env_id, descriptor)
);

-- Driver operations, persisted before the driver is called.
CREATE TABLE operations (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    deployment_id TEXT NOT NULL,
    guresid TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('put', 'delete')),
    input_fingerprint TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('started', 'succeeded', 'failed', 'unknown')),
    attempt INTEGER NOT NULL DEFAULT 1,
    recovery TEXT NOT NULL DEFAULT '{}',
    error TEXT,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE INDEX operations_open ON operations (state) WHERE state IN ('started', 'unknown');

CREATE TABLE logs (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    deployment_id TEXT NOT NULL,
    operation_id TEXT,
    guresid TEXT,
    at INTEGER NOT NULL,
    level TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE INDEX logs_deployment ON logs (org_id, app_id, env_id, deployment_id, seq);

-- Records of resources removed from management.
CREATE TABLE tombstones (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    descriptor TEXT NOT NULL,
    guresid TEXT NOT NULL,
    reason TEXT NOT NULL,
    actor_id TEXT,
    body TEXT NOT NULL,
    at INTEGER NOT NULL
);
