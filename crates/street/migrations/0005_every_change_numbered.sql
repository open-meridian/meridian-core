-- Every change numbered, read within a scope and since a watermark; the
-- statement names its account, external account and institution, and its
-- figures are a set per margin segment with their collateral; a holding
-- carries its cost, average cost, lots and margin requirement (contract v7:
-- plans/the-book-holds-positions, revision A; W2.2 to W2.7, W2.9).
--
-- The partition's head is a row, updated in the transaction that makes each
-- change, never a database sequence, which skips a number on a rollback that a
-- reader would take for a lost delivery (design decision 5). Each account's
-- last change of each kind is a row too, so a change names the previous one
-- of its kind for its account (Q3 clarified per row, 2026-10-01).
--
-- What was recorded before this is numbered 0 and chained to nothing: a
-- reader's first read answers it at the head, and the first change after it
-- names 0 as its previous. A statement completed before carries no number.
--
-- A statement's account is filled from its rows where it has any, which is
-- what a statement from a plugin before v7 takes; its three flat figures
-- become its one set with no segment, and leave the statement.
--
-- Guarded, as migrations 2 to 4 are: a database adopted from before the
-- history may not have every table, and a migration that finds nothing to do
-- must touch nothing, since ALTER TABLE locks even then.

-- What the wire carries (decisions/023), as migrations 3 and 4 hold their
-- columns to it, once for every column below: NULL, or at most 18 places and
-- 38 digits. Numeric arithmetic throughout, never double precision.
CREATE OR REPLACE FUNCTION street_on_the_wire(value numeric) RETURNS boolean
    LANGUAGE sql IMMUTABLE AS $f$
    SELECT value IS NULL
        OR (scale(value) <= 18 AND abs(value) < power(10::numeric, 38 - scale(value)))
$f$;

CREATE TABLE IF NOT EXISTS partition_head (
    partition text   PRIMARY KEY,
    sequence  bigint NOT NULL CHECK (sequence >= 0)
);
INSERT INTO partition_head (partition, sequence) VALUES ('street', 0)
    ON CONFLICT (partition) DO NOTHING;

CREATE TABLE IF NOT EXISTS account_chain (
    chain      text   NOT NULL CHECK (chain IN ('position', 'statement')),
    account_id text   NOT NULL,
    sequence   bigint NOT NULL,
    PRIMARY KEY (chain, account_id)
);

CREATE TABLE IF NOT EXISTS statement_figures (
    statement_id                text    NOT NULL,
    segment                     text    NOT NULL,
    -- The order the statement gave its sets in.
    ordinal                     integer NOT NULL DEFAULT 0,
    buying_power                numeric,
    buying_power_currency       text,
    margin_requirement          numeric,
    margin_requirement_currency text,
    maintenance_excess          numeric,
    maintenance_excess_currency text,
    initial_margin              numeric,
    initial_margin_currency     text,
    variation_margin            numeric,
    variation_margin_currency   text,
    net_liquidation             numeric,
    net_liquidation_currency    text,
    PRIMARY KEY (statement_id, segment),
    CONSTRAINT statement_figures_on_the_wire CHECK (
            street_on_the_wire(buying_power) AND street_on_the_wire(margin_requirement)
        AND street_on_the_wire(maintenance_excess) AND street_on_the_wire(initial_margin)
        AND street_on_the_wire(variation_margin) AND street_on_the_wire(net_liquidation)),
    CONSTRAINT statement_figures_with_their_currency CHECK (
            (buying_power IS NULL) = (buying_power_currency IS NULL)
        AND (margin_requirement IS NULL) = (margin_requirement_currency IS NULL)
        AND (maintenance_excess IS NULL) = (maintenance_excess_currency IS NULL)
        AND (initial_margin IS NULL) = (initial_margin_currency IS NULL)
        AND (variation_margin IS NULL) = (variation_margin_currency IS NULL)
        AND (net_liquidation IS NULL) = (net_liquidation_currency IS NULL))
);

CREATE TABLE IF NOT EXISTS statement_collateral (
    statement_id                 text    NOT NULL,
    segment                      text    NOT NULL,
    ordinal                      integer NOT NULL,
    direction                    text    NOT NULL CHECK (direction IN ('posted', 'received')),
    instrument_id                text,
    identifiers                  jsonb   NOT NULL DEFAULT '[]'::jsonb,
    quantity                     numeric NOT NULL,
    value                        numeric,
    value_currency               text,
    haircut                      numeric,
    value_after_haircut          numeric,
    value_after_haircut_currency text,
    held_at                      text    NOT NULL DEFAULT '',
    PRIMARY KEY (statement_id, segment, ordinal),
    CONSTRAINT statement_collateral_on_the_wire CHECK (
            street_on_the_wire(quantity) AND street_on_the_wire(value)
        AND street_on_the_wire(haircut) AND street_on_the_wire(value_after_haircut)),
    CONSTRAINT statement_collateral_resolved_or_identified CHECK (
        (instrument_id IS NOT NULL AND jsonb_array_length(identifiers) = 0)
     OR (instrument_id IS NULL     AND jsonb_array_length(identifiers) > 0)),
    CONSTRAINT statement_collateral_with_its_currency CHECK (
            (value IS NULL) = (value_currency IS NULL)
        AND (value_after_haircut IS NULL) = (value_after_haircut_currency IS NULL))
);

