-- Secrets openporch creates in external stores. Only confirmed ones belong to
-- the object whose name they carry: a secret someone else put at such a name
-- is neither written over nor usable without a grant. A pending row is written
-- before the create call, with the random `create_id` the create tags the
-- secret with, so a create whose answer was lost is recognized by its tag when
-- it is retried, and a secret someone else made is not.
CREATE TABLE external_secrets (
    org_id TEXT NOT NULL,
    store_id TEXT NOT NULL,
    ref TEXT NOT NULL,
    -- The AWS account, as STS reported it, and region the secret is in.
    aws_account TEXT NOT NULL,
    region TEXT NOT NULL,
    create_id TEXT NOT NULL,
    confirmed INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (org_id, store_id, ref)
);
