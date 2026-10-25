-- The deployment's settings set as one encoded message each (contract v28):
-- the series request rule (W6.37; rulings 561 and 589) and the stale-reference
-- actions permitted (W6.38; rulings 517 and 600), each a deployment admin's.
-- Each change its own record (decisions/031): the setting by its name, its
-- value as the message's bytes, who, through which delegation and client,
-- when and why. Never updated, never deleted; the latest change of a setting
-- is in force, none its default.
CREATE TABLE IF NOT EXISTS config_setting_change (
    change_id           bigserial PRIMARY KEY,
    setting             text      NOT NULL,
    value               bytea     NOT NULL,
    changed_by          text      NOT NULL DEFAULT '',
    through_delegation  text      NOT NULL DEFAULT '',
    client_name         text      NOT NULL DEFAULT '',
    prepared_call_id    text      NOT NULL DEFAULT '',
    changed_at_ns       bigint    NOT NULL,
    note                text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS config_setting_change_by_setting
    ON config_setting_change (setting, change_id);
