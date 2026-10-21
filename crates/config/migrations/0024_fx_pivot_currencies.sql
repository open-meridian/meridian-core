-- The deployment's FX pivot currencies (contract v27, W6.36; rulings 462
-- and 483): the fiat cash instruments, by their INS- IDs, a fiat leg with no
-- direct rate is crossed through, tried in order; USD's and then EUR's until
-- a deployment admin sets another. Each change its own record
-- (decisions/031): the list, who, through which delegation and client, when
-- and why. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_fx_pivot_change (
    change_id           bigserial PRIMARY KEY,
    fx_pivot_currencies text[]    NOT NULL,
    changed_by          text      NOT NULL DEFAULT '',
    through_delegation  text      NOT NULL DEFAULT '',
    client_name         text      NOT NULL DEFAULT '',
    prepared_call_id    text      NOT NULL DEFAULT '',
    changed_at_ns       bigint    NOT NULL,
    note                text      NOT NULL DEFAULT ''
);
