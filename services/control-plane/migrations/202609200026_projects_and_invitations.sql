-- Projects, membership and invitations (ADR-017).
--
-- Until now authorisation identity and ownership were the same UUID: every list filtered
-- WHERE owner_identity_id = $caller. A second person could not see anything, because everything
-- they could be shown was owned by someone else. This adds the project that ownership is really
-- about, and leaves owner_identity_id in place as the record of who did it.

CREATE TABLE projects (
    id uuid PRIMARY KEY,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Every member is an owner. ADR-017 defers roles below owner rather than inventing one that
-- nothing yet distinguishes, so membership itself is the authority.
--
-- A revoked membership is kept rather than deleted: it is how "who could see this, and until
-- when" stays answerable after someone is removed.
CREATE TABLE project_memberships (
    project_id uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    identity_id uuid NOT NULL REFERENCES human_identities(id),
    invited_by_identity_id uuid REFERENCES human_identities(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,
    revoked_by_identity_id uuid REFERENCES human_identities(id),
    PRIMARY KEY (project_id, identity_id),
    CHECK ((revoked_at IS NULL) = (revoked_by_identity_id IS NULL))
);

-- The lookup every authorised request makes, so it is an index rather than a scan.
CREATE INDEX project_memberships_identity_idx
    ON project_memberships (identity_id)
    WHERE revoked_at IS NULL;

-- Counting a project's owners is the last-owner rail, and it runs inside the transaction that
-- removes one. A partial index keeps that count cheap enough to hold a lock across.
CREATE INDEX project_memberships_active_idx
    ON project_memberships (project_id)
    WHERE revoked_at IS NULL;

-- The same shape as worker_enrolments, deliberately. Single use, expiring, revocable, and only
-- an Argon2id verifier is stored; the plaintext is shown to its creator once and never again.
CREATE TABLE project_invitations (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_by_identity_id uuid NOT NULL REFERENCES human_identities(id),
    token_verifier text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    consumed_at timestamptz,
    consumed_by_identity_id uuid REFERENCES human_identities(id),
    revoked_at timestamptz,
    revoked_by_identity_id uuid REFERENCES human_identities(id),
    CHECK (expires_at > created_at),
    -- An invitation is consumed or revoked, never both. The state machine is in the schema so
    -- that a code path which forgets it cannot produce a row that means nothing.
    CHECK (NOT (consumed_at IS NOT NULL AND revoked_at IS NOT NULL)),
    CHECK ((consumed_at IS NULL) = (consumed_by_identity_id IS NULL)),
    CHECK ((revoked_at IS NULL) = (revoked_by_identity_id IS NULL))
);

CREATE INDEX project_invitations_pending_idx
    ON project_invitations (project_id, created_at DESC)
    WHERE consumed_at IS NULL AND revoked_at IS NULL;

-- --- Ownership moves to the project -----------------------------------------------------------
--
-- Added nullable, backfilled, then made NOT NULL. A resource without a project is invisible to
-- everyone once the predicates change, including to the person who created it, so the constraint
-- is what stops a future insert quietly disappearing.

ALTER TABLE compute_groups ADD COLUMN project_id uuid REFERENCES projects(id);
ALTER TABLE worker_enrolments ADD COLUMN project_id uuid REFERENCES projects(id);
ALTER TABLE workers ADD COLUMN project_id uuid REFERENCES projects(id);
ALTER TABLE jobs ADD COLUMN project_id uuid REFERENCES projects(id);

-- --- Backfill -----------------------------------------------------------------------------------
--
-- One default project holds everything that exists. It is created unconditionally, including on
-- an empty database, so that the NOT NULL constraints below have somewhere to point and the
-- bootstrap operator has a project to join on first sign-in rather than needing one created for
-- them in the same breath.
INSERT INTO projects (id, name)
VALUES ('00000000-0000-4000-8000-00000000d00f', 'Kratos');

-- Everyone who owns anything today, and every identity that exists, becomes a member. On a fresh
-- database this inserts nothing and the first sign-in adds the first member.
INSERT INTO project_memberships (project_id, identity_id)
SELECT '00000000-0000-4000-8000-00000000d00f', id
FROM human_identities
ON CONFLICT DO NOTHING;

UPDATE compute_groups SET project_id = '00000000-0000-4000-8000-00000000d00f';
UPDATE worker_enrolments SET project_id = '00000000-0000-4000-8000-00000000d00f';
UPDATE workers SET project_id = '00000000-0000-4000-8000-00000000d00f';
UPDATE jobs SET project_id = '00000000-0000-4000-8000-00000000d00f';

ALTER TABLE compute_groups ALTER COLUMN project_id SET NOT NULL;
ALTER TABLE worker_enrolments ALTER COLUMN project_id SET NOT NULL;
ALTER TABLE workers ALTER COLUMN project_id SET NOT NULL;
ALTER TABLE jobs ALTER COLUMN project_id SET NOT NULL;

-- The predicates that used to filter by owner now filter by project, so these are the shapes
-- that matter. The old owner indexes stay: owner_identity_id is still read, as attribution.
CREATE INDEX jobs_project_submitted_idx ON jobs (project_id, submitted_at DESC);
CREATE INDEX workers_project_status_idx ON workers (project_id, status);
CREATE INDEX worker_enrolments_project_idx ON worker_enrolments (project_id, created_at DESC);
