-- A plugin holds a set of roles (decisions/020), where it held one.
--
-- Backfilled from the single role, so a plugin that reported before this ran
-- still carries what it carried: an access entry naming its role stays valid
-- until the plugin next reports.
ALTER TABLE config_known_plugin ADD COLUMN IF NOT EXISTS roles text[] NOT NULL DEFAULT '{}';
UPDATE config_known_plugin SET roles = ARRAY[role] WHERE role <> '' AND roles = '{}';
ALTER TABLE config_known_plugin DROP COLUMN IF EXISTS role;
