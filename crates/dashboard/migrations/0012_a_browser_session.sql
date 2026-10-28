-- Browsers' sessions (ruling 811), kept here so a dashboard restart or
-- upgrade signs nobody out, as the terminal sessions before delegations
-- were kept (migration 2).
--
-- Keyed by the SHA-256 of the token, in unpadded base64url, and never the
-- token: there is no column one could go in, so a dump of this table signs
-- nobody in. The form token is derived from the session's token, and kept
-- nowhere. A session names a person and nothing they may do; access is
-- evaluated per request from the records.
--
-- Nothing to backfill: a session held in memory before this ended with the
-- process that held it, which is what a restart did until now.
--
-- Written to be run again harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS dashboard_browser_session (
    session_hash      text   PRIMARY KEY CHECK (session_hash <> ''),

    -- Who signed in, as they were at that sign-in and never refreshed: a new
    -- sign-in is.
    subject           text   NOT NULL,
    display_name      text   NOT NULL DEFAULT '',
    directory_groups  text[] NOT NULL DEFAULT '{}',

    -- decisions/015's two bounds are counted from these: 12 hours from the
    -- sign-in, 30 minutes from the last use. The bounds themselves are in
    -- the binary, not here.
    signed_in_at_ns   bigint NOT NULL,
    last_seen_at_ns   bigint NOT NULL
);

-- A password reset ends a person's sessions all at once (W6.16).
CREATE INDEX IF NOT EXISTS dashboard_browser_session_subject
    ON dashboard_browser_session (subject);
