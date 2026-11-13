-- lake_row's partitions made by the role that owns lake_row (step 3d;
-- spec/a-deployment-scales-out-and-fails-over-by-configuration section 6.2).
-- The lake serves under a login that may read and write what the migration
-- made, never create in the schema. Yet a range's table and each month's are
-- made as the writer first needs them. So these functions, run with their
-- owner's rights, make exactly those tables and nothing else: a range's,
-- named from a digest of its ID; a month's, attached under bounds checked to
-- be whole numbers or MINVALUE/MAXVALUE; and the table kept before 3d,
-- dropped once it is empty. Execute is the serving role's alone, granted with
-- the rest of what it may use.

-- A range's table: `lake_row_` and the first eight bytes of SHA-256 of its
-- ID in hex (meridian_lake::leaves::range_table), LIST-attached to lake_row
-- for that ID. False where it is made already.
CREATE OR REPLACE FUNCTION lake_make_range(p_partition text) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE
    t text := 'lake_row_' || substr(encode(sha256(convert_to(p_partition, 'UTF8')), 'hex'), 1, 16);
BEGIN
    IF p_partition IS NULL OR p_partition = '' THEN
        RAISE EXCEPTION 'a range is named';
    END IF;
    IF to_regclass(t) IS NOT NULL THEN
        RETURN false;
    END IF;
    EXECUTE format('CREATE TABLE %I (LIKE lake_row INCLUDING DEFAULTS) PARTITION BY RANGE (valid_from_ns)', t);
    EXECUTE format('ALTER TABLE lake_row ATTACH PARTITION %I FOR VALUES IN (%L)', t, p_partition);
    RETURN true;
END
$$;

-- A month of a range's table: `<range table>_<tag>` (tag YYYYMM, or nYYYYMM
-- before the common era), made empty, given its indexes while empty, then
-- attached FROM p_from TO p_to. False where it is made already.
CREATE OR REPLACE FUNCTION lake_make_month(p_partition text, p_tag text, p_from text, p_to text)
RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE
    r text := 'lake_row_' || substr(encode(sha256(convert_to(p_partition, 'UTF8')), 'hex'), 1, 16);
    t text;
BEGIN
    IF p_tag !~ '^n?[0-9]{6,8}$' THEN
        RAISE EXCEPTION 'a month is YYYYMM: %', p_tag;
    END IF;
    IF p_from !~ '^(MINVALUE|-?[0-9]{1,19})$' OR p_to !~ '^(MAXVALUE|-?[0-9]{1,19})$' THEN
        RAISE EXCEPTION 'a month''s bounds are whole numbers: % to %', p_from, p_to;
    END IF;
    IF to_regclass(r) IS NULL THEN
        RAISE EXCEPTION 'range % has no table yet', p_partition;
    END IF;
    t := r || '_' || p_tag;
    IF to_regclass(t) IS NOT NULL THEN
        RETURN false;
    END IF;
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
    EXECUTE format('CREATE INDEX %I ON %I (dataset, data_type, (subject || ''''), sequence)',
                   t || '_subject_sequence', t);
    EXECUTE format('ALTER TABLE %I ATTACH PARTITION %I FOR VALUES FROM (%s) TO (%s)', r, t, p_from, p_to);
    RETURN true;
END
$$;

-- The table kept before 3d let go once its every row is copied into its
-- months: detached from lake_row and dropped. False where it is gone
-- already; refused while it holds a row.
CREATE OR REPLACE FUNCTION lake_drop_legacy() RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
BEGIN
    IF to_regclass('lake_row_legacy') IS NULL THEN
        RETURN false;
    END IF;
    IF EXISTS (SELECT 1 FROM lake_row_legacy) THEN
        RAISE EXCEPTION 'lake_row_legacy still holds rows';
    END IF;
    ALTER TABLE lake_row DETACH PARTITION lake_row_legacy;
    DROP TABLE lake_row_legacy;
    RETURN true;
END
$$;

REVOKE ALL ON FUNCTION lake_make_range(text) FROM PUBLIC;
REVOKE ALL ON FUNCTION lake_make_month(text, text, text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION lake_drop_legacy() FROM PUBLIC;
