-- Step 3d, checkpoint 5, item 30 (approved 2026-11-17, item 68): an
-- instance's own storage claim copied into its shared storage's shard-0/
-- before its first launch as several pods. The launch that copied keeps
-- which claim it copied and from when its pods' reports count; the record,
-- `PluginStorageCopied`, is written once one of its pods is heard
-- registered after that -- its pods start only once the copy is whole, so
-- that is a true "copied by" -- one per launch, kept as written
-- (decisions/031). A failed copy leaves none. Additive.
ALTER TABLE config_plugin_launch
    ADD COLUMN IF NOT EXISTS storage_copied_from text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS storage_copied_after_ns bigint NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS config_plugin_storage_copied (
    instance_id    text   NOT NULL,
    launched_at_ns bigint NOT NULL,
    from_claim     text   NOT NULL,
    to_claim       text   NOT NULL,
    at_ns          bigint NOT NULL,
    launched_by    text   NOT NULL,
    PRIMARY KEY (instance_id, launched_at_ns)
);

DROP TRIGGER IF EXISTS config_append_only ON config_plugin_storage_copied;
CREATE TRIGGER config_append_only BEFORE UPDATE OR DELETE ON config_plugin_storage_copied
    FOR EACH ROW EXECUTE FUNCTION config_append_only();
