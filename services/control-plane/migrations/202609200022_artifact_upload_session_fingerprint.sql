ALTER TABLE artifact_upload_grants
    ADD COLUMN session_uri_sha256 text
        CHECK (session_uri_sha256 IS NULL OR session_uri_sha256 ~ '^[0-9a-f]{64}$');

COMMENT ON COLUMN artifact_upload_grants.session_uri_sha256 IS
    'Non-secret SHA-256 fingerprint of the exact resumable session URI bytes, retained after the URI is consumed so abandon replay remains session-bound.';
