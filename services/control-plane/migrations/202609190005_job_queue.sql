CREATE TABLE jobs (
    id uuid PRIMARY KEY,
    owner_identity_id uuid NOT NULL REFERENCES human_identities(id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
    image_reference text NOT NULL
        CHECK (image_reference ~ '^[^[:space:]@]+@sha256:[0-9a-f]{64}$'),
    gpu_count integer NOT NULL DEFAULT 1 CHECK (gpu_count = 1),
    timeout_seconds integer NOT NULL CHECK (timeout_seconds BETWEEN 30 AND 3600),
    status text NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'assigned', 'running', 'succeeded', 'failed', 'cancelled')),
    assigned_worker_id uuid REFERENCES workers(id),
    submitted_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz,
    finished_at timestamptz,
    exit_code integer,
    stdout text,
    stderr text,
    failure_message text,
    CHECK ((status <> 'queued') OR assigned_worker_id IS NULL),
    CHECK ((status NOT IN ('succeeded', 'failed', 'cancelled')) OR finished_at IS NOT NULL)
);

CREATE TABLE job_attempts (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    attempt_number integer NOT NULL CHECK (attempt_number > 0),
    worker_id uuid NOT NULL REFERENCES workers(id),
    status text NOT NULL DEFAULT 'assigned'
        CHECK (status IN ('assigned', 'running', 'succeeded', 'failed')),
    assigned_at timestamptz NOT NULL DEFAULT now(),
    lease_expires_at timestamptz NOT NULL,
    started_at timestamptz,
    finished_at timestamptz,
    UNIQUE (job_id, attempt_number)
);

CREATE UNIQUE INDEX job_attempts_one_active_worker
    ON job_attempts (worker_id)
    WHERE status IN ('assigned', 'running');
CREATE INDEX jobs_owner_submitted_idx ON jobs (owner_identity_id, submitted_at DESC);
CREATE INDEX jobs_queue_idx ON jobs (submitted_at) WHERE status = 'queued';

COMMENT ON COLUMN jobs.image_reference IS
    'Operator-approved immutable OCI image reference; mutable tags are rejected.';
COMMENT ON COLUMN job_attempts.lease_expires_at IS
    'Hard execution authority deadline. The initial scheduler does not reassign expired work.';
