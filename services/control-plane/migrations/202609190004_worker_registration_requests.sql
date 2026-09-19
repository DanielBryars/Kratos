CREATE TABLE worker_registration_requests (
    id uuid PRIMARY KEY,
    agent_instance_id uuid NOT NULL,
    display_name text NOT NULL CHECK (length(display_name) BETWEEN 1 AND 100),
    protocol_version text NOT NULL CHECK (protocol_version ~ '^1\.[0-9]+$'),
    capabilities jsonb NOT NULL CHECK (jsonb_typeof(capabilities) = 'object'),
    public_key bytea NOT NULL CHECK (octet_length(public_key) = 32),
    confirmation_code text NOT NULL CHECK (confirmation_code ~ '^[A-Z2-9]{4}-[A-Z2-9]{4}$'),
    claim_challenge bytea CHECK (claim_challenge IS NULL OR octet_length(claim_challenge) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    approved_at timestamptz,
    approved_by_identity_id uuid REFERENCES human_identities(id),
    rejected_at timestamptz,
    rejected_by_identity_id uuid REFERENCES human_identities(id),
    claimed_at timestamptz,
    worker_id uuid REFERENCES workers(id),
    CHECK (expires_at > created_at),
    CHECK (NOT (approved_at IS NOT NULL AND rejected_at IS NOT NULL)),
    CHECK ((approved_at IS NULL) = (approved_by_identity_id IS NULL)),
    CHECK ((rejected_at IS NULL) = (rejected_by_identity_id IS NULL)),
    CHECK ((approved_at IS NULL) = (claim_challenge IS NULL)),
    CHECK ((claimed_at IS NULL) = (worker_id IS NULL))
);

CREATE UNIQUE INDEX worker_registration_requests_one_open_per_agent
    ON worker_registration_requests (agent_instance_id)
    WHERE claimed_at IS NULL AND rejected_at IS NULL;

CREATE UNIQUE INDEX worker_registration_requests_public_key_idx
    ON worker_registration_requests (public_key)
    WHERE claimed_at IS NULL AND rejected_at IS NULL;

CREATE INDEX worker_registration_requests_pending_idx
    ON worker_registration_requests (created_at)
    WHERE approved_at IS NULL AND rejected_at IS NULL AND claimed_at IS NULL;

COMMENT ON COLUMN worker_registration_requests.public_key IS
    'Raw Ed25519 public key. The corresponding private key never leaves the worker.';
COMMENT ON COLUMN worker_registration_requests.confirmation_code IS
    'Human comparison code derived from the public key; it is not an authenticator.';
