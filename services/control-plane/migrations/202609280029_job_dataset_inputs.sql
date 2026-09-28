-- Immutable dataset inputs selected when a job is queued.

CREATE TABLE job_dataset_inputs (
    id uuid PRIMARY KEY,
    job_id uuid NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    alias text NOT NULL CHECK (alias ~ '^[a-z][a-z0-9_-]{0,31}$'),
    dataset_id uuid NOT NULL,
    dataset_version_id uuid NOT NULL,
    dataset_view_id uuid,
    source_kind text NOT NULL CHECK (source_kind IN ('hugging_face', 'upload')),
    source_repository text,
    resolved_revision text,
    dataset_manifest_sha256 text NOT NULL CHECK (dataset_manifest_sha256 ~ '^[0-9a-f]{64}$'),
    dataset_view_manifest_sha256 text CHECK (
        dataset_view_manifest_sha256 IS NULL
        OR dataset_view_manifest_sha256 ~ '^[0-9a-f]{64}$'
    ),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (job_id, alias),
    FOREIGN KEY (dataset_id, dataset_version_id)
        REFERENCES dataset_versions(dataset_id, id),
    FOREIGN KEY (dataset_version_id, dataset_view_id)
        REFERENCES dataset_views(version_id, id),
    CHECK (
        (source_kind = 'hugging_face'
            AND source_repository IS NOT NULL
            AND resolved_revision ~ '^[0-9a-f]{40}$')
        OR
        (source_kind = 'upload'
            AND source_repository IS NULL
            AND resolved_revision IS NULL)
    ),
    CHECK ((dataset_view_id IS NULL) = (dataset_view_manifest_sha256 IS NULL))
);

CREATE INDEX job_dataset_inputs_version_idx
    ON job_dataset_inputs (dataset_version_id, dataset_view_id);

ALTER TABLE observation_streams
    ADD COLUMN dataset_lineage jsonb NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(dataset_lineage) = 'array');

COMMENT ON COLUMN observation_streams.dataset_lineage IS
    'Trusted immutable dataset identities copied from the job specification for MLflow run creation.';
