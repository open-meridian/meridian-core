-- Each user-group and account-group change its own record (decision 47,
-- decisions/031; roadmap v31a item 13). Additive.
--
-- Until this the store replaced a user group or an account group in place
-- and kept nothing of what it had been, who changed it or when. From here
-- each one defined or changed is a row: which group, what it was, what it
-- became, who and through which delegation, and when. Kept when a group
-- changes again: a record is never updated or deleted.
--
-- A group standing before this has one backfilled record (kind 3), saying it
-- as it stood and that who defined it or last changed it, and when, is not
-- known, written by the conductor in the same transaction at the
-- deployment's time as the migration runs (migrations.rs), never back-dated,
-- its valid time not known.
CREATE TABLE IF NOT EXISTS config_group_change (
    change_id          bigserial PRIMARY KEY,
    -- 1 a user group, 2 an account group.
    group_kind         smallint  NOT NULL CHECK (group_kind IN (1, 2)),
    group_id           text      NOT NULL,
    -- 1 defined, 2 changed, 3 held when the records began (backfilled).
    kind               smallint  NOT NULL CHECK (kind BETWEEN 1 AND 3),
    was                text      NOT NULL DEFAULT '',
    became             text      NOT NULL DEFAULT '',
    -- Empty where who made it is not known.
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    -- When it was recorded, by the deployment's clock.
    changed_at_ns      bigint    NOT NULL,
    -- When the change was made (its valid time); NULL where not known, as
    -- it always is for a backfilled record: a group's row holds no date.
    valid_at_ns        bigint,
    backfilled         boolean   NOT NULL DEFAULT false,
    note               text      NOT NULL DEFAULT '',
    CHECK (backfilled = (kind = 3))
);

CREATE INDEX IF NOT EXISTS config_group_change_by_group
    ON config_group_change (group_kind, group_id, change_id);

-- Once per group.
CREATE UNIQUE INDEX IF NOT EXISTS config_group_change_backfill_once
    ON config_group_change (group_kind, group_id) WHERE kind = 3;

-- Kept as written, as every change table is (migration 30).
DROP TRIGGER IF EXISTS config_append_only ON config_group_change;
CREATE TRIGGER config_append_only BEFORE UPDATE OR DELETE ON config_group_change
    FOR EACH ROW EXECUTE FUNCTION config_append_only();
