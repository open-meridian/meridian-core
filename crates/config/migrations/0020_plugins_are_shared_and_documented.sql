-- Contract v25 (sdk-contract/plugins-are-shared-and-documented-contract):
-- where a version came from, and its retirement.
--
-- Where a version came from (W8.10, W8.14): `local` for one uploaded or
-- tracked from a branch -- every version recorded before v25 was, so the
-- default is their fact, not a guess -- `community` or `verified` for one
-- installed from a registry or carried in by a bundle; the listing it
-- belongs to, `organisation/plugin`, empty for a local version; and, for a
-- bundle's, when the signing keys' status it was checked against was issued.
ALTER TABLE config_plugin_version
    ADD COLUMN IF NOT EXISTS source_registry         text   NOT NULL DEFAULT 'local',
    ADD COLUMN IF NOT EXISTS listing                 text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS key_status_issued_at_ns bigint NOT NULL DEFAULT 0;

-- A version retired from the catalogue (W8.15; decision 59): who, through
-- which delegation and client, from which prepared change, when and why,
-- written once beside the version, whose row is kept. No version is retired
-- before v25, so nothing is owed to the past.
ALTER TABLE config_plugin_version
    ADD COLUMN IF NOT EXISTS retired_by                 text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_at_ns              bigint NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS retired_note               text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_through_delegation text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_client_name        text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_prepared_call_id   text   NOT NULL DEFAULT '';
