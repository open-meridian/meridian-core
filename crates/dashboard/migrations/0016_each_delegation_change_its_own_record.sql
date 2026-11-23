-- Each delegation change its own record (decision 47, decisions/031;
-- roadmap v31a item 13). Additive.
--
-- A delegation's row holds what stands now: a renewal replaces what it
-- covers and its dates in place, a revocation marks it, and the sweep
-- removes it a month after it ended. From here each of those -- made,
-- renewed, revoked, its rows rewritten or its tools filled in by an
-- upgrade, lapsed at its expiry (found by the sweep, once, valid when it
-- expired), removed -- is also a row here, written in the transaction that
-- made the change: what it was, what it became (the dates a renewal
-- replaced among them), who made it, when it was made and when it was
-- recorded. No key names the delegation row, so the records outlive it; the
-- dashboard only ever adds to them.
--
-- A delegation standing before this is given the records its row holds the
-- facts of, as config_plugin_launch_gap was (config migration 15): made, by
-- its person, when the row says; renewed, by its person, where it was
-- renewed since; its tools filled in, where they were; revoked, by whom and
-- why the row says. Each is marked backfilled, valid when the row says, and
-- recorded at the deployment's time as the migration runs (database.rs),
-- never back-dated; what the row does not hold -- what a renewal replaced,
-- any renewal before its last -- is said to be not known.
--
-- The release before keeps serving while this runs, and its changes write
-- no record. So the sweep gives the same records to any delegation with
-- none (one it made), and before each change is recorded the delegation as
-- it stands is compared with its latest record: one found otherwise is
-- first given a record saying it was changed by an earlier release, from
-- what the last record said to what it is, who and when not known. Each
-- record's `was` is the one before's `became`.
CREATE TABLE IF NOT EXISTS dashboard_delegation_change (
    change_id     bigserial PRIMARY KEY,
    delegation_id text      NOT NULL,
    -- Whose delegation, and to which client.
    subject       text      NOT NULL,
    client_id     text      NOT NULL,
    kind          text      NOT NULL CHECK (kind IN ('made', 'renewed', 'revoked', 'rewritten',
                                                     'acting_filled', 'lapsed', 'removed',
                                                     'changed_by_an_earlier_release')),
    was           text      NOT NULL DEFAULT '',
    became        text      NOT NULL DEFAULT '',
    -- The person consenting, the subject revoking (or `client`, or
    -- `dashboard`); empty for an upgrade's or the sweep's, and where it is
    -- not known.
    changed_by    text      NOT NULL DEFAULT '',
    -- When it was recorded, by the deployment's clock.
    changed_at_ns bigint    NOT NULL,
    -- When the change was made (its valid time); NULL where not known.
    valid_at_ns   bigint,
    -- Written after the change, from what the delegation's row held.
    backfilled    boolean   NOT NULL DEFAULT false,
    note          text      NOT NULL DEFAULT '',
    CHECK (NOT backfilled OR kind IN ('made', 'renewed', 'acting_filled', 'revoked',
                                      'changed_by_an_earlier_release')),
    CHECK (backfilled OR kind <> 'changed_by_an_earlier_release')
);

CREATE INDEX IF NOT EXISTS dashboard_delegation_change_by_delegation
    ON dashboard_delegation_change (delegation_id, change_id);

-- A delegation's records backfilled once.
CREATE UNIQUE INDEX IF NOT EXISTS dashboard_delegation_change_backfill_once
    ON dashboard_delegation_change (delegation_id) WHERE backfilled AND kind = 'made';

-- A delegation lapses once at each expiry.
CREATE UNIQUE INDEX IF NOT EXISTS dashboard_delegation_change_lapsed_once
    ON dashboard_delegation_change (delegation_id, valid_at_ns) WHERE kind = 'lapsed';
