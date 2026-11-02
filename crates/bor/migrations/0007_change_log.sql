-- The book's change log (step 3a; spec/one-data-access-model section 3, "The
-- change log"; rulings 1046, 1056, 1057, 1060).
--
-- A partition's log is its journal, in sequence, as it has always been
-- numbered: each entry under its partition's advisory lock and head row, in
-- its own transaction, with no holes. What the log adds:
--
-- - each partition's head row carries its writer's `epoch`, checked under
--   the head row's lock in every command, and its `origin`, the deployment
--   that began it; `control` is the book's own partition, as it has been
--   since the first migration;
-- - every entry carries the epoch it was written under and, where it was
--   brought in from elsewhere, its origin (NULL for one made here: the
--   partition's own).
--
-- The journal is append-only and these columns are added with constant
-- defaults: no entry is rewritten, and its trigger is not asked.
ALTER TABLE book_partition_head
    ADD COLUMN IF NOT EXISTS epoch  bigint NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS origin text   NOT NULL DEFAULT '';

ALTER TABLE book_entry
    ADD COLUMN IF NOT EXISTS epoch  bigint NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS origin text;

CREATE INDEX IF NOT EXISTS book_entry_by_partition
    ON book_entry (partition, last_sequence);
