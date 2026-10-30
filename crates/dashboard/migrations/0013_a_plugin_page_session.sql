-- Plugin pages' sessions (ruling 855), kept beside browsers' (migration
-- 12) so a dashboard restart or an upgrade leaves a plugin's page open in
-- a browser working: its forms post and its reads answer, with nobody sent
-- back through the dashboard.
--
-- A plugin page's session is the plugin's host's own cookie, redeemed from
-- a one-time code the dashboard minted (decisions/021). Keyed by the
-- SHA-256 of its token, in unpadded base64url, and never the token, as a
-- browser's session is. It names the session it came from and ends with
-- it: a browser's by that session's hash, the row going when that
-- session's row goes; or a delegation's by its id, swept once the
-- delegation has ended. It holds the instance it opens, the level it was
-- opened at, and the prepared change the page was opened from while that
-- waits (W6.27); nothing about what the person may do, which is evaluated
-- per request from the records.
--
-- Nothing to backfill: a plugin page's session held in memory before this
-- ended with the process that held it, which is what a restart did until
-- now.
--
-- Written to be run again harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS dashboard_plugin_session (
    plugin_session_hash  text   PRIMARY KEY CHECK (plugin_session_hash <> ''),

    -- Exactly one of the two: what it came from, and ends with.
    browser_session_hash text   REFERENCES dashboard_browser_session (session_hash)
                                ON DELETE CASCADE,
    delegation_id        text,
    CHECK ((browser_session_hash IS NULL) <> (delegation_id IS NULL)),

    plugin_instance_id   text   NOT NULL CHECK (plugin_instance_id <> ''),
    -- "admin", "write" or "read" (meridian_access::level_name).
    level                text   NOT NULL CHECK (level IN ('admin', 'write', 'read')),
    -- Cleared once the change is settled or gone.
    prepared_call_id     text   NOT NULL DEFAULT '',
    entered_at_ns        bigint NOT NULL
);

CREATE INDEX IF NOT EXISTS dashboard_plugin_session_browser
    ON dashboard_plugin_session (browser_session_hash);
