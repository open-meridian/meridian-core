-- The datasets' listing's quality counts (contract v29, W10.9; ruling 619
-- (2)): per dataset, the rows in force -- each row key at its latest
-- version -- whose effective state is valid, suspicious or invalid, served
-- without counting every row (ruling 816).
--
-- lake_partition.in_force is the row keys a dataset keeps, each counted
-- once, at its latest version: a new row key adds one as it is recorded, a
-- new version of a key held replaces its last and adds none, and retention
-- takes one off for each row key it leaves no version of. Filled here once
-- from the rows kept. lake_row_quality holds only the rows recorded
-- suspicious or invalid; those escalated later are found from
-- lake_quality_assessment, which holds only escalations; either is counted
-- only while no later version of its row key is kept (lake_row_version), so
-- a row corrected by a new version counts under the new version's state.
-- Valid is what is in force less the two.
--
-- A release before this records rows without moving in_force: the lake is
-- upgraded whole, its migration run before its start.

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
