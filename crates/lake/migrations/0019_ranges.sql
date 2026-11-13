-- Hash ranges (step 3d; spec/a-deployment-scales-out-and-fails-over-by-
-- configuration section 6.1; `<dataset>/<lo>-<hi>` approved 2026-11-12). A
-- lake partition is a range of one dataset's 16-bit hash space: the first
-- two bytes of SHA-256 over a row's first subject. Each range keeps its own
-- head row here -- its sequence, epoch and kept_from -- keyed by its
-- partition ID in `dataset`: the dataset's own ID for the full range, every
-- partition before 3d's among them, and `<dataset>/<lo>-<hi>` for a range
-- split from it.
--
-- A split closes its parent at its head (`closed_at`) and opens two
-- children naming it (`parent`), each numbering on from the parent's close.
-- A closed range's rows are kept and read up to its close.
ALTER TABLE lake_partition
    ADD COLUMN IF NOT EXISTS lo        integer NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS hi        integer NOT NULL DEFAULT 65535,
    ADD COLUMN IF NOT EXISTS parent    text,
    ADD COLUMN IF NOT EXISTS closed_at bigint;

-- Each row's range and its anchor: the hash of its row key's first
-- version's first subject, which places every later version of the key, so
-- a key restated under the record that replaced its subject stays in the
-- range it was first written to (ruled 2026-11-12). Both empty on a row kept
-- before 3d: its dataset's full range, its anchor its own subject's hash.
ALTER TABLE lake_row
    ADD COLUMN IF NOT EXISTS partition text,
    ADD COLUMN IF NOT EXISTS anchor    integer;

-- The records kept beside a row by its sequence name its range and its row
-- key too: two ranges split from one number on from the same sequence.
ALTER TABLE lake_closed_day ADD COLUMN IF NOT EXISTS row_key text;
-- Every re-check kept so far names its row's key: no range is split yet, so
-- its dataset and sequence name one row.
UPDATE lake_closed_day c SET row_key = r.row_key
  FROM lake_row r
 WHERE c.row_key IS NULL AND r.dataset = c.dataset AND r.sequence = c.sequence;
ALTER TABLE lake_served ADD COLUMN IF NOT EXISTS partition text;
