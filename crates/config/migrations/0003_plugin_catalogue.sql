-- The plugin catalogue (W8): every version uploaded, and every launch.
--
-- A version is its name and version, and never replaced: a second upload of
-- the same pair is refused, not an update. A launch is a row per launch, so
-- one stopped and launched again keeps both; at most one per instance is
-- live, which the partial index enforces where two launches could otherwise
-- both pass a check made before either was written.

CREATE TABLE IF NOT EXISTS config_plugin_version (
    name           text    NOT NULL CHECK (name <> ''),
    version        text    NOT NULL CHECK (version <> ''),
    roles          text[]  NOT NULL DEFAULT '{}',
    tags           text[]  NOT NULL DEFAULT '{}',
    interface      boolean NOT NULL,
    sdk_version    text    NOT NULL,
    image_digest   text    NOT NULL CHECK (image_digest LIKE 'sha256:%'),
    uploaded_by    text    NOT NULL,
    uploaded_at_ns bigint  NOT NULL,
    PRIMARY KEY (name, version)
);

CREATE TABLE IF NOT EXISTS config_plugin_launch (
    launch_id      bigserial PRIMARY KEY,
    instance_id    text      NOT NULL CHECK (instance_id <> ''),
    name           text      NOT NULL,
    version        text      NOT NULL,
    image_digest   text      NOT NULL,
    roles          text[]    NOT NULL DEFAULT '{}',
    tags           text[]    NOT NULL DEFAULT '{}',
    launched_by    text      NOT NULL,
    launched_at_ns bigint    NOT NULL,
    -- PluginLaunchState: 1 launched, 2 stopped, 3 failed.
    state          smallint  NOT NULL CHECK (state IN (1, 2, 3)),
    stopped_by     text      NOT NULL DEFAULT '',
    stopped_at_ns  bigint    NOT NULL DEFAULT 0,
    failure        text      NOT NULL DEFAULT '',
    FOREIGN KEY (name, version) REFERENCES config_plugin_version
);

CREATE UNIQUE INDEX IF NOT EXISTS config_plugin_launch_live
    ON config_plugin_launch (instance_id) WHERE state = 1;
