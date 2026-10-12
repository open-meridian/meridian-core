-- A connectivity plugin's `ems`, the `ems`'s venue account map and the
-- venue's catalogue (contract v22; W11.20 to W11.22; ruled 2026-10-10).
--
-- Each connectivity plugin's link as it stands, one row per instance: the
-- wire's ConnectivityLink, encoded -- available, the one `ems` it is linked
-- to, who may route through it. An instance with no row is withdrawn and
-- unlinked, as nothing is available by default.
CREATE TABLE IF NOT EXISTS config_connectivity_link (
    connectivity_instance_id text  PRIMARY KEY CHECK (connectivity_instance_id <> ''),
    link                     bytea NOT NULL
);

-- And each change to one, its own record (ConnectivityLinkChange,
-- decisions/031; kept by the conductor, no proto): the link before -- none
-- for the upgrade's own record -- and after, who, through which delegation
-- and client, when and why. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_connectivity_link_change (
    change_id                bigserial PRIMARY KEY,
    connectivity_instance_id text      NOT NULL,
    from_link                bytea,
    to_link                  bytea     NOT NULL,
    changed_by               text      NOT NULL DEFAULT '',
    through_delegation       text      NOT NULL DEFAULT '',
    client_name              text      NOT NULL DEFAULT '',
    changed_at_ns            bigint    NOT NULL,
    note                     text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS config_connectivity_link_change_by_instance
    ON config_connectivity_link_change (connectivity_instance_id, change_id);

-- Each venue account mapping standing: a code once per connectivity
-- plugin, the `ems` that made it and the account, one it was granted (W6.30).
CREATE TABLE IF NOT EXISTS config_venue_account_mapping (
    connectivity_instance_id text NOT NULL CHECK (connectivity_instance_id <> ''),
    venue_account_code       text NOT NULL CHECK (venue_account_code <> ''),
    ems_instance_id          text NOT NULL CHECK (ems_instance_id <> ''),
    account_id               text NOT NULL REFERENCES config_account,
    PRIMARY KEY (connectivity_instance_id, venue_account_code)
);

-- And each change to an `ems`'s map, its own record (VenueAccountMapChange,
-- decisions/031; kept by the conductor, no proto): the mapping before and
-- after, each empty where there was none, the `ems` that sent it, who it
-- acted for and when. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_venue_account_map_change (
    change_id                bigserial PRIMARY KEY,
    ems_instance_id          text      NOT NULL,
    connectivity_instance_id text      NOT NULL,
    venue_account_code       text      NOT NULL,
    from_account_id          text      NOT NULL DEFAULT '',
    to_account_id            text      NOT NULL DEFAULT '',
    changed_by               text      NOT NULL DEFAULT '',
    through_delegation       text      NOT NULL DEFAULT '',
    client_name              text      NOT NULL DEFAULT '',
    changed_at_ns            bigint    NOT NULL,
    note                     text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS config_venue_account_map_change_by_ems
    ON config_venue_account_map_change (ems_instance_id, change_id);

-- Each venue catalogue a connectivity plugin published that differed from
-- the one before it, its own record (W11.20): the wire's VenueCatalogue,
-- encoded, and when it was heard. The latest per plugin and code is the one
-- read.
CREATE TABLE IF NOT EXISTS config_venue_catalogue (
    catalogue_id             bigserial PRIMARY KEY,
    connectivity_instance_id text      NOT NULL CHECK (connectivity_instance_id <> ''),
    venue_account_code       text      NOT NULL CHECK (venue_account_code <> ''),
    catalogue                bytea     NOT NULL,
    heard_at_ns              bigint    NOT NULL
);
CREATE INDEX IF NOT EXISTS config_venue_catalogue_latest
    ON config_venue_catalogue (connectivity_instance_id, venue_account_code, catalogue_id);
