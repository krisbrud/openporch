-- The audit entry that recorded a permit's issue, which the entries for its
-- binding and use correlate with.
ALTER TABLE permits ADD COLUMN issued_entry TEXT;
