-- What of a holding cannot move, as the source reports it, never derived
-- (contract v8: W2.3; the product owner, 2026-10-01, "Accept recommendation",
-- meridian-design reference/encumbrance-survey). A holding row gains its
-- available and not-available quantities and what the available figure is net
-- of; its encumbered sub-balances are a table beside it, as its lots are. A
-- custodial position carries them from the row that last stated it, as it
-- carries the lots, so the position table gains nothing. A statement gains the
-- account servicer's security interest, and a collateral balance whether the
-- receiver may reuse it, each as reported and NULL where not.
--
-- Guarded, as migrations 2 to 5 are: a migration that finds nothing to do
-- touches nothing.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'holding')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'holding' AND column_name = 'available_quantity') THEN
        ALTER TABLE holding
            ADD COLUMN available_quantity numeric,
            ADD COLUMN not_available_quantity numeric,
            ADD COLUMN available_basis integer NOT NULL DEFAULT 0,
            ADD CONSTRAINT holding_available_on_the_wire CHECK (
                street_on_the_wire(available_quantity)
                AND street_on_the_wire(not_available_quantity));
    END IF;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'statement')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'statement' AND column_name = 'security_interest') THEN
        ALTER TABLE statement ADD COLUMN security_interest boolean;
    END IF;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'statement_collateral')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'statement_collateral'
                          AND column_name = 'reusable') THEN
        ALTER TABLE statement_collateral ADD COLUMN reusable boolean;
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS holding_encumbrance (
    holding_id  text    NOT NULL,
    ordinal     integer NOT NULL,
    kind        integer NOT NULL CHECK (kind > 0),
    quantity    numeric NOT NULL,
    available   boolean,
    source_code text    NOT NULL DEFAULT '',
    pledgee     text    NOT NULL DEFAULT '',
    held_at     text    NOT NULL DEFAULT '',
    segment     text    NOT NULL DEFAULT '',
    detail      text    NOT NULL DEFAULT '',
    PRIMARY KEY (holding_id, ordinal),
    CONSTRAINT holding_encumbrance_on_the_wire CHECK (street_on_the_wire(quantity))
);
