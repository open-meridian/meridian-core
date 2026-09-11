-- The ledger's schema.
--
-- Three tables, and the constraints are the design rather than decoration.
--
-- Applied on start under an advisory lock. `CREATE TABLE IF NOT EXISTS` is not
-- the concurrency answer it reads as: two connections running it at the same
-- moment race inside Postgres' own catalogue and one of them fails.

CREATE TABLE IF NOT EXISTS statement (
    statement_id          text PRIMARY KEY,
    source                text   NOT NULL,
    external_statement_id text   NOT NULL,
    as_of_date            text   NOT NULL,
    read_at_ns            bigint NOT NULL,

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

CREATE TABLE IF NOT EXISTS position (
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
