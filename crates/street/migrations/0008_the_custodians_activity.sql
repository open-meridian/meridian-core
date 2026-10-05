-- The custodian's activity, and each sync status (contract v14: W2.10 to
-- W2.14; spec/the-custodians-activity-explains-a-break, requirements 7 and 8;
-- plans/the-custodians-activity-explains-a-break, its sync-status names ruled
-- 2026-10-05).
--
-- An activity is kept as the custodian stated it and the plugin sent it: the
-- CustodialActivity whole, encoded as it arrived, beside the columns a read
-- selects and orders by -- its source, account and the custodian's identifier,
-- unique together, which is what makes a redelivery recognisable; its trade
-- date; and its kind, instrument and units, for the harness's lines. Nothing
-- derives a position, a lot or a figure from it (requirement 8), so the record
-- is never taken apart past those columns, and a field a later revision adds
-- to it is kept by this table unchanged.
--
-- A sync status is kept as the custody plugin published it, against the
-- account its external account is linked to or against none, '' (W2's
-- invariant: it is not refused for want of a link); every one heard is a
-- record, numbered and chained per account. Its `history_from` is a column,
-- since the street answers it on a read of activity (W2.11).
--
-- Each is a change of its own kind in the street's partition, so the chain
-- table admits the two kinds; a database adopted from before the history may
-- not have it, and a migration that finds nothing to do touches nothing.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables
                WHERE table_schema = current_schema() AND table_name = 'account_chain') THEN
        ALTER TABLE account_chain DROP CONSTRAINT IF EXISTS account_chain_chain_check;
        ALTER TABLE account_chain ADD CONSTRAINT account_chain_chain_check
            CHECK (chain IN ('position', 'statement', 'activity', 'sync_status'));
    END IF;
END $$;

CREATE TABLE IF NOT EXISTS activity (
    activity_id              text    PRIMARY KEY,
    source                   text    NOT NULL CHECK (source <> ''),
    account_id               text    NOT NULL CHECK (account_id <> ''),
    external_account_id      text    NOT NULL DEFAULT '',
    external_activity_id     text    NOT NULL CHECK (external_activity_id <> ''),
    trade_date               text    NOT NULL DEFAULT '',
    kind                     integer NOT NULL DEFAULT 0,
    instrument_id            text    NOT NULL DEFAULT '',
    units                    numeric,
    -- The CustodialActivity, encoded as the plugin sent it.
    record                   bytea   NOT NULL,
    recorded_at_ns           bigint  NOT NULL,
    sequence                 bigint  NOT NULL,
    previous_sequence        bigint  NOT NULL,
    cause_instance_id        text    NOT NULL DEFAULT '',
    cause_acting_for_subject text    NOT NULL DEFAULT '',
    cause_correlation_id     text    NOT NULL DEFAULT '',
    cause_causation_id       text    NOT NULL DEFAULT '',
    CONSTRAINT activity_once UNIQUE (source, account_id, external_activity_id),
    CONSTRAINT activity_units_on_the_wire CHECK (street_on_the_wire(units))
);

-- A read by account and trade date, and one since a watermark.
CREATE INDEX IF NOT EXISTS activity_by_trade_date ON activity (account_id, trade_date, sequence);
CREATE INDEX IF NOT EXISTS activity_by_change ON activity (sequence);

CREATE TABLE IF NOT EXISTS sync_status (
    sequence                 bigint  PRIMARY KEY,
    previous_sequence        bigint  NOT NULL,
    -- '' where the external account is not linked.
    account_id               text    NOT NULL DEFAULT '',
    external_account_id      text    NOT NULL DEFAULT '',
    source                   text    NOT NULL DEFAULT '',
    state                    integer NOT NULL DEFAULT 0,
    history_from             text    NOT NULL DEFAULT '',
    -- The SyncStatusEvent, encoded as the custody plugin published it, its
    -- account as the sidecar stamped it.
    record                   bytea   NOT NULL,
    recorded_at_ns           bigint  NOT NULL,
    cause_instance_id        text    NOT NULL DEFAULT '',
    cause_acting_for_subject text    NOT NULL DEFAULT '',
    cause_correlation_id     text    NOT NULL DEFAULT '',
    cause_causation_id       text    NOT NULL DEFAULT ''
);

-- The latest for each connection's account, and for an account's history.
CREATE INDEX IF NOT EXISTS sync_status_latest
    ON sync_status (account_id, source, external_account_id, sequence);
