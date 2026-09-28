-- The account side fits every venue (spec/the-account-side-fits-every-venue).
--
-- Four things, each so that what a venue said is kept as it said it:
--
-- A holding and a custodial position have a side, long or short, and the
-- position is keyed by account, instrument and side, because a venue can
-- report an account's long and short of one instrument at once and two rows
-- keyed without the side overwrote each other. An existing row was stated
-- with its sign as the only mark of its side, so its side is read from it:
-- negative is short, which is what the contract said a negative meant.
--
-- A settle-date quantity where the venue reports one, NULL where it does not.
--
-- A market value is NULL where the venue reported none, never zero. Before
-- this, an unset value was kept as a zero in no currency -- an empty currency
-- is how it can be told from a zero the venue did say -- and is made NULL
-- here, which is what it meant. The amount and its currency are NULL together.
--
-- Whether the currency was the connector's stated assumption rather than the
-- venue's; and on a statement, the account's buying power and margin figures
-- as reported, each NULL where the venue gave none.
--
-- Whether a position's value is also in the account's cash as the venue
-- reports it (a money-market fund SnapTrade counts in cash), on the row and
-- the position, so whoever loads the book counts it once. Nothing before this
-- was marked, so nothing existing is.
--
-- Guarded per table, as migrations 2 and 3 are: a database adopted from
-- before the history may not have every table, and a migration that finds
-- nothing to do must touch nothing, since ALTER TABLE locks even then.

DO $$
DECLARE
    target text;
    primary_key text;
BEGIN
    FOREACH target IN ARRAY ARRAY['holding', 'custodial_position'] LOOP
        IF EXISTS (SELECT 1 FROM information_schema.tables
                    WHERE table_schema = current_schema() AND table_name = target)
           AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                            WHERE table_schema = current_schema()
                              AND table_name = target AND column_name = 'side') THEN
            EXECUTE format(
                'ALTER TABLE %I
                     ADD COLUMN side text,
                     ADD COLUMN settle_date_quantity numeric,
                     ADD COLUMN also_counted_in_cash boolean NOT NULL DEFAULT false,
                     ALTER COLUMN market_value DROP NOT NULL,
                     ALTER COLUMN currency DROP NOT NULL',
                target);
            EXECUTE format(
                'UPDATE %I SET side = CASE WHEN quantity < 0 THEN ''short'' ELSE ''long'' END',
                target);
            EXECUTE format(
                'UPDATE %I SET market_value = NULL, currency = NULL
                  WHERE market_value = 0 AND currency = ''''',
                target);
            EXECUTE format(
                'ALTER TABLE %I
                     ALTER COLUMN side SET NOT NULL,
                     ADD CONSTRAINT %I CHECK (side IN (''long'', ''short'')),
                     ADD CONSTRAINT %I CHECK ((side = ''long'' AND quantity >= 0)
                                           OR (side = ''short'' AND quantity <= 0)),
                     ADD CONSTRAINT %I CHECK ((market_value IS NULL) = (currency IS NULL)),
                     ADD CONSTRAINT %I CHECK (scale(settle_date_quantity) <= 18
                         AND abs(settle_date_quantity)
                             < power(10::numeric, 38 - scale(settle_date_quantity)))',
                target, target || '_side', target || '_quantity_matches_side',
                target || '_value_with_its_currency',
                target || '_settle_date_quantity_on_the_wire');
        END IF;
    END LOOP;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'holding')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'holding' AND column_name = 'currency_assumed') THEN
        ALTER TABLE holding ADD COLUMN currency_assumed boolean NOT NULL DEFAULT false;
    END IF;

    -- The key. Found by what it is rather than by name, because a database
    -- whose table was renamed from `position` (migration 2) still calls it
    -- `position_pkey`.
    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'custodial_position') THEN
        SELECT c.conname INTO primary_key
          FROM pg_constraint c
          JOIN pg_class t ON t.oid = c.conrelid
          JOIN pg_namespace n ON n.oid = t.relnamespace
         WHERE n.nspname = current_schema() AND t.relname = 'custodial_position'
           AND c.contype = 'p'
           AND array_length(c.conkey, 1) = 2;
        IF primary_key IS NOT NULL THEN
            EXECUTE format('ALTER TABLE custodial_position DROP CONSTRAINT %I', primary_key);
            ALTER TABLE custodial_position ADD PRIMARY KEY (account_id, instrument_id, side);
        END IF;
    END IF;

    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'statement')
       AND NOT EXISTS (SELECT 1 FROM information_schema.columns
                        WHERE table_schema = current_schema()
                          AND table_name = 'statement' AND column_name = 'buying_power') THEN
        ALTER TABLE statement
            ADD COLUMN buying_power numeric,
            ADD COLUMN buying_power_currency text,
            ADD COLUMN margin_requirement numeric,
            ADD COLUMN margin_requirement_currency text,
            ADD COLUMN maintenance_excess numeric,
            ADD COLUMN maintenance_excess_currency text,
            ADD COLUMN currency_assumed boolean NOT NULL DEFAULT false,
            ADD CONSTRAINT statement_buying_power_with_its_currency
                CHECK ((buying_power IS NULL) = (buying_power_currency IS NULL)),
            ADD CONSTRAINT statement_margin_requirement_with_its_currency
                CHECK ((margin_requirement IS NULL) = (margin_requirement_currency IS NULL)),
            ADD CONSTRAINT statement_maintenance_excess_with_its_currency
                CHECK ((maintenance_excess IS NULL) = (maintenance_excess_currency IS NULL)),
            ADD CONSTRAINT statement_figures_on_the_wire CHECK (
                    (buying_power IS NULL OR (scale(buying_power) <= 18
                        AND abs(buying_power) < power(10::numeric, 38 - scale(buying_power))))
                AND (margin_requirement IS NULL OR (scale(margin_requirement) <= 18
                        AND abs(margin_requirement)
                            < power(10::numeric, 38 - scale(margin_requirement))))
                AND (maintenance_excess IS NULL OR (scale(maintenance_excess) <= 18
                        AND abs(maintenance_excess)
                            < power(10::numeric, 38 - scale(maintenance_excess)))));
    END IF;
END $$;
