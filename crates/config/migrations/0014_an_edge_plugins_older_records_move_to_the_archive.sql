-- An edge plugin's older records move to the archive (contract v16;
-- spec/an-edge-plugins-older-records-move-to-the-archive,
-- plans/an-edge-plugins-older-records-move-to-the-archive).
--
-- Three kinds of record, each change its own (decisions/031), never updated
-- and never deleted; what stands now is each one's latest. Nothing of them
-- existed before this, so there is no past to backfill and no gap to record:
-- no hold was ever set, no archive ever allowed, and no plugin could report a
-- move until a sidecar served v16.

-- A hold on raw records (W6.25): the least days a record is kept anywhere,
-- for one edge role or, with an empty role, for every one; 0 days clears it.
CREATE TABLE IF NOT EXISTS config_hold_change (
    change_id          bigserial PRIMARY KEY,
    role               text      NOT NULL,
    days               integer   NOT NULL CHECK (days BETWEEN 0 AND 36500),
    write_once         boolean   NOT NULL,
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL
);

CREATE INDEX IF NOT EXISTS config_hold_change_by_role
    ON config_hold_change (role, change_id);

-- An instance's archive allowed, its bound changed, or withdrawn (W8.7);
-- most_bytes 0 for no bound.
CREATE TABLE IF NOT EXISTS config_archive_change (
    change_id          bigserial PRIMARY KEY,
    instance_id        text      NOT NULL,
    allowed            boolean   NOT NULL,
    most_bytes         bigint    NOT NULL CHECK (most_bytes >= 0),
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL
);

CREATE INDEX IF NOT EXISTS config_archive_change_by_instance
    ON config_archive_change (instance_id, change_id);

-- A move of one unit of a kind of raw record (W4.13): archived (1), restored
-- (2), returned (3) or deleted (4), its count and span, the rule that made it
-- or the person it was done for, and when. Never a record's content.
CREATE TABLE IF NOT EXISTS config_record_move (
    move_id            bigserial PRIMARY KEY,
    instance_id        text      NOT NULL,
    record_kind        text      NOT NULL,
    unit               text      NOT NULL,
    record_count       bigint    NOT NULL CHECK (record_count > 0),
    first_received_ns  bigint    NOT NULL,
    last_received_ns   bigint    NOT NULL,
    outcome            smallint  NOT NULL CHECK (outcome BETWEEN 1 AND 4),
    rule               text      NOT NULL DEFAULT '',
    person             text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    recorded_at_ns     bigint    NOT NULL
);

CREATE INDEX IF NOT EXISTS config_record_move_by_instance
    ON config_record_move (instance_id, move_id);
CREATE INDEX IF NOT EXISTS config_record_move_by_unit
    ON config_record_move (instance_id, record_kind, unit, move_id);
