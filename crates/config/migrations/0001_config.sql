-- The configuration store: what a deployment admin authors.
--
-- Prefixed config_ throughout, because a deployment may put every store in
-- one schema, and two components sharing a table is not supported.

CREATE TABLE IF NOT EXISTS config_account (
    account_id    text     PRIMARY KEY,
    name          text     NOT NULL CHECK (name <> ''),
    -- AccountState: 1 open, 2 closed. Closed, never deleted.
    state         smallint NOT NULL CHECK (state IN (1, 2)),
    created_at_ns bigint   NOT NULL
);

CREATE TABLE IF NOT EXISTS config_user_group (
    user_group_id    text   PRIMARY KEY,
    name             text   NOT NULL CHECK (name <> ''),
    directory_groups text[] NOT NULL DEFAULT '{}',
    logins           text[] NOT NULL DEFAULT '{}'
);

-- An explicit list. No nesting, and no row means "all accounts".
CREATE TABLE IF NOT EXISTS config_account_group (
    account_group_id text   PRIMARY KEY,
    name             text   NOT NULL CHECK (name <> ''),
    account_ids      text[] NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS config_access_group (
    access_group_id text    PRIMARY KEY,
    name            text    NOT NULL CHECK (name <> ''),
    built_in        boolean NOT NULL DEFAULT false
);

-- One plugin per entry, so each plugin's users can be counted on their own.
CREATE TABLE IF NOT EXISTS config_access_entry (
    access_group_id    text     NOT NULL REFERENCES config_access_group ON DELETE CASCADE,
    position           integer  NOT NULL,
    plugin_instance_id text     NOT NULL,
    tag                text     NOT NULL,
    -- AccessLevel: 1 read, 2 write.
    level              smallint NOT NULL CHECK (level IN (1, 2)),
    PRIMARY KEY (access_group_id, position)
);

-- A permission to deployment admin names no account group; every other names
-- one. Enforced here as well as in the code, so it holds for every writer.
CREATE TABLE IF NOT EXISTS config_permission (
    permission_id    text PRIMARY KEY,
    user_group_id    text NOT NULL REFERENCES config_user_group,
    account_group_id text REFERENCES config_account_group,
    access_group_id  text NOT NULL REFERENCES config_access_group,
    CHECK ((access_group_id = 'deployment-admin') = (account_group_id IS NULL))
);

CREATE TABLE IF NOT EXISTS config_external_account_link (
    plugin_instance_id  text NOT NULL,
    external_account_id text NOT NULL,
    account_id          text NOT NULL REFERENCES config_account,
    PRIMARY KEY (plugin_instance_id, external_account_id)
);

-- The latest sign-in of each person who has signed in, and nobody else.
CREATE TABLE IF NOT EXISTS config_sign_in (
    subject          text   PRIMARY KEY,
    display_name     text   NOT NULL,
    directory_groups text[] NOT NULL DEFAULT '{}',
    signed_in_at_ns  bigint NOT NULL
);

-- Plugins that have reported, and what they are launched as.
CREATE TABLE IF NOT EXISTS config_known_plugin (
    plugin_instance_id  text   PRIMARY KEY,
    role                text   NOT NULL,
    tags                text[] NOT NULL DEFAULT '{}',
    last_reported_at_ns bigint NOT NULL
);

-- Built in: exists from the start, and nobody holds it until a claim code is
-- redeemed.
INSERT INTO config_access_group (access_group_id, name, built_in)
VALUES ('deployment-admin', 'Deployment admin', true)
ON CONFLICT (access_group_id) DO NOTHING;
