-- Each subject's last sequence in a dataset, read by an index (ruled
-- 2026-11-10, the record path batched): a batch numbers each row after its
-- subject's previous, which it reads for every subject the batch names in
-- one look.
--
-- 0005 dropped lake_row_by_subject on the word that lake_row_latest served
-- this lookup, as it leads with the same three columns. It finds the rows,
-- but not their last: its entries for one subject are ordered by kind,
-- venue, length, latest_key and valid time before sequence, so max(sequence)
-- read every entry the subject had -- 22,200 a subject in the gate's test
-- database, 25 ms a batch, growing with the subject's history. This index
-- answers it from the top of the subject's own entries, one probe.
--
-- Keyed by `subject || ''`, never the plain column: 0005 also dropped
-- lake_row_by_subject so that a read of the latest on a table without
-- statistics yet is never planned on an index of the subject with a sort in
-- place of lake_row_latest's skip. An index on the plain column was chosen
-- so, and a chain's latest read at 100,000 rows took 8.1 s against 33 ms
-- (part 4's bench, no ANALYZE after the load). Only the previous-sequence
-- lookup, which names the expression, can use this one.
--
-- An index only: nothing recorded changes, and a release before this reads
-- and writes the same rows.

CREATE INDEX IF NOT EXISTS lake_row_subject_sequence
    ON lake_row (dataset, data_type, (subject || ''), sequence);
