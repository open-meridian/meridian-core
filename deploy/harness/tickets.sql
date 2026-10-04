-- The dashboard's tickets (contract v13; W6.21 to W6.24) as stable, sorted
-- lines, which `store tickets` prints: what a run asserts kept, folded and
-- changed, and that nothing else did. One line per ticket, per record it
-- names and per note:
--
--   ticket|ID|concerns kind|concerns instance|filed provenance|filed person|filed instance|state|owner|due|seen count|suspect|matched rules
--   reference|ID|position|kind|value|account|found in its text
--   note|ID|number|kind|author provenance|author person|author client|suspect
--
-- Never a ticket's title, text or a note's text: those are the rows' to
-- answer, and a run asks the rows.
SELECT line FROM (
    SELECT 'ticket|' || ticket_id || '|' || concerns_kind || '|' || concerns_instance || '|'
        || filed_provenance || '|' || filed_person || '|' || filed_instance || '|' || state || '|'
        || owner_name || '|' || due || '|' || seen_count || '|' || suspect || '|'
        || array_to_string(matched_rules, ',') AS line
      FROM dashboard_ticket
    UNION ALL
    SELECT 'reference|' || ticket_id || '|' || position || '|' || kind || '|' || value || '|'
        || account_id || '|' || found_in_text
      FROM dashboard_ticket_reference
    UNION ALL
    SELECT 'note|' || ticket_id || '|' || number || '|'
        || CASE kind WHEN 1 THEN 'note' WHEN 2 THEN 'advice' WHEN 3 THEN 'answer' WHEN 4 THEN 'change' ELSE '?' END
        || '|' || author_provenance || '|' || author_person || '|' || author_client || '|' || suspect
      FROM dashboard_ticket_note
) lines
ORDER BY line;
