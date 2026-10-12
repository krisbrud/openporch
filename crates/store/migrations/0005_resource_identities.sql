-- A detached resource's driver cookie, still encrypted, so the state location
-- the driver kept in it survives for a later adoption, and the references to
-- its secret outputs. Neither is returned by the API.
ALTER TABLE tombstones ADD COLUMN cookie_key_id TEXT;
ALTER TABLE tombstones ADD COLUMN cookie_nonce BLOB;
ALTER TABLE tombstones ADD COLUMN cookie BLOB;
ALTER TABLE tombstones ADD COLUMN secret_outputs TEXT;
