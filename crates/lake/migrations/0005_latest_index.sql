-- The latest read served by an index (ruling 770): C3 part 2's bench found a
-- chain's latest read took 5.1 s at 400,000 rows of one underlying, every
-- row the subject kept in its datasets scanned to choose a few hundred.
--
-- lake_row_latest holds each row by the key its latest is chosen per --
-- dataset, data type, first subject, kind, venue, length and latest_key --
-- newest first, so a read skips from key to key, one probe each, and takes
-- each key's latest from the top of its own entries.
-- lake_row_by_later_subjects finds a row naming an asked subject after its
-- first, which the first index does not hold under that subject.
-- lake_row_unkeyed holds the rows not yet given their latest_key (recorded
-- by a release before migration 4, during a rolling upgrade), each a key of
-- its own until `migrate` fills it in; it also serves that backfill.
--
-- lake_row_by_subject goes: lake_row_latest leads with the same three
-- columns, so the subject's previous sequence a record looks up is found
-- there, and a read of the
-- latest on a table without statistics yet is never planned on it with a
-- sort in place of the skip.
--
-- Indexes only: nothing recorded changes, and a release before this reads
-- the same rows.

CREATE INDEX IF NOT EXISTS lake_row_latest
    ON lake_row (dataset, data_type, subject, kind, venue_id, interval_ns, latest_key,
                 valid_from_ns DESC, sequence DESC);
CREATE INDEX IF NOT EXISTS lake_row_by_later_subjects
    ON lake_row USING gin ((subjects[2:]));
CREATE INDEX IF NOT EXISTS lake_row_unkeyed
    ON lake_row (dataset, sequence) WHERE latest_key IS NULL;
DROP INDEX IF EXISTS lake_row_by_subject;
