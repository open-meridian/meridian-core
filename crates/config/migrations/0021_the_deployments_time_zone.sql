-- The deployment's time zone (contract v26, W6.35, W7.5; the v26 plan's
-- question 229): an IANA time-zone ID, America/New_York until a deployment
-- admin -- or the wizard, at the deployment's first run -- sets another.
-- Each change its own record (decisions/031): the zone, who, through which
-- delegation and client, when and why. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_time_zone_change (
    change_id            bigserial PRIMARY KEY,
    deployment_time_zone text      NOT NULL,
    changed_by           text      NOT NULL DEFAULT '',
    through_delegation   text      NOT NULL DEFAULT '',
    client_name          text      NOT NULL DEFAULT '',
    prepared_call_id     text      NOT NULL DEFAULT '',
    changed_at_ns        bigint    NOT NULL,
    note                 text      NOT NULL DEFAULT ''
);
