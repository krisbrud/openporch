-- Environment types an administrator marked protected: destructive permits in
-- their environments need an administrator, and changes of unknown impact are
-- refused until a driver plans them.
ALTER TABLE env_types ADD COLUMN protected INTEGER NOT NULL DEFAULT 0;
