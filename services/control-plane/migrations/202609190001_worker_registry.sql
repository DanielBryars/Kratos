CREATE TABLE human_identities (
    id uuid PRIMARY KEY,
    provider text NOT NULL,
    provider_subject text NOT NULL,
    display_name text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 200),
    email text,
    created_at timestamptz NOT NULL DEFAULT now(),
    disabled_at timestamptz,
    UNIQUE (provider, provider_subject)
);

CREATE TABLE compute_groups (
    id uuid PRIMARY KEY,
    owner_identity_id uuid NOT NULL REFERENCES human_identities(id),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (owner_identity_id, name)
);

CREATE TABLE worker_enrolments (
    id uuid PRIMARY KEY,
    owner_identity_id uuid NOT NULL REFERENCES human_identities(id),
    token_verifier text NOT NULL,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    consumed_at timestamptz,
    revoked_at timestamptz,
    consumed_by_worker_id uuid,
    CHECK (expires_at > created_at),
    CHECK (NOT (consumed_at IS NOT NULL AND revoked_at IS NOT NULL))
);

CREATE TABLE workers (
    id uuid PRIMARY KEY,
    owner_identity_id uuid NOT NULL REFERENCES human_identities(id),
    agent_instance_id uuid NOT NULL UNIQUE,
    display_name text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 100),
    protocol_version text NOT NULL CHECK (protocol_version ~ '^1\.[0-9]+$'),
    status text NOT NULL DEFAULT 'unapproved'
        CHECK (status IN ('unapproved', 'idle', 'busy', 'draining', 'quarantined', 'revoked')),
    capabilities jsonb NOT NULL CHECK (jsonb_typeof(capabilities) = 'object'),
    heartbeat_sequence bigint NOT NULL DEFAULT -1 CHECK (heartbeat_sequence >= -1),
    last_seen_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

ALTER TABLE worker_enrolments
    ADD CONSTRAINT worker_enrolments_consumed_worker_fk
    FOREIGN KEY (consumed_by_worker_id) REFERENCES workers(id);

CREATE TABLE worker_credentials (
    id uuid PRIMARY KEY,
    worker_id uuid NOT NULL REFERENCES workers(id) ON DELETE CASCADE,
    token_verifier text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz,
    revoked_at timestamptz,
    CHECK (expires_at IS NULL OR expires_at > created_at)
);

CREATE UNIQUE INDEX worker_credentials_one_active
    ON worker_credentials (worker_id)
    WHERE revoked_at IS NULL;

CREATE TABLE compute_group_members (
    compute_group_id uuid NOT NULL REFERENCES compute_groups(id) ON DELETE CASCADE,
    worker_id uuid NOT NULL REFERENCES workers(id) ON DELETE CASCADE,
    approved_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (compute_group_id, worker_id)
);

CREATE TABLE audit_events (
    id uuid PRIMARY KEY,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    actor_type text NOT NULL CHECK (actor_type IN ('human', 'worker', 'service')),
    actor_id uuid,
    action text NOT NULL CHECK (length(action) BETWEEN 1 AND 200),
    target_type text NOT NULL CHECK (length(target_type) BETWEEN 1 AND 100),
    target_id uuid,
    outcome text NOT NULL CHECK (outcome IN ('succeeded', 'denied', 'failed')),
    detail jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(detail) = 'object')
);

CREATE INDEX workers_owner_status_idx ON workers (owner_identity_id, status);
CREATE INDEX workers_last_seen_idx ON workers (last_seen_at);
CREATE INDEX worker_enrolments_owner_idx ON worker_enrolments (owner_identity_id, created_at DESC);
CREATE INDEX audit_events_target_idx ON audit_events (target_type, target_id, occurred_at DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_type, actor_id, occurred_at DESC);

COMMENT ON COLUMN worker_enrolments.token_verifier IS
    'Argon2id verifier only; the bootstrap credential is never stored.';
COMMENT ON COLUMN worker_credentials.token_verifier IS
    'Argon2id verifier only; the worker credential is returned once.';
