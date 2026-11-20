-- lake_row partitioned online (step 3d; section 6.2; the benchmark batch of
-- 2026-11-10): LIST by partition, then RANGE by month of valid time. A row
-- key fixes its valid time (the lake's rule 5), so a row never moves across
-- months.
--
-- - The table kept so far becomes the default partition as it stands,
--   `lake_row_legacy`: no row is copied here, and the release before's
--   writes during a rolling upgrade, naming no range, still land in it.
-- - New rows go to their range's month at once: each range's and month's
--   partition is made by the writer as it first needs it, its indexes made
--   empty before it is attached.
-- - The lake's writer copies the default's rows behind, a 512 KiB page a
--   transaction, each page deleted from it and inserted into its range's
--   month in one statement, so every read answers each row exactly once,
--   before, during and after; then lets the empty default go.
ALTER TABLE lake_row RENAME TO lake_row_legacy;
CREATE TABLE lake_row (LIKE lake_row_legacy INCLUDING DEFAULTS) PARTITION BY LIST (partition);
-- A lake that kept no row yet has nothing to copy: no default partition,
-- whose lock each range's attaching would take.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM lake_row_legacy) THEN
        ALTER TABLE lake_row ATTACH PARTITION lake_row_legacy DEFAULT;
    ELSE
        DROP TABLE lake_row_legacy;
    END IF;
END $$;

-- The latest version of each row key (step 3d; section 6.2, `lake_latest`
-- approved 2026-11-12): kept in the record's own transaction, where the
-- write path finds what it decides a row against and the latest read
-- chooses from. One row a key, naming the range its latest version is in.
-- `force_*` is the version in force, a rate's by published time (ruling
-- 1006 (b)); every other data type's is its latest version.
CREATE TABLE IF NOT EXISTS lake_latest (
    dataset          text     NOT NULL,
    data_type        smallint NOT NULL,
    row_key          text     NOT NULL,
    -- The latest version: where it is, its number, its valid time and its
    -- key's anchor.
    partition        text     NOT NULL,
    sequence         bigint   NOT NULL,
    version          bigint   NOT NULL,
    valid_from_ns    bigint   NOT NULL,
    anchor           integer  NOT NULL,
    -- The version in force, and what the latest read chooses it by.
    force_partition  text     NOT NULL,
    force_sequence   bigint   NOT NULL,
    force_version    bigint   NOT NULL,
    force_published  bigint,
    force_valid_from bigint   NOT NULL,
    subject          text     NOT NULL,
    kind             smallint NOT NULL,
    venue_id         text     NOT NULL,
    interval_ns      bigint   NOT NULL,
    latest_key       text,
    quality          smallint,
    PRIMARY KEY (dataset, data_type, row_key)
);
-- A key's row found by its dataset and key alone: an escalation or a
-- re-check names no data type. The key leads, so the write path's probe of
-- each key it records is one look at it whatever the statistics say of its
-- dataset: a dataset they do not know yet looks empty, and by them every
-- index led by the dataset looked as good as another.
CREATE INDEX IF NOT EXISTS lake_latest_by_key ON lake_latest (row_key, dataset);
CREATE INDEX IF NOT EXISTS lake_latest_by_subject
    ON lake_latest (dataset, data_type, subject, kind, venue_id, interval_ns, latest_key,
                    force_valid_from DESC, force_sequence DESC);

-- Whether lake_latest holds every row key kept: from the start on a lake
-- with no rows; once the default's rows are copied behind on one with some.
CREATE TABLE IF NOT EXISTS lake_conversion (
    one        boolean PRIMARY KEY DEFAULT true CHECK (one),
    latest_all boolean NOT NULL
);
INSERT INTO lake_conversion (latest_all)
SELECT to_regclass('lake_row_legacy') IS NULL ON CONFLICT DO NOTHING;
