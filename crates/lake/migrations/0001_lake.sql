-- The lake (contract v18, W10; spec/the-lake; plans/the-lake-prices-the-book,
-- the lake's 1a): append-only and bitemporal, one partition per dataset.
--
-- A row is never updated: a correction is a new version of its row key,
-- recorded later. Each dataset's rows are numbered from its partition's head
-- in the batch's own transaction (`lake_partition`), so there are no holes,
-- and each names the previous sequence of its subject. Only retention under a
-- dataset's licence removes rows, and each removal is recorded. Every
-- priority and want change is its own record (decisions/031); the lake is new
-- and owes no backfill. Nothing here reaches the platform.

CREATE TABLE IF NOT EXISTS lake_partition (
    dataset  text   PRIMARY KEY CHECK (dataset <> ''),
    head     bigint NOT NULL CHECK (head >= 0)
);

CREATE TABLE IF NOT EXISTS lake_row (
    dataset           text     NOT NULL,
    sequence          bigint   NOT NULL CHECK (sequence > 0),
    -- 1 a price, 2 a bar.
    data_type         smallint NOT NULL,
    row_key           text     NOT NULL CHECK (row_key <> ''),
    version           bigint   NOT NULL CHECK (version > 0),
    -- The first subject, which a subject's previous sequence is kept by; and
    -- every subject the row names.
    subject           text     NOT NULL,
    subjects          text[]   NOT NULL,
    -- A price's kind; 0 for a bar. A bar's length; 0 for a price.
    kind              smallint NOT NULL,
    interval_ns       bigint   NOT NULL,
    venue_id          text     NOT NULL,
    valid_from_ns     bigint   NOT NULL,
    valid_until_ns    bigint   NOT NULL,
    -- A plain date, typed (ruling 3 of 2026-10-09); NULL for a value that is
    -- not daily.
    business_date     date,
    recorded_at_ns    bigint   NOT NULL,
    previous_sequence bigint   NOT NULL,
    -- Whether a value failed conversion and is kept as reported beside it.
    unconverted       boolean  NOT NULL,
    -- meridian.v1.Price or meridian.v1.Bar, as recorded, its envelope whole.
    body              bytea    NOT NULL,
    PRIMARY KEY (dataset, sequence)
);
CREATE UNIQUE INDEX IF NOT EXISTS lake_row_version
    ON lake_row (dataset, data_type, row_key, version);
CREATE INDEX IF NOT EXISTS lake_row_by_subject
    ON lake_row (subject, data_type, dataset, valid_from_ns);
CREATE INDEX IF NOT EXISTS lake_row_by_subjects
    ON lake_row USING gin (subjects);

-- The deployment's priority for a data type and price kind, replaced whole,
-- each change journalled with who, through what, when and why.
CREATE TABLE IF NOT EXISTS lake_priority_change (
    change_id     bigserial PRIMARY KEY,
    data_type     text      NOT NULL,
    kind          integer   NOT NULL,
    -- meridian.v1.SourcePriority, as set.
    priority      bytea     NOT NULL,
    changed_at_ns bigint    NOT NULL
);

-- Each want asked, answered, declined or withdrawn (W10.7).
CREATE TABLE IF NOT EXISTS lake_want_change (
    change_id  bigserial PRIMARY KEY,
    want_id    text      NOT NULL,
    dataset    text      NOT NULL,
    change     smallint  NOT NULL,
    subjects   text[]    NOT NULL,
    reason     integer   NOT NULL,
    by_whom    text[]    NOT NULL,
    -- meridian.v1.ObservationsWantedEvent, on an ask.
    want       bytea,
    at_ns      bigint    NOT NULL
);

-- That a dataset whose licence forbids keeping it was served: what, to
-- whom, when. Never its values.
CREATE TABLE IF NOT EXISTS lake_served (
    served_id      bigserial PRIMARY KEY,
    dataset        text      NOT NULL,
    first_sequence bigint    NOT NULL,
    last_sequence  bigint    NOT NULL,
    subjects       text[]    NOT NULL,
    fields         text[]    NOT NULL,
    readers        text[]    NOT NULL,
    at_ns          bigint    NOT NULL
);

-- Rows removed by retention under a dataset's licence.
CREATE TABLE IF NOT EXISTS lake_removal (
    removal_id         bigserial PRIMARY KEY,
    dataset            text      NOT NULL,
    rows_removed       bigint    NOT NULL,
    recorded_before_ns bigint    NOT NULL,
    why                text      NOT NULL,
    at_ns              bigint    NOT NULL
);

-- The misses each instance reported (W3.2, W3.15), counted for the Data
-- sources page.
CREATE TABLE IF NOT EXISTS lake_miss (
    instance    text   PRIMARY KEY,
    misses      bigint NOT NULL,
    last_at_ns  bigint NOT NULL
);

-- A merged record followed by alias at read (W3.8, W10.8): a read for the
-- record that stays answers the rows recorded under the one it replaced,
-- each still naming the record it was recorded against.
CREATE TABLE IF NOT EXISTS lake_alias (
    replaced     text   PRIMARY KEY,
    stays        text   NOT NULL,
    heard_at_ns  bigint NOT NULL
);

-- The data configuration as last heard from the conductor.
CREATE TABLE IF NOT EXISTS lake_configuration (
    one          boolean PRIMARY KEY DEFAULT true CHECK (one),
    event        bytea   NOT NULL,
    heard_at_ns  bigint  NOT NULL
);
