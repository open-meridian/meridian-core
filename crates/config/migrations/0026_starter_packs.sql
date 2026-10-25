-- Starter packs in the deployment (contract v27's wire, built in v28;
-- spec/starter-packs, requirements 18 to 22; W8.18, W8.19).
--
-- An install from a registry names the pack whose install made it,
-- `organisation/pack`; empty for an install of one listing, and for every
-- version recorded before v28, when no pack could be installed here.
ALTER TABLE config_plugin_version
    ADD COLUMN IF NOT EXISTS installed_by_pack text NOT NULL DEFAULT '';

-- The deployment's record of a starter pack, each change its own record
-- (decisions/031): `pack_installed` (the members ticked and each one's
-- outcome), `pack_updated` ("Update all", each member's outcome) and
-- `pack_removed` (the record removed, every member left as it was); the
-- pack as the catalogue held it then (a PluginPack, encoded), the revision
-- installed, the members' outcomes (a PackStatus's members, encoded), who,
-- through which delegation and client, from which prepared change, when and
-- why. No pack was installed here before v28, so nothing is owed to the
-- past. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_pack_change (
    change_id          bigserial PRIMARY KEY,
    pack               text      NOT NULL,
    kind               text      NOT NULL
                       CHECK (kind IN ('pack_installed', 'pack_updated', 'pack_removed')),
    revision           bigint    NOT NULL,
    held               bytea     NOT NULL,
    members            bytea     NOT NULL,
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    prepared_call_id   text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS config_pack_change_by_pack
    ON config_pack_change (pack, change_id);
