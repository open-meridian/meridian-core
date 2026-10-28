-- The datasets' listing's counts served by indexes (ruling 816, found on dev
-- 2026-10-28): one list-datasets took 1,068 ms. Its two counts, a dataset's
-- rows carrying a value as reported and those flagged on a closed day, each
-- read every row the lake keeps -- a chain's quotes and statistics, hundreds
-- of thousands of them -- to count the few flagged.
--
-- lake_row_unconverted holds only the rows carrying a value as reported,
-- and lake_row_closed only those recorded flagged, so each count reads the
-- rows it counts. A row recorded unflagged and flagged later by a re-check
-- is found from its amendment in lake_closed_day, by the row's key.
--
-- Indexes only: nothing recorded changes, and a release before this reads
-- the same rows.

CREATE INDEX IF NOT EXISTS lake_row_unconverted
    ON lake_row (dataset) WHERE unconverted;
CREATE INDEX IF NOT EXISTS lake_row_closed
    ON lake_row (dataset, sequence) WHERE closed_reason <> 0;
