-- A setting that became secret keeps no plain value in its change records
-- (the product owner, 2026-10-05, for
-- sdk-contract/the-custodians-activity-contract: "agree with recommendation").
--
-- Each change is its own record, never deleted (decisions/031), and a record
-- of a setting that is not secret carries the value it was set to (migration
-- 11). When the setting is later declared secret, or its value is stored
-- sealed, the values its earlier records hold are redacted: the value blanked,
-- the record's shape kept, and nothing else changed -- not who, not when, and
-- no record deleted. The one narrow exception to a record never changing.
-- Each redaction is its own record (action 4): which records it blanked, why,
-- by no person, and when. Migration 11's backfilled copies are records like
-- any other and are redacted the same way.
--
-- What is secret now and still has values in its records is redacted by the
-- conductor in the same transaction (migrations.rs), at the deployment's
-- clock as the migration runs, never back-dated.

ALTER TABLE config_plugin_setting_change
    DROP CONSTRAINT IF EXISTS config_plugin_setting_change_action_check;
ALTER TABLE config_plugin_setting_change
    ADD CONSTRAINT config_plugin_setting_change_action_check CHECK (action IN (1, 2, 3, 4));
