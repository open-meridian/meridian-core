-- lake_row's partitions made by the role that owns lake_row (step 3d;
-- spec/a-deployment-scales-out-and-fails-over-by-configuration section 6.2).
-- The lake serves under a login that may read and write what the migration
-- made, never create in the schema. Yet a range's table and each month's are
-- made as the writer first needs them. So these functions, run with their
-- owner's rights, do that narrow job and nothing else:
--   * every argument is held to its shape before anything is made: a
--     partition is a dataset's ID (letters, digits, `_ . : -`, at most 255)
--     with an optional range `/<lo>-<hi>` (lowercase 4-hex, lo <= hi); a
--     month is YYYY-MM within the 64-bit nanosecond range;
--   * each name is built from a digest or a checked month and quoted with
--     format('%I'); each value with format('%L'); the month's bounds are
--     computed here, never taken from the caller;
--   * they make only `lake_row_<16 hex>` (a range's) and
--     `lake_row_<16 hex>_<YYYYMM>` (its month's) in the schema the
--     migration ran in, attached to lake_row and to that range alone, and
--     drop only lake_row_legacy, once empty.
-- Execute is the serving role's alone, granted with the rest of what it may
-- use; PUBLIC's is revoked.

-- A partition's ID held to its shape, and its range table's name; refused
-- otherwise.
CREATE OR REPLACE FUNCTION lake_partition_checked(p_partition text) RETURNS text
LANGUAGE plpgsql IMMUTABLE SET search_path FROM CURRENT AS $$
DECLARE
    found text[];
BEGIN
    found := regexp_match(p_partition,
        '^([A-Za-z0-9][A-Za-z0-9_.:-]{0,254})(/([0-9a-f]{4})-([0-9a-f]{4}))?$');
    IF found IS NULL THEN
        RAISE EXCEPTION 'not a lake partition: %', p_partition USING ERRCODE = '22023';
    END IF;
    IF found[2] IS NOT NULL AND ('x' || found[3])::bit(16)::int > ('x' || found[4])::bit(16)::int THEN
        RAISE EXCEPTION 'a range''s low end is above its high: %', p_partition USING ERRCODE = '22023';
    END IF;
    RETURN 'lake_row_' || substr(encode(sha256(convert_to(p_partition, 'UTF8')), 'hex'), 1, 16);
END
$$;

-- A range's table: `lake_row_` and the first eight bytes of SHA-256 of its
-- ID in hex (meridian_lake::leaves::range_table), LIST-attached to lake_row
-- for that ID. False where it is made already.
CREATE OR REPLACE FUNCTION lake_make_range(p_partition text) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE
    t text := lake_partition_checked(p_partition);
BEGIN
    IF to_regclass(format('%I', t)) IS NOT NULL THEN
        RETURN false;
    END IF;
    EXECUTE format('CREATE TABLE %I (LIKE lake_row INCLUDING DEFAULTS) PARTITION BY RANGE (valid_from_ns)', t);
    EXECUTE format('ALTER TABLE lake_row ATTACH PARTITION %I FOR VALUES IN (%L)', t, p_partition);
    RETURN true;
END
$$;

-- A month of a range's table: `<range table>_<YYYYMM>`, made empty, given its
-- indexes while empty, then attached for that month's valid times (UTC), its
-- bounds computed here. False where it is made already.
CREATE OR REPLACE FUNCTION lake_make_month(p_partition text, p_month text) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE
    r text := lake_partition_checked(p_partition);
    found text[];
    starts timestamptz;
    from_ns numeric;
    to_ns numeric;
    t text;
BEGIN
    found := regexp_match(p_month, '^([0-9]{4})-(0[1-9]|1[0-2])$');
    IF found IS NULL OR found[1]::int NOT BETWEEN 1677 AND 2262 THEN
        RAISE EXCEPTION 'not a month of the lake''s valid times: %', p_month USING ERRCODE = '22023';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_inherits
                    WHERE inhrelid = to_regclass(format('%I', r))
                      AND inhparent = 'lake_row'::regclass) THEN
        RAISE EXCEPTION 'range % has no table of lake_row''s yet', p_partition USING ERRCODE = '22023';
    END IF;
    t := r || '_' || found[1] || found[2];
    IF to_regclass(format('%I', t)) IS NOT NULL THEN
        RETURN false;
    END IF;
    starts := make_timestamptz(found[1]::int, found[2]::int, 1, 0, 0, 0, 'UTC');
    from_ns := extract(epoch FROM starts) * 1000000000;
    to_ns := extract(epoch FROM starts + interval '1 month') * 1000000000;
    EXECUTE format('CREATE TABLE %I (LIKE lake_row INCLUDING DEFAULTS)', t);
    -- The indexes the table kept before 3d has, over its own rows.
    EXECUTE format('CREATE UNIQUE INDEX %I ON %I (dataset, sequence)', t || '_seq', t);
    EXECUTE format('CREATE UNIQUE INDEX %I ON %I (dataset, data_type, row_key, version)', t || '_version', t);
    EXECUTE format('CREATE INDEX %I ON %I USING gin (subjects)', t || '_subjects', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset, data_type, subject, kind, venue_id, interval_ns, '
                   'latest_key, valid_from_ns DESC, sequence DESC)', t || '_latest', t);
    EXECUTE format('CREATE INDEX %I ON %I USING gin ((subjects[2:]))', t || '_later', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset, sequence) WHERE latest_key IS NULL', t || '_unkeyed', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset) WHERE unconverted', t || '_unconverted', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset, sequence) WHERE closed_reason <> 0', t || '_closed', t);
    EXECUTE format('CREATE INDEX %I ON %I (subject, dataset, row_key, version DESC) '
                   'INCLUDE (expires, listed_type, latest_key, recorded_at_ns, valid_from_ns, quality, sequence) '
                   'WHERE data_type = 9', t || '_listing', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset, sequence) WHERE data_type = 9 AND listed_type IS NULL',
                   t || '_unlisted', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset, sequence) WHERE data_type = 6 AND published_at_ns IS NULL',
                   t || '_unpublished', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset, sequence) WHERE quality IN (2, 3)', t || '_quality', t);
    EXECUTE format('CREATE INDEX %I ON %I (dataset, data_type, (subject || %L), sequence)',
                   t || '_subject_sequence', t, '');
    EXECUTE format('ALTER TABLE %I ATTACH PARTITION %I FOR VALUES FROM (%s) TO (%s)', r, t,
                   CASE WHEN from_ns <= -9223372036854775808 THEN 'MINVALUE' ELSE from_ns::bigint::text END,
                   CASE WHEN to_ns > 9223372036854775807 THEN 'MAXVALUE' ELSE to_ns::bigint::text END);
    RETURN true;
