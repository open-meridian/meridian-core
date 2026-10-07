-- An edge plugin's moves of its raw records (contract v16; W4.13), and the
-- holds and archives a deployment admin set (W6.25, W8.7), as stable, sorted
-- lines, which `store moves` prints: what a run asserts the conductor
-- recorded, each change its own record. One line per move, hold and archive
-- change:
--
--   move|instance|kind|unit|outcome|records|first received ns|last received ns|rule|person
--   hold|role|days|write-once|set by
--   archive|instance|allowed|most bytes|set by
--
-- Never a record's content: the conductor holds none.
SELECT line FROM (
    SELECT 'move|' || instance_id || '|' || record_kind || '|' || unit || '|'
        || CASE outcome WHEN 1 THEN 'archived' WHEN 2 THEN 'restored' WHEN 3 THEN 'returned'
                        WHEN 4 THEN 'deleted' ELSE '?' END
        || '|' || record_count || '|' || first_received_ns || '|' || last_received_ns || '|'
        || rule || '|' || person AS line
      FROM config_record_move
    UNION ALL
    SELECT 'hold|' || role || '|' || days || '|' || write_once || '|' || changed_by
      FROM config_hold_change
    UNION ALL
    SELECT 'archive|' || instance_id || '|' || allowed || '|' || most_bytes || '|' || changed_by
      FROM config_archive_change
) lines
ORDER BY line;
