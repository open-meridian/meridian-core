-- Integrity at rest (step 3b; spec/a-deployment-scales-out-and-fails-over-
-- by-configuration section 9; plan 5j). The book's journal as a chain: each
-- entry carries the digest of the entry before it in its partition and its
-- own -- SHA-256 of that and its canonical form -- so a replay verifies each
-- entry it reads, and the scrub, following the chain, names a changed or
-- missing entry by sequence. The partition's head keeps its chain's end, so
-- the next entry is chained in its own transaction, under the head's lock.
--
-- The entries kept before this are chained by `migrate` right after it, in
-- each partition's order (honest about time: their digests are taken when
-- this ran, of what the record held then).
ALTER TABLE book_entry ADD COLUMN IF NOT EXISTS previous_digest bytea;
ALTER TABLE book_entry ADD COLUMN IF NOT EXISTS digest bytea;
ALTER TABLE book_partition_head ADD COLUMN IF NOT EXISTS last_digest bytea;

-- The book's integrity findings (`IntegrityFinding`), kept for good beside
-- its control partition, and each scrub's run, which `status` reads.
CREATE TABLE IF NOT EXISTS book_integrity_finding (
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
CREATE TABLE IF NOT EXISTS book_scrub (
    started_at_ns  bigint PRIMARY KEY,
    finished_at_ns bigint NOT NULL,
    checked        bigint NOT NULL,
    filled         bigint NOT NULL,
    findings       bigint NOT NULL,
    member         text   NOT NULL
);

-- The journal stays append-only (W9), with the one change an entry takes:
-- its place in the chain, given once to an entry kept before it, nothing
-- else of it touched. Every other update, every delete and every truncate
-- is refused as before.
CREATE OR REPLACE FUNCTION book_entry_is_never_changed() RETURNS trigger
    LANGUAGE plpgsql AS $f$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF OLD.digest IS NULL AND NEW.digest IS NOT NULL
           AND (to_jsonb(OLD) - 'previous_digest' - 'digest')
             = (to_jsonb(NEW) - 'previous_digest' - 'digest') THEN
            RETURN NEW;
        END IF;
    END IF;
    RAISE EXCEPTION 'the book''s journal is append-only: an entry is never updated or deleted; a correction is a later entry (W9)';
END
$f$;
