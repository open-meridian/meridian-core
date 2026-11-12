-- Contract v30 (W8.3; the resilience spec's section 10a; ruled 2026-11-11):
-- the share at its sidecar an instance was launched with, as its launch
-- asked -- a SidecarLimits, encoded. NULL where it asked none, and on every
-- launch before it. Additive.
ALTER TABLE config_plugin_launch
    ADD COLUMN IF NOT EXISTS sidecar_limits bytea;
