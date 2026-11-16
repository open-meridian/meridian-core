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
    -- The write-ahead stream's last sequence as the raise to this epoch
    -- found it: a write the stream held at or below it is admitted whatever
    -- its epoch, so a replay of one already answered is never refused
    -- (review c5a H2; provisional name, item 48).
    admit_through bigint NOT NULL DEFAULT 0,
    -- The identity (creation time, ns) of the stream admit_through is a
    -- place in: a write of another stream -- one made again, numbering from
    -- 1 -- is admitted by nothing (review c5a2 N4; provisional, item 48).
    admit_stream bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (instance, shard)
);
