-- The closed-day flag (W10.4, W10.9, contract v23; the lake plan's "vL1c",
-- items 8 to 13; sdk-contract/venue-originated-fills-and-the-lakes-outcomes-contract).
--
-- A daily value dated on a closed day of its dataset's calendar is recorded
-- as reported, its ObservationMeta.closed_day set by the lake in the row's
-- body; `closed_reason` keeps the flag's reason beside it for the Data
-- sources page's counts (DateCheckReason: 0 none, 1 weekend, 2 holiday, 3
-- outside coverage). A row is never updated: a later calendar version's
-- re-check is an amendment beside the row, its own record (decisions/031),
-- never a new version, and a read as of a recorded time takes the latest
-- amendment at or before it.

ALTER TABLE lake_row ADD COLUMN IF NOT EXISTS closed_reason smallint NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS lake_closed_day (
    amendment_id bigserial PRIMARY KEY,
    dataset      text     NOT NULL,
    sequence     bigint   NOT NULL,
    -- As closed_reason; 0 where the day is no longer closed.
    reason       smallint NOT NULL,
    -- The DateCheck, encoded; NULL where the day is no longer closed.
    closed_day   bytea,
    why          text     NOT NULL,
    at_ns        bigint   NOT NULL
);
CREATE INDEX IF NOT EXISTS lake_closed_day_by_row ON lake_closed_day (dataset, sequence);
