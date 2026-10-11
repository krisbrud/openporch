-- Cloud account secrets are kept as references, separate from plain configuration.
ALTER TABLE cloud_accounts ADD COLUMN secret_refs TEXT NOT NULL DEFAULT '{}';
