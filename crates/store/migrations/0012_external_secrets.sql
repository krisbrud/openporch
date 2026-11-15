-- Secrets openporch creates in external stores. Only confirmed ones, in the
-- destination they were created in, belong to the object whose name they
-- carry: a secret someone else put at such a name is neither written over nor
-- usable without a grant. A pending row is written before the create call, so
-- a create whose answer was lost can be recognized when it is retried.
CREATE TABLE external_secrets (
    org_id TEXT NOT NULL,
    store_id TEXT NOT NULL,
    ref TEXT NOT NULL,
    -- `<cloud account>/<region>` of the store when the secret was created.
    destination TEXT NOT NULL,
    confirmed INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, store_id, ref)
);
