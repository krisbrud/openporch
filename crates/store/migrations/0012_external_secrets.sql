-- Secrets openporch created in external stores. Only these belong to the
-- object whose name they carry: a secret someone else put at such a name is
-- neither written over nor usable without a grant.
CREATE TABLE external_secrets (
    org_id TEXT NOT NULL,
    store_id TEXT NOT NULL,
    ref TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, store_id, ref)
);
