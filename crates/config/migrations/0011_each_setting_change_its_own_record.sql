-- Each settings change its own record (W6.11, decisions/031; ruled
-- 2026-10-05 for sdk-contract/the-custodians-activity-contract: "a plugin page
-- saves its own setting, naming who").
--
-- Since 0005 a change recorded which setting, set or cleared, who and when,
-- and never the value. From here it also records what it was set to, for a
-- setting that is not secret (a secret's value never, only that one was set),
-- and the delegation its person acted through where they used one. A setting
-- is set only on the dashboard's Settings form (option A, 2026-10-05).
--
-- The changes recorded before this lack the value. Where the
-- facts exist they are backfilled: the latest change of each setting still
-- held, made by whoever the setting says set it and at that moment, carries
-- the value the setting holds (or is marked secret), saying it was
-- backfilled and from what; its time is its own, never moved. What cannot be
-- backfilled is recorded as a gap: one record per setting of each plugin
-- with any change before this, saying the earlier values are not known
-- before the moment this migration ran. That
-- record's time is the deployment's clock at the migration, written by the
-- conductor in the same transaction (migrations.rs), never back-dated.

ALTER TABLE config_plugin_setting_change
    -- What a setting that is not secret was set to; NULL for a clear, a
    -- secret, a gap, and a change made before this whose value is not known.
    ADD COLUMN IF NOT EXISTS value      text,
    ADD COLUMN IF NOT EXISTS secret     boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS through_delegation text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS backfilled boolean NOT NULL DEFAULT false,
    -- Why, for a gap and for a backfill: what is not known, or what it was
    -- filled in from.
    ADD COLUMN IF NOT EXISTS note       text    NOT NULL DEFAULT '';

-- 3: not known before this record's time (decisions/031, point 4).
ALTER TABLE config_plugin_setting_change
    DROP CONSTRAINT IF EXISTS config_plugin_setting_change_action_check;
ALTER TABLE config_plugin_setting_change
    ADD CONSTRAINT config_plugin_setting_change_action_check CHECK (action IN (1, 2, 3));

-- A secret's value is never here.
ALTER TABLE config_plugin_setting_change
    ADD CONSTRAINT config_plugin_setting_change_no_secret_value
    CHECK (NOT (secret AND value IS NOT NULL));

-- The facts that exist: the latest recorded change of each setting held now,
-- when it is the change the setting itself says was last made.
UPDATE config_plugin_setting_change change
   SET value      = held.value,
       secret     = held.sealed IS NOT NULL,
       backfilled = true,
       note       = 'value backfilled by migration 11 from the setting as held, set by '
                    'this person at this moment'
  FROM config_plugin_setting held
 WHERE change.action = 1
   AND change.plugin_instance_id = held.plugin_instance_id
   AND change.name = held.name
   AND change.changed_by = held.set_by
   AND change.changed_at_ns = held.set_at_ns
   AND change.change_id = (SELECT max(latest.change_id) FROM config_plugin_setting_change latest
                            WHERE latest.plugin_instance_id = change.plugin_instance_id
                              AND latest.name = change.name);

-- Once per setting.
CREATE UNIQUE INDEX IF NOT EXISTS config_plugin_setting_gap_once
    ON config_plugin_setting_change (plugin_instance_id, name)
    WHERE action = 3;
