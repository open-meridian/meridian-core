-- A version's declaration (W8.1, contract v11): its secret settings' names,
-- what it receives and does not carry, and the storage it asks for with its
-- retention, kept with the version as the upload carried it and approved at
-- launch. Encoded as the wire's PluginDeclaration; NULL for a version
-- uploaded with none, built before v11.

ALTER TABLE config_plugin_version ADD COLUMN IF NOT EXISTS declaration bytea;
