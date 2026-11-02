-- The lake's control partition carries what is not a dataset's row
-- (step 3b; ruling 1062's carried item): each quality assessment, each
-- closed-day amendment, each source priority set and each alias kept, one
-- change each in sequence from the control head row, under the writer's
-- epoch checked under that row's lock -- so a follower's quality states,
-- flags, priorities and aliases match the writer's, and a deposed writer's
-- write is refused like its batch. The rows themselves stay where they are
-- kept (lake_quality_assessment, lake_closed_day, lake_priority_change,
-- lake_alias); this is their log, in the order they were committed.
CREATE TABLE IF NOT EXISTS lake_control_change (
    sequence        bigint PRIMARY KEY CHECK (sequence > 0),
    epoch           bigint NOT NULL,
    -- NULL for a change made here: the partition's own origin.
    origin          text,
    committed_at_ns bigint NOT NULL,
    kind            text   NOT NULL,
    body            bytea  NOT NULL
);
