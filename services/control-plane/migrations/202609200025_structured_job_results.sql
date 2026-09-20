ALTER TABLE job_attempts
    ADD COLUMN structured_result jsonb;

COMMENT ON COLUMN job_attempts.structured_result IS
    'Opaque, bounded workload result object copied from the final protocol 1.2 result record.';
