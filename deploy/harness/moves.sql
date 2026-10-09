-- An edge plugin's moves of its raw records (contract v16; W4.13), the
-- holds and archives a deployment admin set (W6.25, W8.7), and from contract
-- v17 each setting change (W6.11), as stable, sorted lines, which `store
-- moves` prints: what a run asserts the conductor recorded, each change its
-- own record, naming the person, and the delegation, the client and the
-- note where there were any (contract v17). One line per move, hold,
-- archive and setting change:
--
--   move|instance|kind|unit|outcome|records|first received ns|last received ns|rule|person
--   hold|role|days|write-once|set by|delegation|client|note
--   archive|instance|allowed|most bytes|set by|delegation|client|note
--   setting|instance|name|set, cleared, gap or redacted|secret|set by|delegation|client|note
--
-- Never a record's content, nor a setting's value: a table's rows are its
-- value, and a secret's is never in the conductor's records.
SELECT line FROM (
    SELECT 'move|' || instance_id || '|' || record_kind || '|' || unit || '|'
        || CASE outcome WHEN 1 THEN 'archived' WHEN 2 THEN 'restored' WHEN 3 THEN 'returned'
                        WHEN 4 THEN 'deleted' ELSE '?' END
        || '|' || record_count || '|' || first_received_ns || '|' || last_received_ns || '|'
        || rule || '|' || person AS line
      FROM config_record_move
    UNION ALL
    SELECT 'hold|' || role || '|' || days || '|' || write_once || '|' || changed_by
        || '|' || through_delegation || '|' || client_name || '|' || note
      FROM config_hold_change
    UNION ALL
    SELECT 'archive|' || instance_id || '|' || allowed || '|' || most_bytes || '|' || changed_by
        || '|' || through_delegation || '|' || client_name || '|' || note
      FROM config_archive_change
    UNION ALL
    SELECT 'setting|' || plugin_instance_id || '|' || name || '|'
        || CASE action WHEN 1 THEN 'set' WHEN 2 THEN 'cleared' WHEN 3 THEN 'gap'
                       WHEN 4 THEN 'redacted' ELSE '?' END
        || '|' || secret || '|' || changed_by || '|' || through_delegation || '|' || client_name
        || '|' || note
      FROM config_plugin_setting_change
) lines
ORDER BY line;
