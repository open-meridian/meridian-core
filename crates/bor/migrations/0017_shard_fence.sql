-- A plugin shard's epoch as this database last saw it (step 3d, checkpoint
-- 5, item 32; `book_shard_fence` approved 2026-11-15). A sidecar that takes a
-- shard's Lease has the lake raise the shard's epoch, and stamps it on every
-- write it sends for its plugin (MessageMeta.shard, .shard_epoch). Each
-- writer group's database keeps the highest it has seen of each shard and
-- checks a write's in the write's own transaction: lower is refused
-- (REFUSAL_REASON_SHARD_FENCED), equal passes, higher raises. Rows are kept
-- for good, so an epoch never goes back.
CREATE TABLE IF NOT EXISTS book_shard_fence (
    instance text   NOT NULL,
    shard    bigint NOT NULL,
    epoch    bigint NOT NULL,
    PRIMARY KEY (instance, shard)
);
