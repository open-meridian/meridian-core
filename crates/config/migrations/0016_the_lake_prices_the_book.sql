-- The lake's 1a (contract v18, W10.1; plans/the-lake-prices-the-book): what
-- each instance's catalogue declares, and each dataset's licence and each
-- entitlement to it, as a deployment admin set them. Kept beside the plugins'
-- configuration and never sent to the platform (spec/the-lake, requirement
-- 14). Every licence and entitlement change is its own record
-- (decisions/031): the record is the contract's own message, as set, with who,
-- through which delegation and client, when, and why. The data configuration
-- is new, and owes no backfill.
--
-- Written to be run again harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS config_plugin_catalogue (
    plugin_instance_id text   PRIMARY KEY CHECK (plugin_instance_id <> ''),
    -- meridian.v1.Catalogue, as its sidecar's report or its launched version
    -- carried it, replaced whole.
    catalogue          bytea  NOT NULL,
    recorded_at_ns     bigint NOT NULL
);

CREATE TABLE IF NOT EXISTS config_dataset_licence_change (
    change_id     bigserial PRIMARY KEY,
    dataset       text      NOT NULL CHECK (dataset <> ''),
    -- meridian.v1.DatasetLicence, as recorded.
    licence       bytea     NOT NULL,
    changed_at_ns bigint    NOT NULL
);
CREATE INDEX IF NOT EXISTS config_dataset_licence_change_by_dataset
    ON config_dataset_licence_change (dataset, change_id);

CREATE TABLE IF NOT EXISTS config_dataset_entitlement_change (
    change_id     bigserial PRIMARY KEY,
    dataset       text      NOT NULL CHECK (dataset <> ''),
    instance      text      NOT NULL CHECK (instance <> ''),
    -- meridian.v1.DatasetEntitlement, as recorded.
    entitlement   bytea     NOT NULL,
    changed_at_ns bigint    NOT NULL
);
CREATE INDEX IF NOT EXISTS config_dataset_entitlement_change_by_dataset
    ON config_dataset_entitlement_change (dataset, instance, change_id);
