-- What the pre-migration mechanism had already applied by hand.
--
-- A database created before this file exists in one of two states: made before
-- the rename and the two added columns, or made after them by the guarded
-- lists that used to live in postgres.rs. This brings both to the same place
-- and does nothing to a database that is already there.
--
-- Guarded rather than unconditional, and the guard is the point rather than an
-- optimisation: ALTER TABLE takes an exclusive lock even when it has nothing
-- to do, which blocks live traffic and can deadlock against a transaction
-- holding a row lock. A migration that finds nothing to do must touch nothing.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'position')
       AND NOT EXISTS (SELECT 1 FROM information_schema.tables
                        WHERE table_schema = current_schema()
                          AND table_name = 'custodial_position') THEN
        ALTER TABLE position RENAME TO custodial_position;
    END IF;
END $$;

-- The table guard is here because adoption may bring a database that predates
-- the history and does not have every table: altering one that is not there
-- fails the migration and leaves the rest unapplied.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM information_schema.tables
                    WHERE table_schema = current_schema() AND table_name = 'statement') THEN
        RETURN;
    END IF;

    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                    WHERE table_schema = current_schema()
                      AND table_name = 'statement' AND column_name = 'expected_rows') THEN
        ALTER TABLE statement ADD COLUMN expected_rows integer NOT NULL DEFAULT 0;
    END IF;

    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                    WHERE table_schema = current_schema()
                      AND table_name = 'statement' AND column_name = 'completed_at_ns') THEN
        ALTER TABLE statement ADD COLUMN completed_at_ns bigint;
    END IF;
END $$;
