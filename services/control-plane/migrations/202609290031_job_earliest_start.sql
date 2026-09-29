-- Null preserves immediate eligibility for existing jobs and older clients.
ALTER TABLE jobs ADD COLUMN earliest_start_at timestamptz;
