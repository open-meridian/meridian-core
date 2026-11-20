-- A month's table analysed once, by lake_row's owner, after the batch that
-- first fills it (step 3d; `lake_analyze_month` approved 2026-11-14).
--
-- A month's table has no statistics until autovacuum first analyses it,
-- a minute or more after its first rows. Until then every estimate the
-- planner makes of it clamps to one row, and an index that shares its
-- leading columns with the right one -- the subject index
-- (dataset, data_type, subject, sequence) beside (dataset, sequence) or
-- (dataset, data_type, row_key, version) -- costs the same: it was taken,
-- and a lookup of one row scanned the dataset's every row in the month,
-- once for each row looked up (a page of 500 places in seconds; the CI run
-- 43560029609 burst past the pool's wait). Analysed once, the month's
-- columns price each index as it is; autovacuum keeps them from then on.
--
-- Its owner's rights, for that narrow job alone, as migration 23's:
--   * the partition is held to its shape by lake_partition_checked and the
--     month to YYYY-MM, before anything is named;
--   * it names only `lake_row_<16 hex>_<YYYYMM>`, quoted with format('%I'),
--     and only a table that is one of lake_row's months;
--   * it analyses only a table never analysed (reltuples < 0) holding a
--     row, so asked again it does nothing, and answers whether it did.
-- Execute is the serving role's alone; PUBLIC's is revoked.

CREATE OR REPLACE FUNCTION lake_analyze_month(p_partition text, p_month text) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE
    r text := lake_partition_checked(p_partition);
    found text[];
    t text;
    rel oid;
    filled boolean;
BEGIN
    found := regexp_match(p_month, '^([0-9]{4})-(0[1-9]|1[0-2])$');
    IF found IS NULL OR found[1]::int NOT BETWEEN 1677 AND 2262 THEN
        RAISE EXCEPTION 'not a month of the lake''s valid times: %', p_month USING ERRCODE = '22023';
    END IF;
    t := r || '_' || found[1] || found[2];
    rel := to_regclass(format('%I', t));
    IF rel IS NULL OR NOT EXISTS (
        SELECT 1 FROM pg_inherits
         WHERE inhrelid = rel AND inhparent = to_regclass(format('%I', r))) THEN
        RETURN false;
    END IF;
    IF (SELECT reltuples FROM pg_class WHERE oid = rel) >= 0 THEN
        RETURN false;
    END IF;
    EXECUTE format('SELECT EXISTS (SELECT 1 FROM %I)', t) INTO filled;
    IF NOT filled THEN
        RETURN false;
    END IF;
    EXECUTE format('ANALYZE %I', t);
    RETURN true;
END
$$;

REVOKE ALL ON FUNCTION lake_analyze_month(text, text) FROM PUBLIC;
