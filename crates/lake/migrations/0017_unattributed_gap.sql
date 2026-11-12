-- The unattributed gap (contract v30, stage C2; the resilience spec's
-- section 4, "A gap is found when the stream skips", approved 2026-11-11).
-- The lake's writer records a gap the moment it finds the write-ahead
-- stream's sequence past rows it never committed, its datasets not yet known
-- (DatasetGap.dataset empty), and the admin hears of it at once. Each
-- dataset's own gap, found as its next write arrives, names that gap
-- (DatasetGap.found_in_gap_id): the run keeps its ID here, so a writer that
-- starts -- a takeover, a restart -- names it too.
--
-- Empty for a run found before this: its datasets' gaps name none. Written in
-- `ha` alone; a release before this reads and writes what it did.

ALTER TABLE lake_write_ahead_dropped
    ADD COLUMN IF NOT EXISTS gap_id text NOT NULL DEFAULT '';
