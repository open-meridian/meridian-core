-- Plugin settings (W4.8, W6.11): what each plugin declared it needs, what a
-- deployment admin gave it, and who changed which setting when.

-- What a plugin declared when it last registered, carried on its report.
-- Replaced whole by the report of a registered plugin, and left alone by one
-- that is not, so a plugin between registrations keeps its form.
CREATE TABLE IF NOT EXISTS config_plugin_setting_declaration (
    plugin_instance_id text     NOT NULL REFERENCES config_known_plugin ON DELETE CASCADE,
    position           integer  NOT NULL,
    name               text     NOT NULL,
    -- SettingType: 0 unspecified, 1 string, 2 integer, 3 boolean.
    type               smallint NOT NULL,
    required           boolean  NOT NULL,
    secret             boolean  NOT NULL,
    description        text     NOT NULL DEFAULT '',
    PRIMARY KEY (plugin_instance_id, position)
);

-- A value as given, or a secret sealed with the deployment's settings key
-- (requirement 35 as amended 2026-09-28): exactly one of the two. A secret is
-- never written to `value`, so a dump of this table reveals none.
CREATE TABLE IF NOT EXISTS config_plugin_setting (
    plugin_instance_id text   NOT NULL,
    name               text   NOT NULL CHECK (name <> ''),
    value              text,
    sealed             bytea,
    set_by             text   NOT NULL,
    set_at_ns          bigint NOT NULL,
    PRIMARY KEY (plugin_instance_id, name),
    CHECK ((value IS NULL) <> (sealed IS NULL))
);

-- Who changed which setting, and when. Never the value, and never a sealed
-- one: what was set is the table above's, and only as it stands now.
CREATE TABLE IF NOT EXISTS config_plugin_setting_change (
    change_id          bigserial PRIMARY KEY,
    plugin_instance_id text      NOT NULL,
    name               text      NOT NULL,
    -- 1 set or replaced, 2 cleared.
    action             smallint  NOT NULL CHECK (action IN (1, 2)),
    changed_by         text      NOT NULL,
    changed_at_ns      bigint    NOT NULL
);
