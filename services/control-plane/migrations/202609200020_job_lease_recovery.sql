ALTER TABLE jobs DROP CONSTRAINT jobs_status_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_status_check
    CHECK (status IN (
        'queued', 'assigned', 'running', 'cancelling',
        'succeeded', 'failed', 'cancelled'
    ));

ALTER TABLE jobs
    ADD COLUMN max_attempts integer NOT NULL DEFAULT 2 CHECK (max_attempts BETWEEN 1 AND 2),
    ADD COLUMN cancel_requested_at timestamptz;

UPDATE jobs
SET cancel_requested_at = COALESCE(finished_at, submitted_at)
WHERE status = 'cancelled';

ALTER TABLE jobs ADD CONSTRAINT jobs_cancellation_requested_check CHECK (
    status NOT IN ('cancelling', 'cancelled') OR cancel_requested_at IS NOT NULL
);

ALTER TABLE job_attempts DROP CONSTRAINT job_attempts_status_check;
ALTER TABLE job_attempts ADD CONSTRAINT job_attempts_status_check
    CHECK (status IN ('assigned', 'running', 'succeeded', 'failed', 'cancelled'));
ALTER TABLE job_attempts
    ADD COLUMN terminal_reason text
        CHECK (terminal_reason IS NULL OR length(terminal_reason) BETWEEN 1 AND 1000);

CREATE INDEX job_attempts_expired_active_idx
    ON job_attempts (lease_expires_at)
    WHERE status IN ('assigned', 'running');

COMMENT ON COLUMN jobs.max_attempts IS
    'Total execution-attempt ceiling. R0.2 permits one automatic retry after lease expiry.';
COMMENT ON COLUMN jobs.cancel_requested_at IS
    'When cancellation was requested; active work remains cancelling until result or lease expiry.';
COMMENT ON COLUMN job_attempts.terminal_reason IS
    'Control-plane reason for terminalisation when no authoritative worker result was accepted.';
