-- The replica's schema.
--
-- Two tables, because an instrument's identifiers are a set with dates on each
-- member and folding them into a column would make the dated lookup a scan.
--
-- Applied on start, idempotently. A replica is rebuilt from the platform on
-- demand, so there is no migration history to preserve and no state a fresh
-- schema would lose.

CREATE TABLE IF NOT EXISTS instrument (
    instrument_id   text PRIMARY KEY,
    asset_class     text   NOT NULL DEFAULT '',
    currency        text   NOT NULL DEFAULT '',
    exchange_mic    text   NOT NULL DEFAULT '',
    description     text   NOT NULL DEFAULT '',
    lifecycle_state text   NOT NULL DEFAULT '',

    -- Assigned by the platform, monotonic. The apply gate and the resume
    -- cursor are both this number.
    version         bigint NOT NULL,

    valid_from_ns   bigint NOT NULL,
    record_time_ns  bigint NOT NULL
);

CREATE TABLE IF NOT EXISTS instrument_identifier (
    instrument_id text   NOT NULL REFERENCES instrument (instrument_id) ON DELETE CASCADE,
    scheme        text   NOT NULL,
    value         text   NOT NULL,

    -- Empty for a global scheme.
    source        text   NOT NULL DEFAULT '',

    valid_from_ns bigint NOT NULL,

    -- NULL while the mapping is still current.
    valid_to_ns   bigint
);

-- The lookup W3.1 makes, in the order it makes it.
CREATE INDEX IF NOT EXISTS instrument_identifier_lookup
    ON instrument_identifier (scheme, value, source);

CREATE INDEX IF NOT EXISTS instrument_identifier_owner
    ON instrument_identifier (instrument_id);
