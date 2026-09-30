-- Terminal sessions (W6.13, W6.14), kept here so a dashboard restart or
-- upgrade leaves them standing (ruled 2026-09-30,
-- kernel/terminal-sessions-survive-a-restart). A browser's session stays in
-- the dashboard's memory.
--
-- Keyed by the SHA-256 of the token, in unpadded base64url, and never the
-- token: there is no column one could go in, so a dump of this table signs
-- nobody in. A session names a person and nothing they may do; access is
-- evaluated per request from the records, exactly as a browser's is.
--
-- Written to be run again harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS dashboard_terminal_session (
    session_hash      text   PRIMARY KEY CHECK (session_hash <> ''),

    -- Who signed in, as they were at that sign-in and never refreshed: a new
    -- sign-in is.
    subject           text   NOT NULL,
    display_name      text   NOT NULL DEFAULT '',
    directory_groups  text[] NOT NULL DEFAULT '{}',

    -- decisions/015's two bounds are counted from these: 12 hours from the
    -- sign-in, 30 minutes from the last use. The bounds themselves are in
    -- the binary, not here, because a bound somebody can change in a table
    -- is not a bound.
    signed_in_at_ns   bigint NOT NULL,
    last_seen_at_ns   bigint NOT NULL
);

-- An administrator ends a person's sessions all at once (W6.14).
CREATE INDEX IF NOT EXISTS dashboard_terminal_session_subject
    ON dashboard_terminal_session (subject);

-- Why a session stopped, so the refusal can say `lapsed` or `ended` rather
-- than that the deployment never heard of it. Kept until the session would
-- have lapsed anyway, and holding nothing about whose it was.
CREATE TABLE IF NOT EXISTS dashboard_terminal_session_gone (
    session_hash  text   PRIMARY KEY CHECK (session_hash <> ''),
    reason        text   NOT NULL CHECK (reason IN ('lapsed', 'ended')),
    until_ns      bigint NOT NULL
);
