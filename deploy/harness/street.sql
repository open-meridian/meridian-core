-- The street store as lines, for a test to compare with a file it keeps.
--
-- Read-only, and one statement. Run with `psql -At` against the harness's
-- database, where every store shares one schema, so an account's name is
-- read beside its rows. Every account, ordered bytewise (COLLATE "C"), so the
-- output does not depend on the database's collation; and nothing that
-- changes from run to run -- an account's ID, a placeholder's ID, a read
-- time, a statement's ID -- is printed.
--
--   position|<account>|<instrument>|<side>|<quantity>|<settle-date quantity>|<market value> <currency>|<in-cash>
--   assumed|<account>|<instrument>|<side>
--   statement|<account>|<source>|<expected rows>|<complete or open>|<buying power>|<margin requirement>|<maintenance excess>|<currency assumed>
--
-- <account> is the account's name. <instrument> is its INS- ID, or for the
-- deployment's placeholder the identifiers it stands for, sorted:
-- `placeholder(<scheme>:<value>[@<source>], ...)`. With no platform, as in
-- the harness, every instrument is a placeholder. A number is printed at the
-- scale it was stated with (decisions/023), and what was not reported is
-- empty, never zero. <in-cash> is `in-cash` for a position the venue also
-- counts in cash.
--
-- A position is the account's custodial position. An `assumed` line is a row
-- of a latest statement whose currency the connector assumed. A statement is
-- the latest each source recorded for each account, by its rows' account: a
-- statement none of whose rows was recorded -- every one refused, its account
-- unlinked -- belongs to no account and is not printed.
--
-- The format is the plugin harness's published interface, versioned with
-- the image that carries this file; `make interop` and `make harness-check`
-- in meridian-core hold it.

WITH shown AS (
    -- Each placeholder as the identifiers it stands for.
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
latest AS (
    -- The latest statement of each source for each account its rows name.
    SELECT DISTINCT ON (held.account_id, s.source)
           held.account_id, s.*
      FROM statement s
      JOIN (SELECT DISTINCT account_id, statement_id FROM holding) held USING (statement_id)
     -- Read at the same moment, the one recorded last: an ID begins with
     -- the time it was minted, and is compared bytewise.
     ORDER BY held.account_id, s.source, s.read_at_ns DESC, s.statement_id COLLATE "C" DESC
),
lines AS (
    SELECT 'position|' || coalesce(n.name, p.account_id)
           || '|' || coalesce(i.instrument, p.instrument_id)
           || '|' || p.side
           || '|' || p.quantity::text
           || '|' || coalesce(p.settle_date_quantity::text, '')
           || '|' || coalesce(p.market_value::text || ' ' || p.currency, '')
           || '|' || CASE WHEN p.also_counted_in_cash THEN 'in-cash' ELSE '' END AS line
      FROM custodial_position p
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i USING (instrument_id)
    UNION
    SELECT 'assumed|' || coalesce(n.name, h.account_id)
           || '|' || coalesce(i.instrument, h.instrument_id)
           || '|' || h.side
      FROM holding h
      JOIN latest l ON l.statement_id = h.statement_id AND l.account_id = h.account_id
      LEFT JOIN named n ON n.account_id = h.account_id
      LEFT JOIN shown i ON i.instrument_id = h.instrument_id
     WHERE h.currency_assumed AND h.instrument_id IS NOT NULL
    UNION ALL
    SELECT 'statement|' || coalesce(n.name, l.account_id)
           || '|' || l.source
           || '|' || l.expected_rows::text
           || '|' || CASE WHEN l.completed_at_ns IS NULL THEN 'open' ELSE 'complete' END
           || '|' || coalesce(l.buying_power::text || ' ' || l.buying_power_currency, '')
           || '|' || coalesce(l.margin_requirement::text || ' ' || l.margin_requirement_currency, '')
           || '|' || coalesce(l.maintenance_excess::text || ' ' || l.maintenance_excess_currency, '')
           || '|' || l.currency_assumed::text
      FROM latest l
      LEFT JOIN named n USING (account_id)
)
SELECT line FROM lines ORDER BY line COLLATE "C";
