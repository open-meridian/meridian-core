-- The edge keeps its own (contract v11: W2.2 to W2.4; spec/vendor-
-- differences-have-a-place-in-the-contract, slice A; sdk-contract/the-street-
-- counts-each-asset-once). A holding row and a statement reference the raw
-- record they were converted from, in the plugin's own storage, and carry the
-- provenance of each value the plugin closed rather than read; a holding row
-- carries its quantities pending by value date. A custodial position carries
-- them from the row that last stated it, as it carries the lots, so the
-- position table gains nothing.
--
-- A backfill is an amendment journaled beside the row as first recorded,
-- never an overwrite: the row's own columns and child rows stay as first
-- recorded, and what a backfill added is a holding_amendment row, its pending
-- and provenance rows marked with the version that added the field. One per
-- row, version and field, so a backfill run twice adds nothing.
--
-- Guarded, as migrations 2 to 6 are: a migration that finds nothing to do
-- touches nothing.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'holding')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'holding' AND column_name = 'raw_instance') THEN
        ALTER TABLE holding
            ADD COLUMN raw_instance text NOT NULL DEFAULT '',
            ADD COLUMN raw_key text NOT NULL DEFAULT '';
    END IF;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'statement')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'statement' AND column_name = 'raw_instance') THEN
        ALTER TABLE statement
            ADD COLUMN raw_instance text NOT NULL DEFAULT '',
            ADD COLUMN raw_key text NOT NULL DEFAULT '';
    END IF;
END $$;

-- Each value the plugin closed, by its path in the row. `amended_in` is the
-- contract version of the backfill that added it, empty as first recorded.
CREATE TABLE IF NOT EXISTS holding_provenance (
    holding_id   text    NOT NULL,
    ordinal      integer NOT NULL,
    field        text    NOT NULL,
    kind         integer NOT NULL CHECK (kind > 0),
    raw_instance text    NOT NULL DEFAULT '',
    raw_key      text    NOT NULL DEFAULT '',
    source       text    NOT NULL DEFAULT '',
    person       text    NOT NULL DEFAULT '',
    rule         text    NOT NULL DEFAULT '',
    amended_in   text    NOT NULL DEFAULT '',
    PRIMARY KEY (holding_id, amended_in, ordinal)
);

CREATE TABLE IF NOT EXISTS holding_pending (
    holding_id  text    NOT NULL,
    ordinal     integer NOT NULL,
    value_date  text    NOT NULL,
    quantity    numeric NOT NULL,
    amended_in  text    NOT NULL DEFAULT '',
    PRIMARY KEY (holding_id, amended_in, ordinal),
    CONSTRAINT holding_pending_on_the_wire CHECK (street_on_the_wire(quantity))
);

CREATE TABLE IF NOT EXISTS statement_provenance (
    statement_id text    NOT NULL,
    ordinal      integer NOT NULL,
    field        text    NOT NULL,
    kind         integer NOT NULL CHECK (kind > 0),
    raw_instance text    NOT NULL DEFAULT '',
    raw_key      text    NOT NULL DEFAULT '',
    source       text    NOT NULL DEFAULT '',
    person       text    NOT NULL DEFAULT '',
    rule         text    NOT NULL DEFAULT '',
    PRIMARY KEY (statement_id, ordinal)
);

-- A backfill's amendment: the row, the field and the version that added it
-- (its cause), the raw record it was re-converted from, and who sent it when.
CREATE TABLE IF NOT EXISTS holding_amendment (
    holding_id               text   NOT NULL,
    contract_version         text   NOT NULL,
    field                    text   NOT NULL,
    raw_instance             text   NOT NULL DEFAULT '',
    raw_key                  text   NOT NULL DEFAULT '',
    cause_instance_id        text   NOT NULL DEFAULT '',
    cause_acting_for_subject text   NOT NULL DEFAULT '',
    cause_correlation_id     text   NOT NULL DEFAULT '',
    cause_causation_id       text   NOT NULL DEFAULT '',
    committed_at_ns          bigint NOT NULL,
    PRIMARY KEY (holding_id, contract_version, field)
);
