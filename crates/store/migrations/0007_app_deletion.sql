-- An application being deleted takes no new environments and stays until the
-- deletions of all its environments have succeeded.
ALTER TABLE apps ADD COLUMN state TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'deleting'));
