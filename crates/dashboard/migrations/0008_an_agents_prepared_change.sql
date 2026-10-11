-- Contract v20 (plans/an-agents-prepared-change-waits-for-the-person,
-- accepted 2026-10-10; sdk-contract/an-agents-prepared-change-contract): a
-- prepared change, what the admins let agents do with each act, and a
-- person's own settings, all kept by the dashboard.
--
-- Written to be run again harmlessly, as every migration here is. Nothing
-- was prepared, levelled or kept per person before this, so nothing is
-- backfilled but the upgrade's own record of the levels (each act at may
-- make and every gate off, which is what the deployment allowed), written by
-- the dashboard at the moment this ran (database.rs).

-- A prepared change (W6.26, `PreparedCall`): the act, its arguments as
-- prepared, who prepared it -- a person, or a person through a delegation
-- and its client -- why, and where it stands. Kept 90 days after it settles.
CREATE TABLE IF NOT EXISTS dashboard_prepared_call (
    -- `PC-` and a ULID.
    prepared_call_id   text    PRIMARY KEY CHECK (prepared_call_id LIKE 'PC-%'),
    -- The act by its name on the surface (`dashboard__set_hold`,
    -- `sample-ops-1__confirm_opening_balance`); the instance empty for one of
    -- core's.
    tool_name          text    NOT NULL,
    instance           text    NOT NULL DEFAULT '',
    arguments_json     text    NOT NULL,
    preparer_subject   text    NOT NULL,
    preparer_name      text    NOT NULL DEFAULT '',
    -- Empty for a person at a page.
    through_delegation text    NOT NULL DEFAULT '',
    client_name        text    NOT NULL DEFAULT '',
    reason             text    NOT NULL DEFAULT '',
    -- Held as suspect by the ticket's rules (W6.26): shown as withheld.
    reason_suspect     boolean NOT NULL DEFAULT false,
    -- meridian.v1.PreparedCallState's number.
    state              smallint NOT NULL CHECK (state BETWEEN 1 AND 7),
    -- The accounts its input names (`accounts`, W4.1), who it is shown to.
    accounts           text[]  NOT NULL DEFAULT '{}',
    -- The check's answer when it was kept, and the guard it was checked
    -- against (a settings or priority change's `against_updated_at_ns`).
    checked            text    NOT NULL DEFAULT '',
    prepared_at_ns     bigint  NOT NULL,
    settled_at_ns      bigint
);
CREATE INDEX IF NOT EXISTS dashboard_prepared_call_pending
    ON dashboard_prepared_call (prepared_at_ns) WHERE state IN (1, 2);
CREATE INDEX IF NOT EXISTS dashboard_prepared_call_by_delegation
    ON dashboard_prepared_call (through_delegation, prepared_at_ns);

-- Each step of a prepared change, its own record (W6.28, decisions/031):
-- prepared, shared, shown with what differs, made, changed, discarded,
-- withdrawn, lapsed; who or what, through which delegation and client, when
-- and the note where one was given.
CREATE TABLE IF NOT EXISTS dashboard_prepared_call_change (
    change_id          bigserial PRIMARY KEY,
    prepared_call_id   text      NOT NULL REFERENCES dashboard_prepared_call (prepared_call_id),
    -- 1 prepared, 2 shared, 3 shown with what differs, 4 made, 5 changed,
    -- 6 discarded, 7 withdrawn, 8 lapsed.
    kind               smallint  NOT NULL CHECK (kind BETWEEN 1 AND 8),
    by_subject         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    at_ns              bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS dashboard_prepared_call_change_by_call
    ON dashboard_prepared_call_change (prepared_call_id, change_id);

-- What agents may do with an act (W6.29, `agents_may`), and whether its
-- change needs a second pair of eyes, at a scope: the deployment's ceiling
-- (`ceiling`), core's act or a plugin's act as its admins set it (`act`), or
-- an access group's stricter level (`group:<access_group_id>`). What stands
-- now; an act at no scope is at may make, its gate off.
CREATE TABLE IF NOT EXISTS dashboard_agents_may (
    act                text     NOT NULL CHECK (act <> ''),
    scope              text     NOT NULL CHECK (scope <> ''),
    -- 1 may make, 2 prepare only, 3 may not.
    level              smallint NOT NULL CHECK (level BETWEEN 1 AND 3),
    second_pair_of_eyes boolean NOT NULL DEFAULT false,
    PRIMARY KEY (act, scope)
);

-- Each change of a level or a gate, its own record (`AgentsMayChange`):
-- who, when, the act, the scope, from and to, a note. The upgrade's record
-- names act `*`. A change of the lapse period is kept here too, act
-- `prepared_lapse_days`, scope `deployment`.
CREATE TABLE IF NOT EXISTS dashboard_agents_may_change (
    change_id          bigserial PRIMARY KEY,
    act                text      NOT NULL,
    scope              text      NOT NULL,
    from_said          text      NOT NULL DEFAULT '',
    to_said            text      NOT NULL,
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS dashboard_agents_may_change_by_act
    ON dashboard_agents_may_change (act, change_id);

-- The deployment's own settings the dashboard keeps: the lapse period
-- (`prepared_lapse_days`, 30 by default).
CREATE TABLE IF NOT EXISTS dashboard_deployment_setting (
    name   text PRIMARY KEY CHECK (name <> ''),
    value  text NOT NULL
);

-- A person's own values of the settings a plugin declares per_person
-- (W6.31): theirs alone.
CREATE TABLE IF NOT EXISTS dashboard_own_setting (
    instance   text   NOT NULL,
    subject    text   NOT NULL,
    name       text   NOT NULL,
    value      text   NOT NULL,
    PRIMARY KEY (instance, subject, name)
);

-- Each change, its own record (`OwnSettingChange`): the setting, the value,
-- the person, the delegation and client, when. Read by that person alone.
CREATE TABLE IF NOT EXISTS dashboard_own_setting_change (
    change_id          bigserial PRIMARY KEY,
    instance           text      NOT NULL,
    subject            text      NOT NULL,
    name               text      NOT NULL,
    value              text      NOT NULL,
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL
);
CREATE INDEX IF NOT EXISTS dashboard_own_setting_change_by_person
    ON dashboard_own_setting_change (subject, instance, change_id);

-- A notice names a ticket or, from v20, a prepared change (W6.24).
ALTER TABLE dashboard_notice ALTER COLUMN ticket_id DROP NOT NULL;
ALTER TABLE dashboard_notice
    ADD COLUMN IF NOT EXISTS prepared_call_id text NOT NULL DEFAULT '';

-- The delegation's rows the person set to Prepare only (W6.17,
-- `covers_prepare_only`), beside `covers_acting`: none on every delegation
-- consented before v20, which is Make on every row, what its consent covered.
ALTER TABLE dashboard_delegation
    ADD COLUMN IF NOT EXISTS covers_prepare_only text[] NOT NULL DEFAULT '{}';

-- The one-time rename of three roles at v20, in each delegation's rows
-- (`instance:role:level`): dgm became data, ccm connectivity, match
-- confirmation. Only the old names are read, so it changes nothing run again.
UPDATE dashboard_delegation
   SET covers_plugins = ARRAY(
        SELECT regexp_replace(regexp_replace(regexp_replace(r,
               '^([^:]*):dgm:', '\1:data:'),
               '^([^:]*):ccm:', '\1:connectivity:'),
               '^([^:]*):match:', '\1:confirmation:')
          FROM unnest(covers_plugins) AS r)
 WHERE EXISTS (SELECT 1 FROM unnest(covers_plugins) AS r
                WHERE r ~ '^[^:]*:(dgm|ccm|match):');
