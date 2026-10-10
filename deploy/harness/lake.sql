-- The lake (contract v18, W10), as stable, sorted lines, which `store lake`
-- prints: what a run asserts the lake kept. One line per row version kept,
-- per priority, per want change, per serving of a dataset not kept, per
-- removal, per instance's misses and per alias:
--
--   row|dataset|price or bar|sequence|row key|version|subject|kind or interval ns|venue|business date|previous sequence|unconverted
--   priority|data type|kind|changes
--   want|dataset|asked, answered, declined or withdrawn|subjects|reason
--   served|dataset|first sequence|last sequence|subjects|fields|readers
--   removal|dataset|rows removed|why
--   miss|instance|misses
--   alias|replaced|stays
--
-- Never a value: a row's price or bar is its body, which the reading roles
-- read under their entitlements; nor a time a run cannot state. A want's ID
-- is the lake's, so a line names its dataset and subjects instead.
SELECT line FROM (
    SELECT 'row|' || dataset || '|' || CASE data_type WHEN 1 THEN 'price' WHEN 2 THEN 'bar' ELSE '?' END
        || '|' || sequence || '|' || row_key || '|' || version || '|' || subject || '|'
        || CASE data_type WHEN 2 THEN interval_ns::text ELSE kind::text END || '|' || venue_id || '|'
        || COALESCE(business_date::text, '') || '|' || previous_sequence || '|' || unconverted AS line
      FROM lake_row
    UNION ALL
    SELECT 'priority|' || data_type || '|' || kind || '|' || count(*)
      FROM lake_priority_change GROUP BY data_type, kind
    UNION ALL
    SELECT 'want|' || dataset || '|'
        || CASE change WHEN 1 THEN 'asked' WHEN 2 THEN 'answered' WHEN 3 THEN 'declined'
                       WHEN 4 THEN 'withdrawn' ELSE '?' END
        || '|' || array_to_string(subjects, ',') || '|' || reason
      FROM lake_want_change
    UNION ALL
    SELECT 'served|' || dataset || '|' || first_sequence || '|' || last_sequence || '|'
        || array_to_string(subjects, ',') || '|' || array_to_string(fields, ',') || '|'
        || array_to_string(readers, ',')
      FROM lake_served
    UNION ALL
    SELECT 'removal|' || dataset || '|' || rows_removed || '|' || why
      FROM lake_removal
    UNION ALL
    SELECT 'miss|' || instance || '|' || misses
      FROM lake_miss
    UNION ALL
    SELECT 'alias|' || replaced || '|' || stays
      FROM lake_alias
) lines
ORDER BY line COLLATE "C";
