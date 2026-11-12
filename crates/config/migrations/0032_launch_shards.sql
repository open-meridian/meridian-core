-- Contract v30 (W8.3; spec/a-deployment-scales-out-and-fails-over-by-
-- configuration section 7; the launch settings approved by 1057): the shards
-- and the standby pod an instance was launched with, as its launch asked,
-- within its version's most_shards. 0 and false on every launch before
-- them: one pod, no standby. The launcher still runs one pod (3d builds the
-- shards and the standby). Additive.
ALTER TABLE config_plugin_launch
    ADD COLUMN IF NOT EXISTS shards integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS standby boolean NOT NULL DEFAULT false;
