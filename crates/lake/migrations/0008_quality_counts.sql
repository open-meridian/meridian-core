-- The datasets' listing's quality counts (contract v29, W10.9; ruling 619
-- (2)): per dataset, the rows the lake keeps whose effective state is valid,
-- suspicious or invalid, served without counting every row (ruling 816).
--
-- lake_partition.kept is the rows a dataset keeps, moved with its head as
-- rows are recorded and down as retention removes them; filled here once
-- from the rows kept. lake_row_quality holds only the rows recorded
-- suspicious or invalid; those escalated later are found from
-- lake_quality_assessment, which holds only escalations. Valid is what is
-- kept less the two.
--
-- A release before this records rows without moving kept: the lake is
-- upgraded whole, its migration run before its start.

ALTER TABLE lake_partition ADD COLUMN IF NOT EXISTS kept bigint;
UPDATE lake_partition p
   SET kept = c.n
  FROM (SELECT dataset, count(*) AS n FROM lake_row GROUP BY dataset) c
 WHERE p.dataset = c.dataset AND p.kept IS NULL;
UPDATE lake_partition SET kept = 0 WHERE kept IS NULL;
ALTER TABLE lake_partition ALTER COLUMN kept SET DEFAULT 0;
ALTER TABLE lake_partition ALTER COLUMN kept SET NOT NULL;

CREATE INDEX IF NOT EXISTS lake_row_quality
    ON lake_row (dataset, sequence) WHERE quality IN (2, 3);
