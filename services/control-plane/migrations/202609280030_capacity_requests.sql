-- Capacity requests and their dispatch outbox (R0.7).
--
-- Kratos stays the authoritative queue. A capacity provider supplies bounded machines and nothing
-- else: it does not decide fairness, budgets, job state or result identity. These tables exist so
-- that asking a provider for a machine is durable and replayable, and so that a request can be
-- reconciled after a restart without asking the provider to create a second one.
--
-- Nothing here starts any capacity. The boundary and a fake provider are all that exist at this
-- point, and the feature is off unless explicitly enabled.

-- Lets a child carry (job, project) and have the database prove the pair is the one the job
-- actually has, rather than trusting whoever wrote the row. The same trick job_artifacts uses for
-- (attempt, job).
ALTER TABLE jobs ADD CONSTRAINT jobs_id_project_unique UNIQUE (id, project_id);

CREATE TABLE capacity_requests (
    id uuid PRIMARY KEY,
    -- One request per attempt, which is what makes the request replayable rather than repeatable.
    -- It also means a job cancelled while queued can have no request at all: there is no attempt
    -- to derive one from, so the constraint decides it rather than any controller logic.
    attempt_id uuid NOT NULL UNIQUE,
    job_id uuid NOT NULL,
    project_id uuid NOT NULL,
    provider text NOT NULL CHECK (length(provider) BETWEEN 1 AND 40),
    -- Derived from the attempt, so a replay after a restart asks the provider the same question
    -- and is answered with the same machine rather than a second one.
    idempotency_key text NOT NULL UNIQUE CHECK (length(idempotency_key) BETWEEN 1 AND 200),
    -- `unreconciled` is the honest answer to a question nobody can answer from here: the provider
    -- was asked, it never gave a usable reply, and it may or may not have built the machine. A
    -- provider error that is not a positive refusal promises nothing about side effects, so
    -- calling that state `failed` would be asserting something unknown.
    status text NOT NULL DEFAULT 'requested'
        CHECK (status IN ('requested', 'provisioning', 'ready', 'releasing', 'released',
                          'unreconciled', 'failed')),
    -- What the provider calls the machine. Null until the provider has answered, and never
    -- overwritten once set: it is the only handle by which capacity can be released.
    external_id text CHECK (external_id IS NULL OR length(external_id) BETWEEN 1 AND 200),
    last_error text CHECK (last_error IS NULL OR length(last_error) <= 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    released_at timestamptz,
    -- A released request has finished with its machine; anything else has not.
    CHECK ((status = 'released') = (released_at IS NOT NULL)),
    -- Capacity cannot be ready without something to point at.
    CHECK (status NOT IN ('ready', 'releasing') OR external_id IS NOT NULL),
    -- Composite rather than three independent keys. Separate references would each be satisfied
    -- while still permitting a request whose attempt belongs to one job and whose job belongs to
    -- another project -- capacity billed to a project that never asked for it.
    FOREIGN KEY (attempt_id, job_id) REFERENCES job_attempts(id, job_id) ON DELETE CASCADE,
    FOREIGN KEY (job_id, project_id) REFERENCES jobs(id, project_id) ON DELETE CASCADE
);

CREATE INDEX capacity_requests_job_idx ON capacity_requests (job_id);
-- Open means not released, and nothing else. Excluding terminal-looking statuses as well would
-- hide precisely the requests a person needs to see: one that failed without a positive refusal
-- may still have a machine behind it. `released` is the only state that means nothing is held.
CREATE INDEX capacity_requests_open_idx
    ON capacity_requests (project_id, created_at)
    WHERE released_at IS NULL;

-- The outbox. A row is an intention to tell the provider something, and it survives a restart in
-- the middle of telling it.
--
-- Separate from the request's own status on purpose: the status is what Kratos believes, and a
-- dispatch is an attempt to make the provider agree. Collapsing the two would mean a crash
-- between "we decided" and "we told them" is indistinguishable from never having decided.
CREATE TABLE capacity_dispatches (
    id uuid PRIMARY KEY,
    request_id uuid NOT NULL REFERENCES capacity_requests(id) ON DELETE CASCADE,
    action text NOT NULL CHECK (action IN ('provision', 'release')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error text CHECK (last_error IS NULL OR length(last_error) <= 500),
    -- Not before this moment. Backoff is a timestamp rather than a sleep so it survives a restart.
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- One outstanding dispatch per action per request. A second provision for the same request is
    -- exactly the double-dispatch this table exists to make impossible.
    UNIQUE (request_id, action)
);

CREATE INDEX capacity_dispatches_pending_idx
    ON capacity_dispatches (next_attempt_at)
    WHERE completed_at IS NULL;

-- Deleting the parent job or attempt would cascade away `external_id` -- the only handle by which
-- the machine can ever be released -- and the pending release dispatch with it, leaving a paid
-- machine running with nothing in the database pointing at it.
--
-- The test is `released_at IS NULL`, not "holds a handle". A request that is merely `requested` or
-- `provisioning` has no handle yet and may have a provider call in flight this instant: cascade it
-- away and the provider's answer arrives with nowhere durable to record the machine it describes.
-- Being certain nothing is running is exactly what `released` means, and it is the only state that
-- means it.
--
-- A trigger rather than ON DELETE RESTRICT because the rule is conditional on the row's state: a
-- released request holds nothing and should not stand in the way of retention tidying a finished
-- job away. Cascaded deletes fire row triggers, so this guards the parents too.
CREATE FUNCTION capacity_requests_refuse_open_delete() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.released_at IS NULL THEN
        RAISE EXCEPTION
            'capacity request % is not released (status %, handle %); settle it before deleting',
            OLD.id, OLD.status, coalesce(OLD.external_id, 'none')
            USING ERRCODE = 'restrict_violation';
    END IF;
    RETURN OLD;
END;
$$;

CREATE TRIGGER capacity_requests_open_delete_guard
    BEFORE DELETE ON capacity_requests
    FOR EACH ROW EXECUTE FUNCTION capacity_requests_refuse_open_delete();

COMMENT ON TABLE capacity_requests IS
    'One bounded capacity request per job attempt. Kratos remains the authoritative queue; a '
    'provider supplies machines and decides nothing about fairness, budget, job state or results.';
COMMENT ON COLUMN capacity_requests.idempotency_key IS
    'Derived from the attempt so a replay asks the provider the same question and receives the '
    'same machine rather than a second one.';
COMMENT ON TABLE capacity_dispatches IS
    'Durable outbox of things to tell a provider. Separate from the request status so that a '
    'crash between deciding and telling is distinguishable from never having decided.';
