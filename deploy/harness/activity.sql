-- The custodian's activity and each connection's sync status, as the street
-- keeps them (contract v14; W2.10 to W2.14), as stable, sorted lines, which
-- `store activity` prints. Read-only, one statement, every account, ordered
-- bytewise (COLLATE "C"); nothing that changes from run to run -- an
-- account's ID, an activity's, a time, a number in the street's partition --
-- is printed.
--
--   activity|<account>|<source>|<external activity id>|<kind>|<instrument>|<trade date>|<units>
--   sync|<account>|<source>|<external account>|<state>|<history from>
--   re-resolution|<account>|<source>|<external activity id>|<n>|<instrument>
--
-- <account> is the account's name, empty for a sync status of an external
-- account nothing links. <instrument> as `store street` prints it, empty
-- where the activity's did not resolve. <kind> (ActivityKind) and <state>
-- (SyncState) are their numbers, 0 for not known. <units> is printed at the
-- scale it was stated with, empty where none was stated. An activity is one
-- line however often it was sent; a sync status line is the latest the
-- street heard for that connection. A re-resolution (contract v15) is a line
-- of its own beside its activity's, which stays as first recorded: <n> its
-- place among the activity's re-resolutions, 1 the first, <instrument> empty
-- where it is unresolved again.
--
-- Apart from `store street`, whose lines a plugin's e2e already compares with
-- a file it keeps: a plugin reading these lines asks for them by name.

WITH shown AS (
    SELECT instrument_id,
           'local(' || string_agg(said, ', ' ORDER BY said COLLATE "C") || ')' AS instrument
      FROM (SELECT instrument_id,
                   scheme || ':' || value || CASE WHEN source <> '' THEN '@' || source ELSE '' END AS said
              FROM instrument_identifier
             WHERE instrument_id LIKE 'LCL-%' AND valid_to_ns IS NULL) identifiers
     GROUP BY instrument_id
),
named AS (
    SELECT account_id, name FROM config_account
),
latest AS (
    SELECT DISTINCT ON (account_id, source, external_account_id)
           account_id, source, external_account_id, state, history_from
      FROM sync_status
     ORDER BY account_id, source, external_account_id, sequence DESC
),
lines AS (
    SELECT 'activity|' || coalesce(n.name, a.account_id)
           || '|' || a.source
           || '|' || a.external_activity_id
           || '|' || a.kind::text
           || '|' || coalesce(i.instrument, a.instrument_id)
           || '|' || a.trade_date
           || '|' || coalesce(a.units::text, '') AS line
      FROM activity a
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i USING (instrument_id)
    UNION ALL
    SELECT 'sync|' || CASE WHEN l.account_id = '' THEN '' ELSE coalesce(n.name, l.account_id) END
           || '|' || l.source
           || '|' || l.external_account_id
           || '|' || l.state::text
           || '|' || l.history_from
      FROM latest l
      LEFT JOIN named n USING (account_id)
    UNION ALL
    SELECT 're-resolution|' || coalesce(n.name, r.account_id)
           || '|' || a.source
           || '|' || a.external_activity_id
           || '|' || (row_number() OVER (PARTITION BY r.activity_id ORDER BY r.sequence))::text
           || '|' || coalesce(i.instrument, r.instrument_id)
      FROM activity_re_resolution r
      JOIN activity a USING (activity_id)
      LEFT JOIN named n ON n.account_id = r.account_id
      LEFT JOIN shown i ON i.instrument_id = r.instrument_id
)
SELECT line FROM lines ORDER BY line COLLATE "C";
