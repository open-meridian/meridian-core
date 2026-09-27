-- A launch in the live shape (W8.3, spec/live-plugin-development): on a
-- development deployment, a plugin that runs the files sent to it since its
-- image. Recorded, so the catalogue says which instances are running code
-- nobody uploaded as a version.

ALTER TABLE config_plugin_launch ADD COLUMN IF NOT EXISTS live boolean NOT NULL DEFAULT false;
