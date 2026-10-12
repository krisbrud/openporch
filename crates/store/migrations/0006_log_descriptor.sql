-- Log lines name the resource they are about, as well as its GUResID, which a
-- resource that failed before its first operation does not have yet.
ALTER TABLE logs ADD COLUMN descriptor TEXT;
