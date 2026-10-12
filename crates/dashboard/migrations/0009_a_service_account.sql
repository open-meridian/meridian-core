-- Contract v21 (plans/a-service-account-runs-a-plugins-autonomous-work,
-- "Refreshed 2026-10-10", ruled 2026-10-11;
-- sdk-contract/a-service-account-runs-a-plugins-autonomous-work): a service
-- account, each role's "runs as" choice, each change's record, and the
-- notices its owner is told, all kept by the dashboard.
--
-- Written to be run again harmlessly, as every migration here is. Nothing
-- is backfilled: no service account existed before this, no role ran as
-- anything, and what a plugin did as itself stays a system actor typed as
-- its instance, which is what it was (decisions/031).

-- A service account (W6.32, `ServiceAccount`): a user of the deployment that
-- is not a person, its subject its `SA-` ID. It holds no credential: what
-- vouches for it is this row, its grants and a role's choice.
CREATE TABLE IF NOT EXISTS dashboard_service_account (
    -- `SA-` and a ULID, and its subject.
    service_account_id text    PRIMARY KEY CHECK (service_account_id LIKE 'SA-%'),
    name               text    NOT NULL CHECK (name <> ''),
    purpose            text    NOT NULL DEFAULT '',
    -- A person the deployment knows, by their deployment-local subject.
    owner_subject      text    NOT NULL CHECK (owner_subject <> ''),
    -- `prepare_only` or `act`.
    setting            text    NOT NULL CHECK (setting IN ('prepare_only', 'act')),
    -- `active`, `disabled` or `removed`.
    state              text    NOT NULL CHECK (state IN ('active', 'disabled', 'removed')),
    created_at_ns      bigint  NOT NULL,
    -- Its last call, and its last refusal and when, as its row shows them.
    last_used_ns       bigint,
    last_refusal       text    NOT NULL DEFAULT '',
    last_refusal_ns    bigint
);

-- A role's "runs as" (W6.33): the service account a plugin instance's role
-- runs its autonomous work as, and the level, `write` or `read`. What stands
-- now; a role with no row runs as nobody.
CREATE TABLE IF NOT EXISTS dashboard_runs_as (
    instance           text    NOT NULL CHECK (instance <> ''),
    role               text    NOT NULL,
    service_account_id text    NOT NULL REFERENCES dashboard_service_account (service_account_id),
    level              text    NOT NULL CHECK (level IN ('write', 'read')),
    PRIMARY KEY (instance, role)
);

-- Each change, its own record (`ServiceAccountChange`, decisions/031): who,
-- when, the account it names, the scope -- empty for the account itself,
-- `instance:role` for a role's choice -- from, to and the note. Never
-- updated, never deleted: removing an account keeps every one.
CREATE TABLE IF NOT EXISTS dashboard_service_account_change (
    change_id          bigserial PRIMARY KEY,
    service_account_id text      NOT NULL,
    scope              text      NOT NULL DEFAULT '',
    from_said          text      NOT NULL DEFAULT '',
    to_said            text      NOT NULL DEFAULT '',
    changed_by         text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS dashboard_service_account_change_by_account
    ON dashboard_service_account_change (service_account_id, change_id);

-- A notice names a ticket, a prepared change or, from v21, a service account
-- (W6.24, W6.34): its owner told it was disabled, and each prepared change
-- of its that lapsed with it.
ALTER TABLE dashboard_notice
    ADD COLUMN IF NOT EXISTS service_account_id text NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS dashboard_notice_service_account
    ON dashboard_notice (subject, notice_id) WHERE service_account_id <> '';
