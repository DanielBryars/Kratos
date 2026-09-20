CREATE TABLE job_output_requirements (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    logical_path text NOT NULL,
    role text NOT NULL CHECK (role ~ '^[a-z][a-z0-9_-]{0,31}$'),
    media_type text NOT NULL CHECK (
        length(media_type) BETWEEN 3 AND 127
        AND media_type ~ '^[A-Za-z0-9!#$&^_.+-]+/[A-Za-z0-9!#$&^_.+-]+$'
    ),
    mandatory boolean NOT NULL,
    max_bytes bigint NOT NULL CHECK (max_bytes BETWEEN 1 AND 5368709120),
    UNIQUE (job_id, logical_path),
    UNIQUE (id, job_id),
    CHECK (
        octet_length(logical_path) BETWEEN 1 AND 240
        AND logical_path !~ '(^/|\\|//|(^|/)\.\.?(/|$)|[[:cntrl:]])'
    )
);

ALTER TABLE job_attempts ADD CONSTRAINT job_attempts_id_job_unique UNIQUE (id, job_id);

CREATE TABLE job_artifact_manifests (
    id uuid PRIMARY KEY,
    attempt_id uuid NOT NULL UNIQUE,
    job_id uuid NOT NULL,
    declared_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (id, attempt_id, job_id),
    FOREIGN KEY (attempt_id, job_id) REFERENCES job_attempts(id, job_id) ON DELETE CASCADE
);

CREATE TABLE job_artifacts (
    id uuid PRIMARY KEY,
    manifest_id uuid NOT NULL,
    attempt_id uuid NOT NULL,
    job_id uuid NOT NULL,
    output_requirement_id uuid NOT NULL,
    logical_path text NOT NULL,
    role text NOT NULL,
    media_type text NOT NULL,
    mandatory boolean NOT NULL,
    byte_length bigint NOT NULL CHECK (byte_length BETWEEN 0 AND 5368709120),
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    crc32c text NOT NULL CHECK (crc32c ~ '^[A-Za-z0-9+/]{6}==$'),
    object_key text NOT NULL UNIQUE,
    status text NOT NULL DEFAULT 'declared'
        CHECK (status IN ('declared', 'uploading', 'verified', 'rejected', 'deleted')),
    storage_generation bigint CHECK (storage_generation > 0),
    uploaded_byte_length bigint CHECK (uploaded_byte_length >= 0),
    uploaded_crc32c text CHECK (uploaded_crc32c ~ '^[A-Za-z0-9+/]{6}==$'),
    verified_storage_generation bigint CHECK (verified_storage_generation > 0),
    verified_byte_length bigint CHECK (verified_byte_length >= 0),
    verified_crc32c text CHECK (verified_crc32c ~ '^[A-Za-z0-9+/]{6}==$'),
    verification_source text CHECK (verification_source IN ('gcs_metadata')),
    declared_at timestamptz NOT NULL DEFAULT now(),
    upload_started_at timestamptz,
    upload_completed_at timestamptz,
    verified_at timestamptz,
    rejected_at timestamptz,
    deleted_at timestamptz,
    state_reason text CHECK (state_reason IS NULL OR length(state_reason) BETWEEN 1 AND 1000),
    UNIQUE (attempt_id, logical_path),
    FOREIGN KEY (manifest_id, attempt_id, job_id)
        REFERENCES job_artifact_manifests(id, attempt_id, job_id) ON DELETE CASCADE,
    FOREIGN KEY (attempt_id, job_id) REFERENCES job_attempts(id, job_id) ON DELETE CASCADE,
    FOREIGN KEY (output_requirement_id, job_id) REFERENCES job_output_requirements(id, job_id),
    CHECK (
        (upload_completed_at IS NULL AND storage_generation IS NULL
            AND uploaded_byte_length IS NULL AND uploaded_crc32c IS NULL)
        OR
        (upload_completed_at IS NOT NULL AND storage_generation IS NOT NULL
            AND uploaded_byte_length IS NOT NULL AND uploaded_crc32c IS NOT NULL)
    ),
    CHECK (
        (verified_at IS NULL
            AND verified_storage_generation IS NULL AND verified_byte_length IS NULL
            AND verified_crc32c IS NULL AND verification_source IS NULL)
        OR
        (upload_completed_at IS NOT NULL AND verified_at IS NOT NULL
            AND verified_storage_generation IS NOT NULL AND verified_byte_length IS NOT NULL
            AND verified_crc32c IS NOT NULL AND verification_source = 'gcs_metadata'
            AND verified_storage_generation = storage_generation
            AND verified_byte_length = byte_length
            AND verified_byte_length = uploaded_byte_length
            AND verified_crc32c = crc32c
            AND verified_crc32c = uploaded_crc32c)
    ),
    CHECK (status <> 'verified' OR verified_at IS NOT NULL),
    CHECK ((status <> 'rejected') OR rejected_at IS NOT NULL),
    CHECK ((status <> 'deleted') OR deleted_at IS NOT NULL)
);

CREATE INDEX job_artifacts_attempt_idx ON job_artifacts (attempt_id, status);
CREATE INDEX job_artifacts_pending_verification_idx
    ON job_artifacts (upload_completed_at)
    WHERE status = 'uploading' AND upload_completed_at IS NOT NULL;

COMMENT ON TABLE job_artifact_manifests IS
    'One immutable, worker-declared output manifest per execution attempt.';
COMMENT ON COLUMN job_artifacts.object_key IS
    'Opaque deterministic-scope object key; the workload logical path is metadata only.';
COMMENT ON COLUMN job_artifacts.upload_completed_at IS
    'The worker reported a completed GCS upload. Status remains uploading until storage metadata is independently verified.';
