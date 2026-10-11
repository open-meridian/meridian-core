-- Contract v20 (plans/an-agents-prepared-change-waits-for-the-person,
-- sdk-contract/an-agents-prepared-change-contract): what the configuration
-- store keeps of it.
--
-- Written to be run again harmlessly, as every migration here is. What needs
-- the deployment's time -- the account grants every standing link implies,
-- and the one-time rename of three roles -- is written by the conductor in
-- the same transaction (migrations.rs), each its own record, never
-- back-dated (decisions/031).

-- The prepared change a person made a change from (W4.9, W6.27; the plan's
-- Q16): its `PC-` ID beside the delegation and client each change table
-- already keeps, stamped by the dashboard, never typed. Empty for every
-- change made otherwise, and for every change before v20, when nothing was
-- prepared: nothing is backfilled.
ALTER TABLE config_plugin_setting_change
    ADD COLUMN IF NOT EXISTS prepared_call_id text NOT NULL DEFAULT '',
    -- A secret's last four characters, for a secret of sixteen or more
    -- (W6.11, `hint`, approved 2026-10-11): kept with the change that set
    -- it, apart from the sealed value, and never derived from it again.
    -- Empty for a setting that is not secret, a shorter secret, a clear, and
    -- every secret set before v20.
    ADD COLUMN IF NOT EXISTS hint text NOT NULL DEFAULT '';
ALTER TABLE config_hold_change
    ADD COLUMN IF NOT EXISTS prepared_call_id text NOT NULL DEFAULT '';
ALTER TABLE config_archive_change
    ADD COLUMN IF NOT EXISTS prepared_call_id text NOT NULL DEFAULT '';
ALTER TABLE config_plugin_launch
    ADD COLUMN IF NOT EXISTS prepared_call_id         text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS stopped_prepared_call_id text NOT NULL DEFAULT '';

-- The accounts a plugin instance's admins may link its external accounts to
-- (W6.30; the product owner, 2026-10-10): nothing by default, granted and
-- withdrawn by a deployment admin. What stands now, one row per account.
CREATE TABLE IF NOT EXISTS config_linkable_account (
    plugin_instance_id text NOT NULL CHECK (plugin_instance_id <> ''),
    account_id         text NOT NULL REFERENCES config_account,
    PRIMARY KEY (plugin_instance_id, account_id)
);

-- And each grant and withdrawal, its own record (LinkableAccountsChange,
-- decisions/031): the accounts added and withdrawn, who, through which
-- delegation and client, when and why. The upgrade's own grant names no
-- person and says so in its note. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_linkable_accounts_change (
    change_id          bigserial PRIMARY KEY,
    plugin_instance_id text      NOT NULL,
    granted            text[]    NOT NULL DEFAULT '{}',
    withdrawn          text[]    NOT NULL DEFAULT '{}',
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS config_linkable_accounts_change_by_instance
    ON config_linkable_accounts_change (plugin_instance_id, change_id);

-- The one-time rename of three roles at v20 (`dgm` to `data`, `ccm` to
-- `connectivity`, `match` to `confirmation`; the product owner, 2026-10-10):
-- each launch, version, plugin's report, access group and hold the rewrite
-- renamed, its own record, what it named before and after, by no person.
CREATE TABLE IF NOT EXISTS config_role_rename (
    rename_id   bigserial PRIMARY KEY,
    -- launch, version, known plugin, access group, hold.
    what        text      NOT NULL,
    which       text      NOT NULL,
    from_role   text      NOT NULL,
    to_role     text      NOT NULL,
    renamed_at_ns bigint  NOT NULL
);
