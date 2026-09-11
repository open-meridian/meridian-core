-- The ledger's schema.
--
-- Three tables, and the constraints are the design rather than decoration.
--
-- Applied on start under an advisory lock. `CREATE TABLE IF NOT EXISTS` is not
-- the concurrency answer it reads as: two connections running it at the same
-- moment race inside Postgres' own catalogue and one of them fails.
--
-- This file is the schema as created. Columns added after a database already
-- existed live in `postgres.rs` instead, because they have to be guarded: see
-- ADDITIONS there for why an unguarded ALTER deadlocks against live traffic.
--
-- Both together are enough while every change is additive and no more. This
-- ledger holds statements and positions that cannot be rebuilt from anywhere,
-- unlike the replica, so the first change that renames or drops a column needs
-- a real migration history. Queued as kernel/ledger-needs-migrations.

CREATE TABLE IF NOT EXISTS statement (
    statement_id          text PRIMARY KEY,
    source                text   NOT NULL,
    external_statement_id text   NOT NULL,
    as_of_date            text   NOT NULL,
    read_at_ns            bigint NOT NULL,

    -- How many rows will follow. The only thing that marks the end of a
    -- statement, so a statement is complete when this many holdings point at
    -- it and not before.
    expected_rows         integer NOT NULL DEFAULT 0,

    -- Set once, by the row that completed it. A unique guard rather than a
    -- flag we check and then set, because checking and setting in two steps is
    -- how a statement announces itself twice.
    completed_at_ns       bigint,

    -- What makes a redelivery recognisable rather than a duplicate. The rail's
    -- identifiers are its own, so two rails may number theirs alike.
    UNIQUE (source, external_statement_id)
);

CREATE TABLE IF NOT EXISTS holding (
    holding_id       text PRIMARY KEY,
    statement_id     text   NOT NULL REFERENCES statement (statement_id) ON DELETE CASCADE,
    account_id       text   NOT NULL,

    -- NULL when the connector could not resolve it. The row is kept either
    -- way: dropping it loses the only evidence that something was held.
    instrument_id    text,

    quantity_scaled  bigint NOT NULL,
    value_scaled     bigint NOT NULL,
    currency         text   NOT NULL,
    escalated        boolean NOT NULL DEFAULT false,

    -- Exactly one of an instrument or some identifiers. Enforced here as well
    -- as in the code, because a row that names both describes two things and a
    -- row that names neither describes nothing, and neither should be storable
    -- by any path.
    identifiers      jsonb  NOT NULL DEFAULT '[]'::jsonb,
    CONSTRAINT resolved_or_identified CHECK (
        (instrument_id IS NOT NULL AND jsonb_array_length(identifiers) = 0)
     OR (instrument_id IS NULL     AND jsonb_array_length(identifiers) > 0)
    )
);

CREATE INDEX IF NOT EXISTS holding_by_statement ON holding (statement_id);
CREATE INDEX IF NOT EXISTS holding_unresolved ON holding (account_id) WHERE instrument_id IS NULL;

-- What the custodian says an account holds. Named for whose belief it is: our
-- own book is calculated from our own activity, does not exist yet, and is a
-- different number whose disagreement with this one is the whole of
-- reconciliation.
CREATE TABLE IF NOT EXISTS custodial_position (
    account_id        text   NOT NULL,
    instrument_id     text   NOT NULL,
    quantity_scaled   bigint NOT NULL,
    value_scaled      bigint NOT NULL,
    currency          text   NOT NULL,

    -- Which statement last set this, and what that statement's positions
    -- reflected. Together they say how current this is without a reader having
    -- to ask anything else.
    last_statement_id text   NOT NULL,
    as_of_date        text   NOT NULL,
    updated_at_ns     bigint NOT NULL,

    PRIMARY KEY (account_id, instrument_id)
);
