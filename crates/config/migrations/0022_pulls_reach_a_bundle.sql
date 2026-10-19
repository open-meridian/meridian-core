-- Pulls reach a deployment by bundle (contract v26, W8.14; ruling 196): the
-- newest PluginPullStatus held, its JSON as the platform signed it, kept
-- beside the catalogue's copy and its key status. The newer of a bundle's
-- and the one held, by issued_at_ns, is kept; empty until a bundle brings
-- one.
ALTER TABLE config_registry_catalogue
    ADD COLUMN IF NOT EXISTS pull_status text NOT NULL DEFAULT '';
