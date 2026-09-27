-- Project-scoped immutable datasets and episode curation (ADR-018).

CREATE TABLE datasets (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects(id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    description text CHECK (description IS NULL OR length(description) <= 2000),
    created_by_identity_id uuid NOT NULL REFERENCES human_identities(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    archived_at timestamptz,
    UNIQUE (project_id, name)
);

CREATE INDEX datasets_project_created_idx
    ON datasets (project_id, created_at DESC)
    WHERE archived_at IS NULL;

CREATE TABLE dataset_versions (
    id uuid PRIMARY KEY,
    dataset_id uuid NOT NULL REFERENCES datasets(id),
    project_id uuid NOT NULL REFERENCES projects(id),
    version_number integer NOT NULL CHECK (version_number > 0),
    source_kind text NOT NULL CHECK (source_kind IN ('hugging_face', 'upload')),
    status text NOT NULL CHECK (status IN ('draft', 'uploading', 'ready', 'failed')),
    source_repository text,
    requested_revision text,
    resolved_revision text,
    manifest_sha256 text CHECK (manifest_sha256 IS NULL OR manifest_sha256 ~ '^[0-9a-f]{64}$'),
    info_json jsonb NOT NULL,
    validation_json jsonb NOT NULL DEFAULT '{}'::jsonb,
    total_episodes integer NOT NULL CHECK (total_episodes >= 0),
    total_frames bigint NOT NULL CHECK (total_frames >= 0),
    fps double precision NOT NULL CHECK (
        fps > 0
        AND fps <> 'Infinity'::double precision
        AND fps <> 'NaN'::double precision
    ),
    created_by_identity_id uuid NOT NULL REFERENCES human_identities(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    ready_at timestamptz,
    UNIQUE (dataset_id, version_number),
    UNIQUE (dataset_id, id),
    CHECK (
        (source_kind = 'hugging_face'
            AND source_repository IS NOT NULL
            AND requested_revision IS NOT NULL
            AND resolved_revision IS NOT NULL
            AND resolved_revision ~ '^[0-9a-f]{40}$')
        OR
        (source_kind = 'upload'
            AND source_repository IS NULL
            AND requested_revision IS NULL
            AND resolved_revision IS NULL)
    ),
    CHECK ((status = 'ready') = (ready_at IS NOT NULL AND manifest_sha256 IS NOT NULL))
);

CREATE INDEX dataset_versions_project_created_idx
    ON dataset_versions (project_id, created_at DESC);

CREATE TABLE dataset_files (
    id uuid PRIMARY KEY,
    version_id uuid NOT NULL REFERENCES dataset_versions(id) ON DELETE CASCADE,
    project_id uuid NOT NULL REFERENCES projects(id),
    logical_path text NOT NULL CHECK (
        octet_length(logical_path) BETWEEN 1 AND 512
        AND logical_path !~ '(^|/)(\.|\.\.|)($|/)'
        AND left(logical_path, 1) <> '/'
        AND position(E'\\' in logical_path) = 0
    ),
    media_type text NOT NULL CHECK (length(media_type) BETWEEN 1 AND 200),
    byte_length bigint NOT NULL CHECK (byte_length > 0),
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    storage_bucket text,
    storage_object_key text,
    storage_generation bigint,
    status text NOT NULL CHECK (status IN ('declared', 'uploading', 'verified', 'rejected')),
    rejection_reason text,
    verified_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (version_id, logical_path),
    CHECK ((storage_bucket IS NULL) = (storage_object_key IS NULL)),
    CHECK ((status = 'verified') = (storage_generation IS NOT NULL AND verified_at IS NOT NULL))
);

CREATE INDEX dataset_files_version_idx ON dataset_files (version_id, logical_path);

CREATE TABLE dataset_file_uploads (
    file_id uuid PRIMARY KEY REFERENCES dataset_files(id) ON DELETE CASCADE,
    session_uri text NOT NULL,
    issued_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    completed_at timestamptz,
    CHECK (expires_at > issued_at)
);

CREATE TABLE dataset_episode_curations (
    version_id uuid NOT NULL REFERENCES dataset_versions(id) ON DELETE CASCADE,
    project_id uuid NOT NULL REFERENCES projects(id),
    episode_index integer NOT NULL CHECK (episode_index >= 0),
    decision text NOT NULL CHECK (decision IN ('included', 'excluded', 'needs_review')),
    note text CHECK (note IS NULL OR length(note) <= 2000),
    updated_by_identity_id uuid NOT NULL REFERENCES human_identities(id),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (version_id, episode_index)
);

CREATE INDEX dataset_episode_curations_project_idx
    ON dataset_episode_curations (project_id, version_id, episode_index);

CREATE TABLE dataset_views (
    id uuid PRIMARY KEY,
    dataset_id uuid NOT NULL REFERENCES datasets(id),
    version_id uuid NOT NULL REFERENCES dataset_versions(id),
    project_id uuid NOT NULL REFERENCES projects(id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    manifest_sha256 text NOT NULL CHECK (manifest_sha256 ~ '^[0-9a-f]{64}$'),
    included_episode_count integer NOT NULL CHECK (included_episode_count >= 0),
    created_by_identity_id uuid NOT NULL REFERENCES human_identities(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (version_id, name),
    UNIQUE (version_id, id)
);

CREATE TABLE dataset_view_episodes (
    view_id uuid NOT NULL REFERENCES dataset_views(id) ON DELETE CASCADE,
    episode_index integer NOT NULL CHECK (episode_index >= 0),
    PRIMARY KEY (view_id, episode_index)
);
