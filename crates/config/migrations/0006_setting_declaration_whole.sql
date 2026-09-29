-- What a plugin declared of each setting, whole (W4.8): the label, default,
-- unit, choices and condition the dashboard's form is built from, and whether
-- it is a developer's, as well as the columns 0005 made. Encoded as the
-- SettingDeclaration the report carried, so a field the declaration gains is
-- kept with no migration of its own. Null for a row written before this, which
-- is read from its columns and replaced at the plugin's next report. The
-- `type` column now also holds 4, a choice (SETTING_TYPE_CHOICE).

ALTER TABLE config_plugin_setting_declaration ADD COLUMN IF NOT EXISTS declared bytea;
