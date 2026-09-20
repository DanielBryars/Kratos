ALTER TABLE job_attempts
    ADD COLUMN artifact_delivery_expires_at timestamptz;

DROP INDEX job_attempts_expired_active_idx;
CREATE INDEX job_attempts_expired_active_idx
    ON job_attempts ((COALESCE(artifact_delivery_expires_at, lease_expires_at)))
    WHERE status IN ('assigned', 'running');

COMMENT ON COLUMN job_attempts.artifact_delivery_expires_at IS
    'Fixed deadline for durable output transfer after a manifest is accepted; this does not extend workload execution authority.';
