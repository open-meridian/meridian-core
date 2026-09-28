-- Quantities and values as exact decimals at the scale they were stated with.
--
-- decisions/023: a quantity is an integer that carries its own scale, 0 to 18
-- decimal places and at most 38 digits, where it was an int64 at a fixed 1e8.
-- `numeric` with no precision given is that in a column: it keeps a value
-- exactly and at its own scale, 1.50 as 1.50, and compares it as a number, so
-- 1.50 = 1.5. A bigint scaled by 1e8 could not hold Alpaca's ninth decimal
-- place or a cheap token's hundred-billion-unit holding; this holds both.
--
-- An existing row was stated at eight places, the only scale there was, and
-- is carried at eight: 1250000000 becomes 12.50000000, which is what it said.
-- Multiplying by 0.00000001 gives exactly that scale, where dividing would
-- give whatever scale division chose.
--
-- The checks refuse what the wire could not have carried, whoever writes it,
-- as the code does before it gets here. `power(10::numeric, ...)` is numeric
-- arithmetic; `10 ^ n` would be double precision, which is exactly what no
-- quantity may pass through.
--
-- Guarded per table, as migration 2 is: a database adopted from before the
-- history may not have every table, and one it has may already be here.

DO $$
DECLARE
    target text;
BEGIN
    FOREACH target IN ARRAY ARRAY['holding', 'custodial_position'] LOOP
        IF EXISTS (SELECT 1 FROM information_schema.columns
                    WHERE table_schema = current_schema()
                      AND table_name = target AND column_name = 'quantity_scaled') THEN
            EXECUTE format(
                'ALTER TABLE %I
                     ALTER COLUMN quantity_scaled TYPE numeric USING quantity_scaled * 0.00000001,
                     ALTER COLUMN value_scaled TYPE numeric USING value_scaled * 0.00000001',
                target);
            EXECUTE format('ALTER TABLE %I RENAME COLUMN quantity_scaled TO quantity', target);
            EXECUTE format('ALTER TABLE %I RENAME COLUMN value_scaled TO market_value', target);
            EXECUTE format(
                'ALTER TABLE %I
                     ADD CONSTRAINT %I CHECK (scale(quantity) <= 18
                         AND abs(quantity) < power(10::numeric, 38 - scale(quantity))),
                     ADD CONSTRAINT %I CHECK (scale(market_value) <= 18
                         AND abs(market_value) < power(10::numeric, 38 - scale(market_value)))',
                target, target || '_quantity_on_the_wire', target || '_market_value_on_the_wire');
        END IF;
    END LOOP;
END $$;
