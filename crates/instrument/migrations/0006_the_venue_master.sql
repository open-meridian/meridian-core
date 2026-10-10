-- Contract v18 (W3.5, W3.14; plans/the-lake-prices-the-book, ruling 2 of
-- 2026-10-09): the venues the deployment pulled from the platform's venue
-- master, kept at the platform's version, and the venue an instrument is
-- listed on, a venue master ID. A deployment mints no venue: every row here
-- is one the platform's answer carried.
--
-- Additions only, `IF NOT EXISTS` throughout, so it is run again harmlessly.

ALTER TABLE instrument
    ADD COLUMN IF NOT EXISTS listing_venue_id text NOT NULL DEFAULT '';

CREATE TABLE IF NOT EXISTS instrument_venue (
    venue_id    text   PRIMARY KEY CHECK (venue_id LIKE 'VEN-%'),
    version     bigint NOT NULL,
    -- meridian.v1.VenueRecord, as the platform answered it.
    record      bytea  NOT NULL,
    kept_at_ns  bigint NOT NULL
);
