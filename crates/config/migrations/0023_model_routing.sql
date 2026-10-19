-- The analytics routing table (contract v26, W12.6; the one-API spec's
-- requirement 21 and Q9; ruling 285: the conductor holds it): which
-- implementation answers each model id deployment-wide, `built-in` unless
-- a deployment admin routes it to a launched `pricing` instance. Each
-- change its own record (decisions/031): the model id, the implementation,
-- who, through which delegation and client, when and why. Never updated,
-- never deleted; the table's version is one more than its changes.
CREATE TABLE IF NOT EXISTS config_model_routing_change (
    change_id          bigserial PRIMARY KEY,
    model_id           text      NOT NULL,
    implementation     text      NOT NULL,
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    prepared_call_id   text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);
