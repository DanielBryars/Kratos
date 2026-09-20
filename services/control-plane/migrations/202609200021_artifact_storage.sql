ALTER TABLE job_artifacts
    ADD COLUMN storage_bucket text,
    ADD COLUMN verified_sha256 text CHECK (verified_sha256 ~ '^[0-9a-f]{64}$'),
    ADD COLUMN protection_pending boolean NOT NULL DEFAULT false;

-- The preceding release allowed tests or operators to record GCS evidence without the bucket or
-- object SHA metadata. Those rows cannot satisfy the stronger storage-identity proof. Fail closed
-- instead of manufacturing evidence during the upgrade; a later attempt may upload the output
-- again under the new contract.
UPDATE job_artifacts
SET status = 'rejected',
    verified_storage_generation = NULL,
    verified_byte_length = NULL,
    verified_crc32c = NULL,
    verification_source = NULL,
    verified_at = NULL,
    rejected_at = COALESCE(rejected_at, now()),
    state_reason = 'storage_identity_requires_reissue'
WHERE status = 'verified';

ALTER TABLE job_artifacts ADD CONSTRAINT job_artifacts_verified_storage_identity
    CHECK (
        status <> 'verified'
        OR (
            storage_bucket IS NOT NULL
            AND verified_sha256 = sha256
            AND protection_pending = false
        )
    );

ALTER TABLE job_artifacts ADD CONSTRAINT job_artifacts_protection_pending_evidence
    CHECK (
        NOT protection_pending
        OR (
            status = 'uploading'
            AND verified_at IS NOT NULL
            AND verified_storage_generation IS NOT NULL
            AND verified_byte_length IS NOT NULL
            AND verified_crc32c IS NOT NULL
            AND verified_sha256 = sha256
            AND verification_source = 'gcs_metadata'
        )
    );

CREATE INDEX job_artifacts_protection_pending_idx
    ON job_artifacts (verified_at)
    WHERE protection_pending = true;

CREATE TABLE artifact_upload_grants (
    id uuid PRIMARY KEY,
    artifact_id uuid NOT NULL REFERENCES job_artifacts(id) ON DELETE CASCADE,
    worker_id uuid NOT NULL REFERENCES workers(id),
    bucket_name text NOT NULL,
    object_key text NOT NULL,
    session_uri text NOT NULL,
    issued_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    UNIQUE (artifact_id),
    CHECK (expires_at > issued_at)
);

COMMENT ON TABLE artifact_upload_grants IS
    'One control-plane-created resumable session per artifact. The bearer session URI is sensitive and returned only to the owning worker.';
COMMENT ON COLUMN job_artifacts.protection_pending IS
    'Authoritative metadata matched, but the exact generation still needs its lifecycle-protection hold before publication.';
COMMENT ON COLUMN job_artifacts.storage_bucket IS
    'Exact private bucket selected by the control plane when upload authority is issued.';
COMMENT ON COLUMN job_artifacts.verified_sha256 IS
    'SHA-256 read from authoritative GCS custom metadata during finalization.';
