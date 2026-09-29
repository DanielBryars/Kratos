-- Server-side idempotency for job submission (TRN-011).
--
-- Both columns are null for every existing job and for any submission made without an
-- Idempotency-Key header, so older clients keep exactly the behaviour they had.
ALTER TABLE jobs
    ADD COLUMN submission_idempotency_key uuid,
    ADD COLUMN submission_fingerprint text
        CHECK (submission_fingerprint IS NULL OR submission_fingerprint ~ '^[0-9a-f]{64}$'),
    ADD CONSTRAINT jobs_submission_idempotency_pair
        CHECK ((submission_idempotency_key IS NULL) = (submission_fingerprint IS NULL));

-- The guarantee itself. Two simultaneous submissions with the same key cannot both insert: the
-- second waits on the first's uncommitted row and then conflicts with it. The key is scoped to the
-- project and to who submitted, so one person cannot collide with, or probe, another's keys.
CREATE UNIQUE INDEX jobs_submission_idempotency_key_idx
    ON jobs (project_id, owner_identity_id, submission_idempotency_key)
    WHERE submission_idempotency_key IS NOT NULL;

COMMENT ON COLUMN jobs.submission_idempotency_key IS
    'Client-supplied Idempotency-Key, unique per project and submitting identity; null when none was sent.';
COMMENT ON COLUMN jobs.submission_fingerprint IS
    'SHA-256 of the canonical job request the key was first used with; a replay must match it.';
