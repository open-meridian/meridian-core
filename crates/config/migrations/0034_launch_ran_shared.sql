-- Step 3d, checkpoint 5, PO item 91 (approved 2026-11-17): whether the
-- launcher ran a launch on its instance's shared storage, as its
-- CreatePluginReply.ran_shared said. Only a later launch that ran on the
-- instance's own claim -- false here, and not failed -- keeps an earlier
-- launch's copy unrecorded (review c5c89 M4). Additive.
ALTER TABLE config_plugin_launch
    ADD COLUMN IF NOT EXISTS ran_shared boolean NOT NULL DEFAULT false;
