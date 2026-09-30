-- Credits are pounds sterling, and a unit is one penny (ADR-020, ADR-021).
--
-- This changes no data and no constraint. It exists because the ledger shipped with "whole units"
-- and no statement of what a unit is, and the ledger is append-only: an entry written under one
-- meaning of "unit" can never be rewritten under another. So the meaning is recorded against the
-- columns themselves, before the first entry exists, rather than only in application code that a
-- future reader of the database would not see.
--
-- Integer pence rather than pounds, because the costs this will eventually record -- GPU time --
-- are fractions of a pound; and integers rather than numeric, because the ledger's arithmetic,
-- constraints and idempotency comparisons are all exact integer comparisons and should stay so.

COMMENT ON COLUMN project_credit_accounts.balance_units IS
    'Balance in pence (GBP, minor unit exponent 2). Never negative.';
COMMENT ON COLUMN project_credit_entries.amount_units IS
    'Signed change in pence (GBP, minor unit exponent 2). Never zero.';
COMMENT ON COLUMN project_credit_entries.balance_after_units IS
    'Account balance in pence (GBP, minor unit exponent 2) immediately after this entry.';
