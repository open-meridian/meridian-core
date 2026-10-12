-- Contract v22 (W3.16 to W3.21; spec/settlement-calendars-are-core-
-- reference-data): the settlement calendars and date rules the deployment
-- pulled from the platform, every version kept, the earlier ones readable,
-- with when this deployment kept each, which a read as of a recorded time is
-- held to. A deployment mints and edits none: every row is one the
-- platform's answer carried, and a version is never changed.
--
-- Additions only, `IF NOT EXISTS` throughout, so it is run again harmlessly.

CREATE TABLE IF NOT EXISTS instrument_calendar_version (
    calendar_id text   NOT NULL CHECK (calendar_id LIKE 'CAL-%'),
    version     bigint NOT NULL CHECK (version > 0),
    -- meridian.v1.CalendarRecord, as the platform answered it.
    record      bytea  NOT NULL,
    kept_at_ns  bigint NOT NULL,
    PRIMARY KEY (calendar_id, version)
);

CREATE TABLE IF NOT EXISTS instrument_date_rule_version (
    -- meridian.v1.DateRuleKind: 1 settlement, 2 spot.
    kind        integer NOT NULL CHECK (kind IN (1, 2)),
    subject     text    NOT NULL CHECK (subject <> ''),
    version     bigint  NOT NULL CHECK (version > 0),
    -- meridian.v1.DateRuleVersion, as the platform answered it.
    rule        bytea   NOT NULL,
    kept_at_ns  bigint  NOT NULL,
    PRIMARY KEY (kind, subject, version)
);
