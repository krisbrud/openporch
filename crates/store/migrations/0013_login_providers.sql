-- OpenID Connect providers people sign in with, managed through the API beside
-- the ones in the server's configuration file. `config` holds the provider's
-- settings without its client secret, which is a built-in store reference.
CREATE TABLE login_providers (
    org_id TEXT NOT NULL,
    id TEXT NOT NULL,
    config TEXT NOT NULL,
    client_secret TEXT,
    generation INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    PRIMARY KEY (org_id, id)
);
