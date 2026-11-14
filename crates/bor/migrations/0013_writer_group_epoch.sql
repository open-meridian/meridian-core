-- Each writer group's epoch (step 3d; spec/a-deployment-scales-out-and-
-- fails-over-by-configuration sections 6.3 and 6.8; `book_writer_group`
-- approved 2026-11-14, the same as the lake's `lake_writer_group`). Each
-- writer group past the first keeps its epoch here, raised at its takeover
-- with its partitions', and kept while it owns none: a partition moved to
-- it takes this epoch, so its writer is never fenced by a partition moved
-- back. Group 0's is the control partition's.
--
-- Empty with one writer group, the default: nothing reads or writes it.
CREATE TABLE IF NOT EXISTS book_writer_group (
    writer_group integer PRIMARY KEY,
    epoch        bigint  NOT NULL,
    origin       text    NOT NULL DEFAULT ''
);

-- A deployment already writing at several groups keeps each group's epoch:
-- the one its partitions' heads name it under. A partition named by no
-- group is its rendezvous owner's, which its writer records at its start.
INSERT INTO book_writer_group (writer_group, epoch, origin)
SELECT writer_group, max(epoch), coalesce(max(origin), '')
  FROM book_partition_head
 WHERE writer_group IS NOT NULL AND writer_group > 0 AND partition <> 'control'
 GROUP BY writer_group
ON CONFLICT (writer_group) DO NOTHING;
