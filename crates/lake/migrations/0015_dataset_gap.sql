-- A dataset's gaps (contract v30, W10.14; the resilience spec's section 4).
-- The write-ahead stream holds what the lake has not recorded within its
-- bound; writes it drops before the lake takes them -- a writer gone longer
-- than the bound -- leave a gap in each dataset they were of.
--
-- lake_write_ahead_dropped: each run of the stream's sequences it dropped,
-- as the writer found them, with the stream's time of the last write the
-- lake had before them and of the first it took after. A later write whose
-- sidecar names one of them as the write it sent before is the first of its
-- dataset after a gap.
--
-- lake_write_ahead_dataset: each dataset's latest valid time recorded from
-- the stream: where a gap of it begins.
--
-- lake_dataset_gap: each gap's every state, its own row a change
-- (decisions/031) -- refetching, recovered, unrecovered -- the DatasetGap as
-- recorded in its body; its control change is in lake_control_change. Read
-- by ListDatasetGaps, latest state per gap.
--
-- Written in `ha` alone; a release before this reads and writes what it did.

CREATE TABLE IF NOT EXISTS lake_write_ahead_dropped (
    stream         text   NOT NULL,
    first_sequence bigint NOT NULL,
    last_sequence  bigint NOT NULL,
    after_at_ns    bigint NOT NULL,
    until_at_ns    bigint NOT NULL,
    noted_at_ns    bigint NOT NULL,
    PRIMARY KEY (stream, first_sequence)
);

CREATE TABLE IF NOT EXISTS lake_write_ahead_dataset (
    stream   text   NOT NULL,
    dataset  text   NOT NULL,
    valid_ns bigint NOT NULL,
    PRIMARY KEY (stream, dataset)
);

CREATE TABLE IF NOT EXISTS lake_dataset_gap (
    change_id      bigserial PRIMARY KEY,
    gap_id         text      NOT NULL,
    dataset        text      NOT NULL,
    state          smallint  NOT NULL,
    recorded_at_ns bigint    NOT NULL,
    body           bytea     NOT NULL
);

CREATE INDEX IF NOT EXISTS lake_dataset_gap_by_gap ON lake_dataset_gap (gap_id, change_id);
