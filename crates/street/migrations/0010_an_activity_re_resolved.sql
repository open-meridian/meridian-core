-- An activity re-resolved (contract v15: W2.15, W2.16;
-- tasks/sdk-contract/an-activity-is-re-resolved-when-its-instrument-resolves,
-- its names ruled 2026-10-05 and 2026-10-06).
--
-- When an activity's instrument resolves later -- a plan's own code linked
-- after the activity was reported -- the custody plugin re-resolves it, and
-- the street keeps each re-resolution as a record of its own beside the
-- activity as first recorded, which never changes (decisions/031): the
-- instrument it now resolves to, empty where unresolved again, the
-- Provenance of that resolution encoded as it arrived, when it was resolved
-- as the plugin sent it, and when the street recorded it, never back-dated.
--
-- Each is a change of its own kind in the street's partition, chained per
-- account apart from the activities, so a reader hearing activity-recorded
-- sees no gap from them; the chain table admits the kind. Nothing is
-- backfilled and no gap is recorded: before this, no activity was ever
-- re-resolved, so there is nothing to know of. A migration that finds
-- nothing to do touches nothing.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'account_chain') THEN
        ALTER TABLE account_chain DROP CONSTRAINT IF EXISTS account_chain_chain_check;
        ALTER TABLE account_chain ADD CONSTRAINT account_chain_chain_check
            CHECK (chain IN ('position', 'statement', 'activity', 'sync_status', 're_resolution'));
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS activity_re_resolution (
    sequence                 bigint  PRIMARY KEY,
    previous_sequence        bigint  NOT NULL,
    activity_id              text    NOT NULL REFERENCES activity (activity_id),
    account_id               text    NOT NULL CHECK (account_id <> ''),
    -- '' where the link the activity had been resolved by was removed.
    instrument_id            text    NOT NULL DEFAULT '',
    -- The Provenance, encoded as the plugin sent it.
    provenance               bytea   NOT NULL,
    resolved_at_ns           bigint  NOT NULL CHECK (resolved_at_ns <> 0),
    recorded_at_ns           bigint  NOT NULL,
    cause_instance_id        text    NOT NULL DEFAULT '',
    cause_acting_for_subject text    NOT NULL DEFAULT '',
    cause_correlation_id     text    NOT NULL DEFAULT '',
    cause_causation_id       text    NOT NULL DEFAULT ''
);

-- An activity's latest, and a read since a watermark by account.
CREATE INDEX IF NOT EXISTS activity_re_resolution_by_activity
    ON activity_re_resolution (activity_id, sequence);
CREATE INDEX IF NOT EXISTS activity_re_resolution_by_account
    ON activity_re_resolution (account_id, sequence);
