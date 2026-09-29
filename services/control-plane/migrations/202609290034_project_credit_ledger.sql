-- A per-project credit ledger that records grants, adjustments and reversals and enforces nothing.
--
-- No job is charged, reserved against or refused because of it. The balance is a cached running
-- total of an append-only history, kept honest by the database rather than by the one handler
-- that writes it: every change to an account must be accompanied, in the same transaction, by
-- exactly one entry recording it, and an entry can never be changed or removed afterwards.
--
-- Amounts are whole units in bigint. The API carries them as decimal strings so a browser never
-- rounds one through a double.

-- Who may write to a project's ledger. Deliberately empty: nothing grants this, and there is no
-- bootstrap, because who should hold it is a product decision still to be made. Being listed here
-- is not sufficient on its own either; the handler also requires an active operator membership of
-- the same project, which the composite foreign key below can only partly express (it holds for
-- revoked memberships too).
--
-- Revocation sets revoked_at rather than deleting the row, so who could move credit, and until
-- when, stays answerable.
CREATE TABLE project_credit_managers (
    project_id uuid NOT NULL REFERENCES projects(id),
    identity_id uuid NOT NULL REFERENCES human_identities(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,
    PRIMARY KEY (project_id, identity_id),
    FOREIGN KEY (project_id, identity_id)
        REFERENCES project_memberships (project_id, identity_id)
);

-- Created lazily by the first entry, so a project nobody has ever credited has no row and reads as
-- a zero balance. No ON DELETE CASCADE here or below: removing a project must not quietly take its
-- financial history with it.
CREATE TABLE project_credit_accounts (
    project_id uuid PRIMARY KEY REFERENCES projects(id),
    balance_units bigint NOT NULL DEFAULT 0 CHECK (balance_units >= 0),
    -- The sequence of the newest entry. A counter on the account rather than a database sequence,
    -- so a rolled-back write leaves no gap and the entries of an account number 1, 2, 3 exactly.
    last_sequence bigint NOT NULL DEFAULT 0 CHECK (last_sequence >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE project_credit_entries (
    id uuid PRIMARY KEY,
    project_id uuid NOT NULL REFERENCES project_credit_accounts(project_id),
    sequence bigint NOT NULL CHECK (sequence > 0),
    kind text NOT NULL CHECK (kind IN ('grant', 'adjustment', 'reversal')),
    amount_units bigint NOT NULL CHECK (amount_units <> 0),
    balance_after_units bigint NOT NULL CHECK (balance_after_units >= 0),
    reason text NOT NULL CHECK (length(reason) BETWEEN 1 AND 500),
    actor_identity_id uuid NOT NULL REFERENCES human_identities(id),
    reverses_entry_id uuid,
    idempotency_key uuid NOT NULL,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, sequence),
    -- The target of the composite reversal key below.
    UNIQUE (project_id, id),
    -- A retry is recognised per project and per writer, so one person's keys can neither collide
    -- with nor reveal another's.
    UNIQUE (project_id, actor_identity_id, idempotency_key),
    -- Composite, so a reversal can only ever name an entry in its own project's ledger.
    FOREIGN KEY (project_id, reverses_entry_id)
        REFERENCES project_credit_entries (project_id, id),
    CHECK (
        (kind = 'grant' AND amount_units > 0 AND reverses_entry_id IS NULL)
        OR (kind = 'adjustment' AND reverses_entry_id IS NULL)
        OR (kind = 'reversal' AND reverses_entry_id IS NOT NULL)
    )
);

-- An entry is reversed at most once.
CREATE UNIQUE INDEX project_credit_entries_reversed_once_idx
    ON project_credit_entries (reverses_entry_id)
    WHERE reverses_entry_id IS NOT NULL;

-- History is never edited. The application role holds UPDATE, DELETE and TRUNCATE on every table,
-- so a grant cannot express this; a trigger can. Cascaded deletes fire row triggers too, so this
-- also stops a parent's deletion from reaching the ledger.
CREATE FUNCTION project_credit_refuse_change() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% on % is refused: the credit ledger is append-only', TG_OP, TG_TABLE_NAME
        USING ERRCODE = 'restrict_violation';
END;
$$;

CREATE TRIGGER project_credit_entries_append_only
    BEFORE UPDATE OR DELETE ON project_credit_entries
    FOR EACH ROW EXECUTE FUNCTION project_credit_refuse_change();
CREATE TRIGGER project_credit_entries_no_truncate
    BEFORE TRUNCATE ON project_credit_entries
    FOR EACH STATEMENT EXECUTE FUNCTION project_credit_refuse_change();
CREATE TRIGGER project_credit_accounts_no_delete
    BEFORE DELETE ON project_credit_accounts
    FOR EACH ROW EXECUTE FUNCTION project_credit_refuse_change();
CREATE TRIGGER project_credit_accounts_no_truncate
    BEFORE TRUNCATE ON project_credit_accounts
    FOR EACH STATEMENT EXECUTE FUNCTION project_credit_refuse_change();

-- An account starts empty and moves exactly one entry at a time.
CREATE FUNCTION project_credit_accounts_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.balance_units <> 0 OR NEW.last_sequence <> 0 THEN
            RAISE EXCEPTION 'credit account % must start empty', NEW.project_id
                USING ERRCODE = 'check_violation';
        END IF;
    ELSIF NEW.project_id <> OLD.project_id
        OR NEW.created_at <> OLD.created_at
        OR NEW.last_sequence <> OLD.last_sequence + 1 THEN
        RAISE EXCEPTION 'credit account % may only advance by one entry at a time', OLD.project_id
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER project_credit_accounts_guard
    BEFORE INSERT OR UPDATE ON project_credit_accounts
    FOR EACH ROW EXECUTE FUNCTION project_credit_accounts_guard();

-- The balance is only a cache. At commit, every account change must be matched by the entry that
-- explains it, so a balance edited on its own fails rather than drifting from its history.
CREATE FUNCTION project_credit_accounts_reconcile() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM project_credit_entries
        WHERE project_id = NEW.project_id
          AND sequence = NEW.last_sequence
          AND balance_after_units = NEW.balance_units
    ) THEN
        RAISE EXCEPTION 'credit account % changed without a matching entry %',
            NEW.project_id, NEW.last_sequence
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NULL;
END;
$$;

CREATE CONSTRAINT TRIGGER project_credit_accounts_reconcile
    AFTER UPDATE ON project_credit_accounts
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION project_credit_accounts_reconcile();

-- An entry records the account change made just before it in the same transaction, continues the
-- running total from the entry before it, and, if a reversal, exactly undoes an entry that is not
-- itself a reversal.
CREATE FUNCTION project_credit_entries_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    account_balance bigint;
    account_sequence bigint;
    previous_balance bigint;
    target_kind text;
    target_amount bigint;
BEGIN
    SELECT balance_units, last_sequence INTO account_balance, account_sequence
    FROM project_credit_accounts WHERE project_id = NEW.project_id;
    IF NEW.sequence <> account_sequence OR NEW.balance_after_units <> account_balance THEN
        RAISE EXCEPTION 'credit entry % does not record the current state of account %',
            NEW.sequence, NEW.project_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.sequence = 1 THEN
        previous_balance := 0;
    ELSE
        SELECT balance_after_units INTO previous_balance FROM project_credit_entries
        WHERE project_id = NEW.project_id AND sequence = NEW.sequence - 1;
    END IF;
    IF previous_balance IS NULL
        OR previous_balance + NEW.amount_units <> NEW.balance_after_units THEN
        RAISE EXCEPTION 'credit entry % does not continue the running balance of account %',
            NEW.sequence, NEW.project_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.kind = 'reversal' THEN
        SELECT kind, amount_units INTO target_kind, target_amount FROM project_credit_entries
        WHERE project_id = NEW.project_id AND id = NEW.reverses_entry_id;
        IF target_kind IS NULL OR target_kind = 'reversal'
            OR NEW.amount_units <> -target_amount THEN
            RAISE EXCEPTION 'credit entry % is not an exact reversal of a reversible entry',
                NEW.id
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER project_credit_entries_guard
    BEFORE INSERT ON project_credit_entries
    FOR EACH ROW EXECUTE FUNCTION project_credit_entries_guard();

COMMENT ON TABLE project_credit_managers IS
    'Identities allowed to write a project''s credit ledger. Starts empty; nothing grants it yet. '
    'Effective only together with an active operator membership of the same project.';
COMMENT ON TABLE project_credit_accounts IS
    'Cached balance and entry counter per project. Not enforced against any job.';
COMMENT ON TABLE project_credit_entries IS
    'Append-only credit history. Reversals exactly undo one earlier non-reversal entry, once.';
COMMENT ON COLUMN project_credit_entries.idempotency_key IS
    'Client Idempotency-Key, unique per project and writer; a replay must match the entry exactly.';