END
$$;

-- The table kept before 3d let go once its every row is copied into its
-- months: detached from lake_row and dropped. False where it is gone
-- already; refused while it holds a row or is not lake_row's.
CREATE OR REPLACE FUNCTION lake_drop_legacy() RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
BEGIN
    IF to_regclass('lake_row_legacy') IS NULL THEN
        RETURN false;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_inherits
                    WHERE inhrelid = 'lake_row_legacy'::regclass AND inhparent = 'lake_row'::regclass) THEN
        RAISE EXCEPTION 'lake_row_legacy is not lake_row''s' USING ERRCODE = '55000';
    END IF;
    IF EXISTS (SELECT 1 FROM lake_row_legacy) THEN
        RAISE EXCEPTION 'lake_row_legacy still holds rows' USING ERRCODE = '55000';
    END IF;
    ALTER TABLE lake_row DETACH PARTITION lake_row_legacy;
    DROP TABLE lake_row_legacy;
    RETURN true;
END
$$;

REVOKE ALL ON FUNCTION lake_partition_checked(text) FROM PUBLIC;
REVOKE ALL ON FUNCTION lake_make_range(text) FROM PUBLIC;
REVOKE ALL ON FUNCTION lake_make_month(text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION lake_drop_legacy() FROM PUBLIC;
