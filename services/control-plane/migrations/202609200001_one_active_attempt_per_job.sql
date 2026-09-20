CREATE UNIQUE INDEX job_attempts_one_active_job
    ON job_attempts (job_id)
    WHERE status IN ('assigned', 'running');

COMMENT ON INDEX job_attempts_one_active_job IS
    'Prevents replacement work from running until the previous attempt is durably terminal.';
