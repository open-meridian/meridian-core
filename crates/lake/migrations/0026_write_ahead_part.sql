-- A writer group's part of a write (step 3d; spec/a-deployment-scales-out-
-- and-fails-over-by-configuration section 6.7; `lake_write_ahead_part`
-- ruled 2026-11-15). In `ha` every group takes the whole write-ahead stream
-- and commits its own ranges' rows of each write; the group owning the
-- dataset's own partition answers the write, once every other group has
-- passed it. A group that commits rows of a write of a dataset whose ranges
-- span groups keeps what they did here, in the same transaction as the rows
-- and its position in the stream, so the answer counts every part.
--
-- `stream` is the group's position's key (`lake_write_ahead.stream`), and
-- `sequence` the write's in the stream. A row is let go ten minutes after
-- its commit, with a later batch: its answer was given long before.
-- Nothing in `tolerant` writes here, nor does a lake of one writer group.

CREATE TABLE IF NOT EXISTS lake_write_ahead_part (
    stream          text    NOT NULL,
    sequence        bigint  NOT NULL,
    recorded        integer NOT NULL,
    restated        integer NOT NULL,
    unchanged       integer NOT NULL,
    committed_at_ns bigint  NOT NULL,
    PRIMARY KEY (stream, sequence)
);
