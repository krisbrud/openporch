-- A resource whose deletion the driver confirmed but whose record could not be
-- removed. The next attempt removes the record without calling the driver.
ALTER TABLE active_resources ADD COLUMN cleanup_pending INTEGER NOT NULL DEFAULT 0;
