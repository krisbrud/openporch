-- Previews of deployments, and the destructive-action permits issued from them.
CREATE TABLE plans (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    base_generation INTEGER NOT NULL,
    set_id TEXT,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, id)
);

-- A permit authorizes one action on one resource, in the operation it is bound
-- to and that operation's retries.
CREATE TABLE permits (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    descriptor TEXT NOT NULL,
    guresid TEXT,
    action TEXT NOT NULL CHECK (action IN ('delete', 'detach', 'replace', 'unknown')),
    -- The driver plan the permit covers; any plan of the action when absent.
    plan_fingerprint TEXT,
    base_generation INTEGER NOT NULL,
    -- What the resource's configuration was when the plan was made.
    config_digest TEXT NOT NULL,
    issuer_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    -- The deployment it was bound to on acceptance, or `detach`.
    bound_to TEXT,
    bound_at INTEGER,
    used_at INTEGER,
    PRIMARY KEY (org_id, id)
);
CREATE INDEX permits_bound ON permits (org_id, app_id, env_id, bound_to);

-- Driver plans that needed a permit the deployment did not have, so that a new
-- preview can offer a permit for exactly that plan.
CREATE TABLE approval_requests (
    org_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    env_id TEXT NOT NULL,
    guresid TEXT NOT NULL,
    descriptor TEXT NOT NULL,
    action TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    actions TEXT NOT NULL,
    deployment_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, guresid, fingerprint)
);
