-- A new database timeline raises the epoch (contract v30, stage C; the
-- resilience spec's section 3, approved 2026-11-10). The database's
-- identity -- its system identifier and the timeline it writes on -- the
-- writer last saw, kept on the control partition's head row, so a writer
-- that finds another (a failover or a restore that can have lost a committed
-- tail) raises the epoch before it commits there. Empty until a writer
-- first looks: nothing is raised then, since nothing says what came before.
ALTER TABLE lake_change_head
    ADD COLUMN IF NOT EXISTS database_identity text NOT NULL DEFAULT '';
