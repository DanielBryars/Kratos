ALTER TABLE job_artifacts
    ADD COLUMN storage_bucket text,
    ADD COLUMN verified_sha256 text CHECK (verified_sha256 ~ '^[0-9a-f]{64}$');

ALTER TABLE job_artifacts ADD CONSTRAINT job_artifacts_verified_storage_identity
    CHECK (
        status <> 'verified'
        OR (
            storage_bucket IS NOT NULL
            AND verified_sha256 = sha256
        )
    );

CREATE TABLE artifact_upload_grants (
    id uuid PRIMARY KEY,
    artifact_id uuid NOT NULL REFERENCES job_artifacts(id) ON DELETE CASCADE,
    worker_id uuid NOT NULL REFERENCES workers(id),
    bucket_name text NOT NULL,
    object_key text NOT NULL,
    issued_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    CHECK (expires_at > issued_at)
);

CREATE INDEX artifact_upload_grants_artifact_idx
    ON artifact_upload_grants (artifact_id, issued_at DESC);

COMMENT ON TABLE artifact_upload_grants IS
    'Audit records for short-lived upload initiation grants. Signed URLs and resumable session URIs are never persisted.';
COMMENT ON COLUMN job_artifacts.storage_bucket IS
    'Exact private bucket selected by the control plane when upload authority is issued.';
COMMENT ON COLUMN job_artifacts.verified_sha256 IS
    'SHA-256 read from authoritative GCS custom metadata during finalization.';
