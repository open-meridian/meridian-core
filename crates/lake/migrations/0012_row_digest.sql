-- Integrity at rest (step 3b; spec/a-deployment-scales-out-and-fails-over-
-- by-configuration section 9; plan 5j). Each row's digest, SHA-256 of its
-- body as recorded, kept beside it, carried on its change so a follower
-- verifies what it applies, and checked when the scrub re-reads the record.
--
-- Not backfilled here: a lake holds millions of rows, and rewriting each in
-- one migration would hold the upgrade for as long and double the table on
-- disk. A row recorded before this has none until the scrub, at low
-- priority under the writer's lease, gives it its digest; until then it is
-- taken as it is, and said so (`filled` in each scrub's run).
ALTER TABLE lake_row ADD COLUMN IF NOT EXISTS digest bytea;

-- The lake's integrity findings (`IntegrityFinding`), kept for good beside
-- its control partition, and each scrub's run, which `status` reads.
CREATE TABLE IF NOT EXISTS lake_integrity_finding (
    id              bigserial PRIMARY KEY,
    partition       text   NOT NULL,
    sequence        bigint NOT NULL,
    committed_at_ns bigint NOT NULL,
    what            text   NOT NULL,
    member          text   NOT NULL,
    origin          text   NOT NULL,
    found_at_ns     bigint NOT NULL,
    UNIQUE (partition, sequence, what)
);
CREATE TABLE IF NOT EXISTS lake_scrub (
    started_at_ns  bigint PRIMARY KEY,
    finished_at_ns bigint NOT NULL,
    checked        bigint NOT NULL,
    filled         bigint NOT NULL,
    findings       bigint NOT NULL,
    member         text   NOT NULL
);
