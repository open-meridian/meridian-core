-- Each delegation change its own record (decision 47, decisions/031;
-- roadmap v31a item 13). Additive.
--
-- A delegation's row holds what stands now: a renewal replaces what it
-- covers and its dates in place, a revocation marks it, and the sweep
-- removes it a month after it ended. From here each of those -- made,
-- renewed, revoked, its rows rewritten or its tools filled in by an
-- upgrade, removed -- is also a row here, written in the transaction that
-- made the change: what it was, what it became (the dates a renewal
-- replaced among them), who made it, and when. No key names the delegation
-- row, so the records outlive it; the dashboard only ever adds to them.
--
-- A delegation standing before this has one backfilled record, saying it as
-- it stood and that who made each change before is not known, written by the
-- dashboard in the same transaction at the deployment's time as the
-- migration runs (database.rs), never back-dated.
CREATE TABLE IF NOT EXISTS dashboard_delegation_change (
    change_id     bigserial PRIMARY KEY,
    delegation_id text      NOT NULL,
    -- Whose delegation, and to which client.
    subject       text      NOT NULL,
    client_id     text      NOT NULL,
    kind          text      NOT NULL CHECK (kind IN ('made', 'renewed', 'revoked', 'rewritten',
                                                     'acting_filled', 'removed',
                                                     'held_when_records_began')),
    was           text      NOT NULL DEFAULT '',
    became        text      NOT NULL DEFAULT '',
    -- The person consenting, the subject revoking (or `client`, or
    -- `dashboard`); empty for an upgrade's or the sweep's, and where it is
    -- not known.
    changed_by    text      NOT NULL DEFAULT '',
    changed_at_ns bigint    NOT NULL,
    backfilled    boolean   NOT NULL DEFAULT false,
    note          text      NOT NULL DEFAULT '',
    CHECK (backfilled = (kind = 'held_when_records_began'))
);

CREATE INDEX IF NOT EXISTS dashboard_delegation_change_by_delegation
    ON dashboard_delegation_change (delegation_id, change_id);

-- Once per delegation.
CREATE UNIQUE INDEX IF NOT EXISTS dashboard_delegation_change_backfill_once
    ON dashboard_delegation_change (delegation_id) WHERE kind = 'held_when_records_began';
