-- The gap before the street listened (decisions/031, point 4; ruled
-- 2026-10-05 for sdk-contract/the-custodians-activity-contract).
--
-- The street keeps each sync status from migration 0008 on. Before it heard a
-- connection's first, that connection's sync statuses were kept nowhere a
-- record could be backfilled from: the dashboard held only the latest, in
-- memory. So the first sync status kept for each connection -- its account,
-- source and external account, as a read of the latest groups them -- says so:
-- `not_known_before` is why nothing is known of the connection's sync status
-- before that record's own time, recorded once and only there, never left as
-- silence. Empty on every later one.
--
-- A database that already kept sync statuses under 0008 has its first one for
-- each connection marked here, saying it was marked by this migration and on
-- what; its recorded time is its own, never back-dated. A connection already
-- marked is left as it is, so a migration that finds nothing to do touches
-- nothing.

ALTER TABLE sync_status ADD COLUMN IF NOT EXISTS not_known_before text NOT NULL DEFAULT '';

UPDATE sync_status first
   SET not_known_before = 'not known before: the street keeps each sync status from the first '
                          'it hears for a connection, and none was kept before it to backfill '
                          'from; marked by migration 0009 on the first one this street kept'
 WHERE first.sequence = (SELECT min(held.sequence) FROM sync_status held
                          WHERE held.account_id = first.account_id
                            AND held.source = first.source
                            AND held.external_account_id = first.external_account_id)
   AND NOT EXISTS (SELECT 1 FROM sync_status marked
                    WHERE marked.account_id = first.account_id
                      AND marked.source = first.source
                      AND marked.external_account_id = first.external_account_id
                      AND marked.not_known_before <> '');

-- Once per connection.
CREATE UNIQUE INDEX IF NOT EXISTS sync_status_gap_once
    ON sync_status (account_id, source, external_account_id)
    WHERE not_known_before <> '';
