-- Core's plugin area is at parity on the MCP (contract v17;
-- plans/cores-plugin-area-is-at-parity-on-the-mcp, accepted 2026-10-09).
--
-- Each change through a tool is its own record naming the person, the
-- delegation and the client (decisions/031; spec/a-deployment-serves-its-mcp,
-- requirement 18). The change tables kept the delegation since v16 and no
-- client: each gains the client's name beside it. A setting change, a hold
-- and an archive change made before this were each made in a browser,
-- through no delegation, so their empty delegation and client are true and
-- are left so; nothing is backfilled.
ALTER TABLE config_plugin_setting_change
    ADD COLUMN IF NOT EXISTS client_name text NOT NULL DEFAULT '';
ALTER TABLE config_hold_change
    ADD COLUMN IF NOT EXISTS client_name text NOT NULL DEFAULT '',
    -- Why, in the words of whoever made it: required through /mcp (W6.20),
    -- optional at the page; empty on every change before this.
    ADD COLUMN IF NOT EXISTS note        text NOT NULL DEFAULT '';
ALTER TABLE config_archive_change
    ADD COLUMN IF NOT EXISTS client_name text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS note        text NOT NULL DEFAULT '';
ALTER TABLE config_record_move
    ADD COLUMN IF NOT EXISTS client_name text NOT NULL DEFAULT '';

-- A launch and its stop each name the delegation and client they were made
-- through, and keep their note (W8.3, W8.4).
ALTER TABLE config_plugin_launch
    ADD COLUMN IF NOT EXISTS launched_through_delegation text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS launched_client_name        text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS launch_note                 text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS stopped_through_delegation  text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS stopped_client_name         text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS stop_note                   text NOT NULL DEFAULT '';

-- Every launch and stop before this was made through the terminal on a
-- delegation that was not recorded. Each gets a gap record saying so (its
-- form as v15's per access group), written by the conductor in the same
-- transaction at the deployment's time (migrations.rs), never back-dated and
-- never an invented delegation. Kept when the launch is gone: a record is
-- never deleted.
CREATE TABLE IF NOT EXISTS config_plugin_launch_gap (
    gap_id      bigserial PRIMARY KEY,
    launch_id   bigint    NOT NULL,
    instance_id text      NOT NULL,
    launched_at_ns bigint NOT NULL,
    -- 1 the launch, 2 its stop.
    act         smallint  NOT NULL CHECK (act IN (1, 2)),
    noted_at_ns bigint    NOT NULL,
    note        text      NOT NULL
);

-- Once per launch and act.
CREATE UNIQUE INDEX IF NOT EXISTS config_plugin_launch_gap_once
    ON config_plugin_launch_gap (launch_id, act);
