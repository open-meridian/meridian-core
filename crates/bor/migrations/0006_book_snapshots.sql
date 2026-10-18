-- An account's projection kept as at one entry (W9.10, contract v25): the
-- book starts a command or a dated read from the latest snapshot at or
-- before the entry it needs and replays the journal's tail. Core's own and no
-- record kind: derived from the journal, never the record. Taken when a
-- command's replay reaches the interval; from v26 also when a read replays
-- past it, and at the book's start for every account past it with none of the
-- current format (an upgraded account's). One of another format, or that does
-- not read, is passed over -- the book replays from an older one, or the
-- first entry -- until one is taken, which replaces it where both are as at
-- the same entry. `meridian-bor rebuild` makes them again.
--
-- `last_sequence` and `last_entry_id` name the latest entry the snapshot
-- holds (a snapshot whose entry the journal no longer has there is stale and
-- passed over); `entries` how many it holds; `latest_effective_date` the
-- latest effective date among them, so a read by business date starts from
-- one only where none is later; `taken_at_ns` the deployment's time of the
-- entry it was taken at. The latest few per account are kept.
CREATE TABLE IF NOT EXISTS book_snapshot (
    account_id            text    NOT NULL,
    last_sequence         bigint  NOT NULL,
    last_entry_id         text    NOT NULL,
    entries               bigint  NOT NULL,
    latest_effective_date text    NOT NULL DEFAULT '',
    format                integer NOT NULL,
    taken_at_ns           bigint  NOT NULL,
    snapshot              bytea   NOT NULL,
    PRIMARY KEY (account_id, last_sequence)
);
