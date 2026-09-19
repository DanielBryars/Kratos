ALTER TABLE human_identities
    ADD COLUMN role text NOT NULL DEFAULT 'member'
    CHECK (role IN ('member', 'operator'));

CREATE INDEX human_identities_role_idx ON human_identities (role) WHERE disabled_at IS NULL;