CREATE TABLE IF NOT EXISTS holding_lot (
    holding_id    text    NOT NULL,
    ordinal       integer NOT NULL,
    quantity      numeric NOT NULL,
    cost          numeric,
    cost_currency text,
    acquired_date text    NOT NULL DEFAULT '',
    PRIMARY KEY (holding_id, ordinal),
    CONSTRAINT holding_lot_on_the_wire CHECK (
        street_on_the_wire(quantity) AND street_on_the_wire(cost)),
    CONSTRAINT holding_lot_cost_with_its_currency CHECK ((cost IS NULL) = (cost_currency IS NULL))
);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'statement')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'statement' AND column_name = 'account_id') THEN
        ALTER TABLE statement
            ADD COLUMN account_id text NOT NULL DEFAULT '',
            ADD COLUMN external_account_id text NOT NULL DEFAULT '',
            ADD COLUMN institution text NOT NULL DEFAULT '',
            ADD COLUMN completion_sequence bigint NOT NULL DEFAULT 0,
            ADD COLUMN completion_previous bigint NOT NULL DEFAULT 0,
            ADD COLUMN cause_instance_id text NOT NULL DEFAULT '',
            ADD COLUMN cause_acting_for_subject text NOT NULL DEFAULT '',
            ADD COLUMN cause_correlation_id text NOT NULL DEFAULT '',
            ADD COLUMN cause_causation_id text NOT NULL DEFAULT '';

        IF EXISTS (SELECT 1 FROM information_schema.tables
                    WHERE table_schema = current_schema() AND table_name = 'holding') THEN
            UPDATE statement s
               SET account_id = h.account_id
              FROM (SELECT DISTINCT ON (statement_id) statement_id, account_id
                      FROM holding ORDER BY statement_id, holding_id) h
             WHERE h.statement_id = s.statement_id;
        END IF;

        INSERT INTO statement_figures
               (statement_id, segment, buying_power, buying_power_currency, margin_requirement,
                margin_requirement_currency, maintenance_excess, maintenance_excess_currency)
        SELECT statement_id, '', buying_power, buying_power_currency, margin_requirement,
               margin_requirement_currency, maintenance_excess, maintenance_excess_currency
          FROM statement
         WHERE buying_power IS NOT NULL OR margin_requirement IS NOT NULL
            OR maintenance_excess IS NOT NULL
        ON CONFLICT DO NOTHING;

        ALTER TABLE statement
            DROP COLUMN IF EXISTS buying_power,
            DROP COLUMN IF EXISTS buying_power_currency,
            DROP COLUMN IF EXISTS margin_requirement,
            DROP COLUMN IF EXISTS margin_requirement_currency,
            DROP COLUMN IF EXISTS maintenance_excess,
            DROP COLUMN IF EXISTS maintenance_excess_currency;

        CREATE INDEX IF NOT EXISTS statement_by_completion
            ON statement (completion_sequence, statement_id) WHERE completed_at_ns IS NOT NULL;
    END IF;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'holding')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'holding' AND column_name = 'cost_basis') THEN
        ALTER TABLE holding
            ADD COLUMN cost_basis numeric,
            ADD COLUMN cost_basis_currency text,
            ADD COLUMN average_cost numeric,
            ADD COLUMN average_cost_currency text,
            ADD COLUMN margin_requirement numeric,
            ADD COLUMN margin_requirement_currency text,
            ADD CONSTRAINT holding_cost_on_the_wire CHECK (
                    street_on_the_wire(cost_basis) AND street_on_the_wire(average_cost)
                AND street_on_the_wire(margin_requirement)),
            ADD CONSTRAINT holding_cost_with_its_currency CHECK (
                    (cost_basis IS NULL) = (cost_basis_currency IS NULL)
                AND (average_cost IS NULL) = (average_cost_currency IS NULL)
                AND (margin_requirement IS NULL) = (margin_requirement_currency IS NULL));
    END IF;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'custodial_position')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'custodial_position'
                          AND column_name = 'sequence') THEN
        ALTER TABLE custodial_position
            ADD COLUMN cost_basis numeric,
            ADD COLUMN cost_basis_currency text,
            ADD COLUMN average_cost numeric,
            ADD COLUMN average_cost_currency text,
            ADD COLUMN margin_requirement numeric,
            ADD COLUMN margin_requirement_currency text,
            ADD COLUMN last_holding_id text,
            ADD COLUMN sequence bigint NOT NULL DEFAULT 0,
            ADD COLUMN previous_sequence bigint NOT NULL DEFAULT 0,
            ADD COLUMN removed boolean NOT NULL DEFAULT false,
            ADD COLUMN changed_by_instance text NOT NULL DEFAULT '',
            ADD COLUMN changed_for_subject text NOT NULL DEFAULT '',
            ADD CONSTRAINT custodial_position_cost_on_the_wire CHECK (
                    street_on_the_wire(cost_basis) AND street_on_the_wire(average_cost)
                AND street_on_the_wire(margin_requirement)),
            ADD CONSTRAINT custodial_position_cost_with_its_currency CHECK (
                    (cost_basis IS NULL) = (cost_basis_currency IS NULL)
                AND (average_cost IS NULL) = (average_cost_currency IS NULL)
                AND (margin_requirement IS NULL) = (margin_requirement_currency IS NULL));

        CREATE INDEX IF NOT EXISTS custodial_position_by_change
            ON custodial_position (sequence);
    END IF;
END $$;
