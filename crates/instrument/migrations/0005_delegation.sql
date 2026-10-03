-- Contract v12: what a person sets or merges through a client on the
-- deployment's MCP surface is recorded with the delegation and the client's
-- name beside the person (W3.10, W3.13, W6.20; spec/a-deployment-serves-its-
-- mcp, Q9), as the book records them (W9.8): stamped from the command's
-- envelope, never typed. Empty for a person at the dashboard in a browser, a
-- plugin's report, the platform's answer and a migration.
--
-- Additions only, `IF NOT EXISTS` throughout, so it is run again harmlessly.

ALTER TABLE instrument_value_source
    ADD COLUMN IF NOT EXISTS acting_through_delegation text NOT NULL DEFAULT '';
ALTER TABLE instrument_value_source
    ADD COLUMN IF NOT EXISTS client_name text NOT NULL DEFAULT '';

ALTER TABLE instrument_version
    ADD COLUMN IF NOT EXISTS acting_through_delegation text NOT NULL DEFAULT '';
ALTER TABLE instrument_version
    ADD COLUMN IF NOT EXISTS client_name text NOT NULL DEFAULT '';
