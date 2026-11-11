-- The lake's place in its write-ahead stream (the `ha` profile; the
-- resilience spec's section 4; the rulings of 2026-11-08). In `ha` a data
-- plugin's lake writes go to a JetStream stream at three replicas first; the
-- lake's writer takes them from it and commits each batch with the stream's
-- sequence it reached, in the same transaction. A writer that starts -- a
-- takeover, or after the database failed over and lost its last commits --
-- goes on from the sequence held here, so a row is recorded once and nothing
-- the stream held is lost.
--
-- One row per stream. `at_ns` is the stream's time for the sequence;
-- `committed_at_ns` the lake's clock at the commit. Nothing in `tolerant`
-- writes here, and a release before this reads and writes what it did.

CREATE TABLE IF NOT EXISTS lake_write_ahead (
    stream          text   PRIMARY KEY,
    sequence        bigint NOT NULL,
    at_ns           bigint NOT NULL,
    committed_at_ns bigint NOT NULL
);
