-- Short-lived, version-scoped dataset preview sessions (ADR-018).
--
-- The dataset viewer runs in the browser and needs to read a version's files. It must not be
-- given a bucket credential, and it must not be given durable object locations: either would let
-- a viewer reach datasets its operator was never shown. A preview session is the narrow thing it
-- gets instead -- one version, a few minutes, and a token stored only as an Argon2id verifier.
CREATE TABLE dataset_preview_sessions (
    id uuid PRIMARY KEY,
    version_id uuid NOT NULL REFERENCES dataset_versions(id) ON DELETE CASCADE,
    project_id uuid NOT NULL REFERENCES projects(id),
    token_verifier text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    created_by_identity_id uuid NOT NULL REFERENCES human_identities(id),
    revoked_at timestamptz,
    CHECK (expires_at > created_at)
);

-- The lookup every preview file request makes, so it is an index rather than a scan.
CREATE INDEX dataset_preview_sessions_version_idx
    ON dataset_preview_sessions (version_id, expires_at DESC)
    WHERE revoked_at IS NULL;
