-- The definition and driver an operation ran with, so an active resource
-- describes its last successful operation rather than its latest attempt.
ALTER TABLE operations ADD COLUMN definition_id TEXT;
ALTER TABLE operations ADD COLUMN definition_version INTEGER;
ALTER TABLE operations ADD COLUMN driver_type TEXT;

-- The cloud account a resource was provisioned in.
ALTER TABLE active_resources ADD COLUMN driver_account TEXT;

CREATE INDEX active_resources_definition ON active_resources (org_id, definition_id);
