-- What the dashboard holds between two requests that may reach different
-- copies of it (step 3b; spec/a-deployment-scales-out-and-fails-over-by-
-- configuration section 4): a directory sign-in between its start and its
-- callback, a plugin page's one-time code, and an agent's authorisation
-- from its asking to the code its client trades. Until now each was held
-- in the one dashboard's memory, which is why the chart refused a second.
--
-- Keyed by the SHA-256 of the state, code or id, in unpadded base64url, and
-- never the key itself, as sessions are (migration 12): a dump of this
-- table answers no callback. Each row is short-lived and spent once; a
-- sweep removes what has expired.
--
-- Nothing to backfill: what was in flight in memory before this ended with
-- the process that held it, as a restart always ended it.
--
-- Written to be run again harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS dashboard_in_flight (
    kind           text        NOT NULL CHECK (kind IN ('sign-in', 'plugin-code',
                                   'authorisation', 'provider-state', 'agent-code')),
    key_hash       text        NOT NULL CHECK (key_hash <> ''),
    held           jsonb       NOT NULL,
    expires_at_ns  bigint      NOT NULL,
    -- The order things were put in, for keeping the newest of a kind; the
    -- database's clock, never compared with the deployment's.
    put_at         timestamptz NOT NULL,
    PRIMARY KEY (kind, key_hash)
);

CREATE INDEX IF NOT EXISTS dashboard_in_flight_expires
    ON dashboard_in_flight (expires_at_ns);
