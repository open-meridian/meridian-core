-- Every instrument named by the platform master's ID (contract v24;
-- decisions/037; spec/the-platform-master-mints-every-instrument-from-a-template).
--
-- A record's one-ID state -- provisional, why, its template and version, the
-- fields its template asks that it lacks, a decommission the platform's feed
-- said, and when the platform was last asked about it -- kept beside it as
-- one JSON document, read and written whole with the record.
ALTER TABLE instrument ADD COLUMN IF NOT EXISTS one_id text NOT NULL DEFAULT '';

-- Every replacement says why (W3.8; the spec's requirement 3) and is
-- numbered in one sequence a reader catches up after (W3.22).
ALTER TABLE instrument_replacement ADD COLUMN IF NOT EXISTS cause integer NOT NULL DEFAULT 0;
ALTER TABLE instrument_replacement ADD COLUMN IF NOT EXISTS sequence bigint;
ALTER TABLE instrument_replacement ADD COLUMN IF NOT EXISTS backfilled boolean NOT NULL DEFAULT false;
ALTER TABLE instrument_replacement ADD COLUMN IF NOT EXISTS gap boolean NOT NULL DEFAULT false;

-- The replacements recorded before v24, backfilled honestly (decisions/031):
-- numbered in the order they were made; a person's merge from v10 as merged,
-- a placeholder's move before v10 as resolved, each saying it was
-- backfilled, and a gap recorded where the facts are missing.
WITH ordered AS (
    SELECT replaced_id, row_number() OVER (ORDER BY replaced_at_ns, replaced_id) AS n
      FROM instrument_replacement WHERE sequence IS NULL
)
UPDATE instrument_replacement r
   SET sequence = ordered.n + COALESCE((SELECT max(sequence) FROM instrument_replacement), 0),
       backfilled = true,
       cause = CASE
           WHEN EXISTS (SELECT 1 FROM instrument_version v
                         WHERE v.operation = 'merge' AND v.merged_instrument_id = r.replaced_id)
               THEN 1
           WHEN EXISTS (SELECT 1 FROM instrument_placeholder p WHERE p.placeholder_id = r.replaced_id)
               THEN 2
           ELSE 0
       END,
       gap = NOT (
           EXISTS (SELECT 1 FROM instrument_version v
                    WHERE v.operation = 'merge' AND v.merged_instrument_id = r.replaced_id)
           OR EXISTS (SELECT 1 FROM instrument_placeholder p WHERE p.placeholder_id = r.replaced_id)
       )
  FROM ordered
 WHERE r.replaced_id = ordered.replaced_id;
CREATE UNIQUE INDEX IF NOT EXISTS instrument_replacement_by_sequence
    ON instrument_replacement (sequence);

-- The platform's template catalogue, every version kept and never changed
-- (W3.24), and each licence list read, its own record (W3.25).
CREATE TABLE IF NOT EXISTS instrument_template (
    template_id      text   NOT NULL,
    template_version text   NOT NULL,
    record           bytea  NOT NULL,
    kept_at_ns       bigint NOT NULL,
    PRIMARY KEY (template_id, template_version)
);
CREATE TABLE IF NOT EXISTS instrument_licence_list (
    version    bigint PRIMARY KEY,
    record     bytea  NOT NULL,
    kept_at_ns bigint NOT NULL
);

-- What the store keeps of its own runs: the replacement feed's sequence read
-- to (W3.27) and the migration's (W3.26).
CREATE TABLE IF NOT EXISTS instrument_state (
    key   text PRIMARY KEY,
    value text NOT NULL
);
