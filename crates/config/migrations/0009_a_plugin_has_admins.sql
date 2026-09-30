-- A plugin has admins (sdk-contract/a-plugin-has-admins, W6.6 to W6.8).
--
-- `admin` is a level beside `read` and `write`: an access entry may name a
-- plugin at it, and it reaches no account.
ALTER TABLE config_access_entry DROP CONSTRAINT IF EXISTS config_access_entry_level_check;
ALTER TABLE config_access_entry
    ADD CONSTRAINT config_access_entry_level_check CHECK (level IN (1, 2, 3));

-- Two more groups are built in. All accounts holds every account the
-- deployment has, those opened later included, and lists none of its own;
-- All plugins (admin) grants `admin` on every plugin, those launched later
-- included, and no account.
ALTER TABLE config_account_group ADD COLUMN IF NOT EXISTS built_in boolean NOT NULL DEFAULT false;

INSERT INTO config_account_group (account_group_id, name, account_ids, built_in)
VALUES ('all-accounts', 'All accounts', '{}', true)
ON CONFLICT (account_group_id) DO NOTHING;

INSERT INTO config_access_group (access_group_id, name, built_in)
VALUES ('all-plugins-admin', 'All plugins (admin)', true)
ON CONFLICT (access_group_id) DO NOTHING;

-- A permission to either built-in access group names no account group. One
-- to an access group whose entries are all `admin` names none either, and one
-- with a `read` or `write` entry names one; that depends on the entries, so
-- it is the code's to check, as it was already.
ALTER TABLE config_permission DROP CONSTRAINT IF EXISTS config_permission_check;
ALTER TABLE config_permission
    ADD CONSTRAINT config_permission_check
    CHECK (access_group_id NOT IN ('deployment-admin', 'all-plugins-admin')
           OR account_group_id IS NULL);

-- A deployment admin was admin on every plugin by being one. They are now
-- only through All plugins (admin), which first run and a claim code link
-- them to (W6.2, W7.6); a deployment set up before this is linked the same
-- way here, so upgrading takes nothing away that a new deployment would have.
-- The link may be withdrawn afterwards like any other.
INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
SELECT 'P-all-plugins-' || user_group_id, user_group_id, NULL, 'all-plugins-admin'
  FROM config_permission
 WHERE access_group_id = 'deployment-admin'
ON CONFLICT (permission_id) DO NOTHING;
