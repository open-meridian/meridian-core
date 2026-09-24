-- The accounts this deployment holds itself, for a firm with no directory of
-- its own (decisions/018). The other two branches keep nothing here: a firm's
-- provider and a firm's LDAP both hold their own people.
--
-- Prefixed dashboard_, because a deployment may put every store in one
-- database and two components sharing a table is not supported. It is the
-- dashboard's because the dashboard is what checks a password: it already has
-- the password in hand, having served the form it arrived on, so putting the
-- hashes anywhere else would mean sending the password somewhere to be
-- compared against them.

CREATE TABLE IF NOT EXISTS dashboard_local_account (
    -- What somebody types, lowercased going in and coming out, so `Ada` and
    -- `ada` cannot become two people holding different permissions.
    name             text   PRIMARY KEY CHECK (name <> '' AND name = lower(name)),
    display_name     text   NOT NULL DEFAULT '',

    -- Argon2id in PHC string form, carrying its own salt and parameters so
    -- changing those later does not invalidate rows written before it. Never
    -- a password: there is no column one could go in.
    password_hash    text   NOT NULL CHECK (password_hash <> ''),

    -- Matched exactly as a directory's groups are. An account this deployment
    -- holds is not a different kind of person, and the access model never
    -- learns there are two kinds.
    groups           text[] NOT NULL DEFAULT '{}',

    -- The only branch where a password is guessed against us rather than
    -- against somebody else's directory, so the only one that counts.
    failed_attempts  integer NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    -- Zero means not locked. A time rather than a flag, so a lock lifts by
    -- itself and nothing has to remember to undo it.
    locked_until_ns  bigint NOT NULL DEFAULT 0,

    created_at_ns    bigint NOT NULL
);
