-- Event groups (contract v26; W1.30, W3.29, W3.30;
-- spec/the-contract-gaps-v26-and-v27-close, A1 to A9): a venue's grouping of
-- event contracts, every version kept and never changed, each its own record
-- (decisions/031) -- the platform's `EVG-` group once matched or minted, the
-- deployment's own `LCL-` group while it waits. A group's identity is its
-- venue and the venue's reference for its event; each version takes the next
-- place in the store's sequence of event groups, which a reader catches up by.
--
-- Additions only, `IF NOT EXISTS`, so it is run again harmlessly.

CREATE SEQUENCE IF NOT EXISTS instrument_event_group_sequence;

CREATE TABLE IF NOT EXISTS instrument_event_group (
    event_group_id  text   NOT NULL,
    version         bigint NOT NULL,
    venue_id        text   NOT NULL,
    venue_event_ref text   NOT NULL,
    sequence        bigint NOT NULL UNIQUE DEFAULT nextval('instrument_event_group_sequence'),
    record          bytea  NOT NULL,
    kept_at_ns      bigint NOT NULL,
    PRIMARY KEY (event_group_id, version)
);
CREATE INDEX IF NOT EXISTS instrument_event_group_by_event
    ON instrument_event_group (venue_id, venue_event_ref);
