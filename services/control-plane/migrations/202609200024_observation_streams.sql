CREATE TABLE observation_streams (
    id uuid PRIMARY KEY,
    attempt_id uuid NOT NULL UNIQUE REFERENCES job_attempts(id) ON DELETE CASCADE,
    job_id uuid NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    worker_id uuid NOT NULL REFERENCES workers(id),
    accepted_through_sequence bigint NOT NULL DEFAULT 0
        CHECK (accepted_through_sequence >= 0),
    mlflow_run_id text,
    mlflow_created_at timestamptz,
    mlflow_last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE observation_batches (
    stream_id uuid NOT NULL REFERENCES observation_streams(id) ON DELETE CASCADE,
    batch_id uuid NOT NULL,
    first_sequence bigint NOT NULL CHECK (first_sequence > 0),
    last_sequence bigint NOT NULL CHECK (last_sequence >= first_sequence),
    content_sha256 text NOT NULL CHECK (content_sha256 ~ '^[0-9a-f]{64}$'),
    received_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (stream_id, batch_id)
);

CREATE TABLE observations (
    stream_id uuid NOT NULL REFERENCES observation_streams(id) ON DELETE CASCADE,
    sequence bigint NOT NULL CHECK (sequence > 0),
    first_batch_id uuid NOT NULL,
    observed_at timestamptz NOT NULL,
    record_type text NOT NULL CHECK (record_type IN ('param', 'metric', 'progress')),
    content jsonb NOT NULL,
    content_sha256 text NOT NULL CHECK (content_sha256 ~ '^[0-9a-f]{64}$'),
    mlflow_applied_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (stream_id, sequence)
);

CREATE INDEX observations_pending_mlflow_idx
    ON observations (stream_id, sequence)
    WHERE mlflow_applied_at IS NULL;

ALTER TABLE job_attempts
    ADD COLUMN observation_counters jsonb;

COMMENT ON TABLE observation_streams IS
    'Durable attempt-local correlation and acknowledgement state; remains writable after terminal result.';
COMMENT ON COLUMN observations.content IS
    'Canonical validated protocol 1.2 record used as the durable MLflow delivery outbox.';
COMMENT ON COLUMN job_attempts.observation_counters IS
    'Agent snapshot of bounded observation loss, non-export and retry counters at result submission.';
