-- A person's access to a plugin is granted per role (contract v15,
-- decisions/033; plans/access-is-granted-per-role).
--
-- An access entry names a plugin, one role it holds, and a level; empty only
-- for a plugin holding no role. Every entry before this names none: the
-- conductor rewrites each, once, in the same transaction (migrations.rs), to
-- name its plugin's one role where it holds exactly one, as its sidecar last
-- reported or, where no sidecar has, its latest launch said. An entry on a
-- plugin holding none, or several, is left naming none.
ALTER TABLE config_access_entry ADD COLUMN IF NOT EXISTS role text NOT NULL DEFAULT '';

-- Every grant change its own record (W6.7, W6.8, decisions/031; the plan's
-- R1, ruled 2026-10-05): an access group defined or changed, an entry the
-- rewrite rewrote, a permission granted or withdrawn -- what it was, what it
-- became, who, through which delegation, when. Before this the store kept
-- each group as it stood and nothing of how it came to be, so each group has
-- one gap record (kind 6) saying its history is not known before the moment
-- this migration ran, written by the conductor at the deployment's time, never
-- back-dated. Kept when a group or a permission is gone: a record is never
-- deleted.
CREATE TABLE IF NOT EXISTS config_access_change (
    change_id          bigserial PRIMARY KEY,
    access_group_id    text      NOT NULL,
    -- 1 defined, 2 changed, 3 rewritten at v15, 4 permission granted,
    -- 5 permission withdrawn, 6 not known before this record.
    kind               smallint  NOT NULL CHECK (kind BETWEEN 1 AND 6),
    was                text      NOT NULL DEFAULT '',
    became             text      NOT NULL DEFAULT '',
    permission_id      text      NOT NULL DEFAULT '',
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS config_access_change_by_group
    ON config_access_change (access_group_id, change_id);

-- Once per group.
CREATE UNIQUE INDEX IF NOT EXISTS config_access_change_gap_once
    ON config_access_change (access_group_id) WHERE kind = 6;
