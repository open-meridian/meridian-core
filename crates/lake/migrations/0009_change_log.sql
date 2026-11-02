-- The lake's change log (step 3a; spec/one-data-access-model section 3, "The
-- change log"; rulings 1046, 1056, 1057, 1060).
--
-- A dataset's log is its rows in sequence, and the rows it served and did
-- not keep, numbered from its head in each batch's own transaction, as the
-- lake has always done. What the log adds:
--
-- - each partition's head row carries its writer's `epoch`, checked under
--   the head's lock in every batch, and its `origin`, the deployment that
--   began it; every row, and every range served, carries the epoch it was
--   written under and, where it was brought in from elsewhere, its origin
--   (NULL for one made here: the partition's own);
-- - `kept_from`: the lowest sequence the partition's log still reaches. A
--   removal past retention raises it, so a follower behind it is told so and
--   reloads the partition at its head rather than missing what was removed;
-- - the lake's `control` partition (ruling 1060), beside its datasets in a
--   table of its own so no dataset count or watermark ever names it: where
--   its assignments, takeovers, moves, switches and findings are kept from
--   step 3b. Its epoch is the writer's: a dataset begun by a writer takes it,
--   checked against this row.
--
-- Every column added has a constant default, so no row is rewritten.
ALTER TABLE lake_partition
    ADD COLUMN IF NOT EXISTS epoch     bigint NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS origin    text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS kept_from bigint NOT NULL DEFAULT 1;

ALTER TABLE lake_row
    ADD COLUMN IF NOT EXISTS epoch  bigint NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS origin text;

ALTER TABLE lake_served
    ADD COLUMN IF NOT EXISTS epoch  bigint NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS origin text;

CREATE INDEX IF NOT EXISTS lake_served_by_sequence
    ON lake_served (dataset, last_sequence);

-- A dataset whose rows were removed before its log kept a floor: its log
-- reaches from its lowest row still kept.
UPDATE lake_partition p
   SET kept_from = coalesce((SELECT min(r.sequence) FROM lake_row r WHERE r.dataset = p.dataset),
                            p.head + 1)
 WHERE EXISTS (SELECT 1 FROM lake_removal m WHERE m.dataset = p.dataset);

CREATE TABLE IF NOT EXISTS lake_change_head (
    partition text   PRIMARY KEY CHECK (partition = 'control'),
    sequence  bigint NOT NULL DEFAULT 0 CHECK (sequence >= 0),
    epoch     bigint NOT NULL DEFAULT 1,
    origin    text   NOT NULL DEFAULT ''
);
INSERT INTO lake_change_head (partition) VALUES ('control') ON CONFLICT (partition) DO NOTHING;
