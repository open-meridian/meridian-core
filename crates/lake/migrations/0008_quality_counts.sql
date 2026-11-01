-- The datasets' listing's quality counts (contract v29, W10.9; ruling 619
-- (2)): per dataset, the rows in force -- each row key at the version a
-- default read serves (ruling 1006 (b)) -- whose effective state is valid,
-- suspicious or invalid, served without counting every row (ruling 816).
--
-- A row key's version in force is its latest, and a rate's its latest by
-- published time, then by version (contract v27, W10.4): a late-arriving
-- older publication is kept as the next version and never takes force.
-- lake_row.published_at_ns keeps a rate row's published time -- its
-- published source time, else when the lake recorded it -- NULL on every
-- other data type; `migrate` fills it in for the rate rows recorded before
-- this, read from their bodies by lake_row_unpublished.
--
-- lake_partition.in_force is the row keys a dataset keeps, each counted
-- once, at its version in force: a new row key adds one as it is recorded, a
-- new version of a key held replaces its last and adds none, and retention
-- takes one off for each row key it leaves no version of. Filled here once
-- from the rows kept. lake_row_quality holds only the rows recorded
-- suspicious or invalid; those escalated later are found from
-- lake_quality_assessment, which holds only escalations; either is counted
-- only while it is its row key's version in force (lake_row_version), so a
-- row corrected by a new version counts under the new version's state.
-- Valid is what is in force less the two.
--
-- A release before this records rows without moving in_force: the lake is
-- upgraded whole, its migration run before its start.

ALTER TABLE lake_row ADD COLUMN IF NOT EXISTS published_at_ns bigint;
CREATE INDEX IF NOT EXISTS lake_row_unpublished
    ON lake_row (dataset, sequence) WHERE data_type = 6 AND published_at_ns IS NULL;

ALTER TABLE lake_partition ADD COLUMN IF NOT EXISTS in_force bigint;
UPDATE lake_partition p
   SET in_force = c.n
  FROM (SELECT dataset, count(*) AS n
          FROM (SELECT DISTINCT dataset, data_type, row_key FROM lake_row) keys
         GROUP BY dataset) c
 WHERE p.dataset = c.dataset AND p.in_force IS NULL;
UPDATE lake_partition SET in_force = 0 WHERE in_force IS NULL;
ALTER TABLE lake_partition ALTER COLUMN in_force SET DEFAULT 0;
ALTER TABLE lake_partition ALTER COLUMN in_force SET NOT NULL;

CREATE INDEX IF NOT EXISTS lake_row_quality
    ON lake_row (dataset, sequence) WHERE quality IN (2, 3);
