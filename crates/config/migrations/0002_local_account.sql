-- The accounts this deployment holds itself, for a firm with no directory of
-- its own (decisions/018). The other two branches keep nothing here: a firm's
-- provider and a firm's LDAP both hold their own people, and this deployment
-- learns who signed in and nothing more.
--
-- It is a credential store, and a small one. What that obliges is written
-- into the columns rather than left to whoever reads them next.

CREATE TABLE IF NOT EXISTS config_local_account (
    -- What somebody types. Lowercased before it is stored and before it is
    -- looked up, so `Ada` and `ada` cannot become two people.
    name             text   PRIMARY KEY CHECK (name <> '' AND name = lower(name)),
    display_name     text   NOT NULL DEFAULT '',

    -- Argon2id, in PHC string form, carrying its own salt and parameters so a
    -- later change of those does not invalidate the rows written before it.
    -- Never a password, and there is no column that could hold one.
    password_hash    text   NOT NULL CHECK (password_hash <> ''),

    -- Matched exactly as a directory's groups are. An account this deployment
    -- holds is not a different kind of person, and the access model does not
    -- learn that there are two kinds.
    groups           text[] NOT NULL DEFAULT '{}',

    -- The only branch where a password is guessed against us rather than
    -- against somebody else's directory, so it is the only one that counts.
    failed_attempts  integer NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    -- Zero means not locked. A time rather than a flag, so a lock expires by
    -- itself and nothing has to remember to unlock it.
    locked_until_ns  bigint NOT NULL DEFAULT 0,

    created_at_ns    bigint NOT NULL
);
