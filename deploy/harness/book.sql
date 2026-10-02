-- The book of record as lines, for a test to compare with a file it keeps
-- (contract v8, W9).
--
-- Read-only, and one statement. Run with `psql -At` against the harness's
-- database, where every store shares one schema, so an account's name is
-- read beside its records. Every account, ordered bytewise (COLLATE "C"), and
-- nothing that changes from run to run -- an account's ID, an entry's, a
-- lot's or a break's, a number in a partition, a time -- is printed.
--
--   attributes|<account>|<base currency>|<lot relief>|<opening balance as of, or empty>
--   position|<account>|<instrument>|<side>|<trade date>|<settled, empty when unknown>|<not stated>|<effective date>
--   pending|<account>|<instrument>|<side>|<value date, empty when not stated>|<quantity>|<failing>
--   lot|<account>|<instrument>|<side>|<order opened>|<open>|<original>|<cost> <currency>|<acquired>|<source>
--   break|<account>|<subject>|<category>|<state>|<first seen>|<last seen>|<recorded by>|<confirmed cause>|<resolved by>
--   figures|<account>|<external account>/<segment>|<business date>
--   entry|<account>|<kind>|<effective date>|<actor>
--
-- <account> is the account's name. <instrument> is its INS- ID, or for the
-- deployment's placeholder the identifiers it stands for, sorted, as
-- street.sql prints it. A number is printed at the scale it was stated with
-- (decisions/023); what is unknown is empty, never zero. <lot relief>,
-- <category>, <state>, <confirmed cause> and <source> are the enum's number
-- (0 is none). <recorded by> and <actor> are a person's subject, `instance
-- <id>` for a finding a plugin sent as itself, or `book` for the book's own
-- act. A position the book removed after a placeholder's move is kept as a
-- tombstone and not printed. Entries are printed in the order they were
-- made in each account.
--
-- The format is the plugin harness's published interface, versioned with
-- the image that carries this file; `make e2e-book` in meridian-core holds
-- it, comparing the book before and after `meridian-bor rebuild`.

WITH shown AS (
    SELECT placeholder_id AS instrument_id,
           'placeholder(' || string_agg(said, ', ' ORDER BY said COLLATE "C") || ')' AS instrument
      FROM (SELECT placeholder_id,
                   scheme || ':' || value || CASE WHEN source <> '' THEN '@' || source ELSE '' END AS said
              FROM instrument_placeholder_identifier) identifiers
     GROUP BY placeholder_id
),
named AS (
    SELECT account_id, name FROM config_account
),
side AS (
    SELECT 1 AS side, 'long' AS said UNION ALL SELECT 2, 'short'
),
lines AS (
    SELECT 1 AS part, coalesce(n.name, a.account_id) AS account, '' AS at,
           'attributes|' || coalesce(n.name, a.account_id) || '|' || a.base_currency_code
           || '|' || a.lot_relief_default::text || '|' || a.opening_as_of AS line
      FROM book_attributes a LEFT JOIN named n USING (account_id)
    UNION ALL
    SELECT 2, coalesce(n.name, p.account_id), coalesce(i.instrument, p.instrument_id),
           'position|' || coalesce(n.name, p.account_id)
           || '|' || coalesce(i.instrument, p.instrument_id)
           || '|' || s.said
           || '|' || p.trade_date_quantity::text
           || '|' || coalesce(p.settled_quantity::text, '')
           || '|' || p.not_stated_quantity::text
           || '|' || p.effective_date
      FROM book_position p
      JOIN side s USING (side)
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i USING (instrument_id)
     WHERE NOT p.removed
    UNION ALL
    SELECT 3, coalesce(n.name, q.account_id), coalesce(i.instrument, q.instrument_id),
           'pending|' || coalesce(n.name, q.account_id)
           || '|' || coalesce(i.instrument, q.instrument_id)
           || '|' || s.said
           || '|' || q.value_date
           || '|' || q.quantity::text
           || '|' || q.failing::text
      FROM book_pending q
      JOIN side s USING (side)
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i USING (instrument_id)
    UNION ALL
    SELECT 4, coalesce(n.name, l.account_id), coalesce(i.instrument, l.instrument_id),
           'lot|' || coalesce(n.name, l.account_id)
           || '|' || coalesce(i.instrument, l.instrument_id)
           || '|' || s.said
           || '|' || l.ordinal::text
           || '|' || l.open_quantity::text
           || '|' || l.original_quantity::text
           || '|' || coalesce(l.cost::text || ' ' || l.cost_currency, '')
           || '|' || l.acquired_date
           || '|' || l.source::text
      FROM book_lot l
      JOIN side s USING (side)
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i USING (instrument_id)
    UNION ALL
    SELECT 5, coalesce(n.name, b.account_id), b.subject || b.first_seen || b.category::text,
           'break|' || coalesce(n.name, b.account_id)
           || '|' || coalesce(i.instrument || substr(b.subject, length(b.instrument_id) + 1), b.subject)
           || '|' || b.category::text
           || '|' || b.state::text
           || '|' || b.first_seen
           || '|' || b.last_seen
           || '|' || b.recorded_by
           || '|' || b.cause::text
           || '|' || b.resolution
      FROM book_break b
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i ON i.instrument_id = b.instrument_id AND b.subject LIKE b.instrument_id || '|%'
    UNION ALL
    SELECT 6, coalesce(n.name, f.account_id), f.agreement_key || f.business_date,
           'figures|' || coalesce(n.name, f.account_id)
           || '|' || split_part(f.agreement_key, chr(31), 2) || '/' || split_part(f.agreement_key, chr(31), 3)
           || '|' || f.business_date
      FROM book_figures f
      LEFT JOIN named n USING (account_id)
    UNION ALL
    SELECT 7, coalesce(n.name, e.account_id), lpad(e.first_sequence::text, 20, '0'),
           'entry|' || coalesce(n.name, e.account_id)
           || '|' || e.kind
           || '|' || e.effective_date
           || '|' || e.actor
      FROM book_entry e
      LEFT JOIN named n USING (account_id)
)
SELECT line FROM lines
 ORDER BY part, account COLLATE "C", at COLLATE "C", line COLLATE "C";
