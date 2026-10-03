-- The street store as lines, for a test to compare with a file it keeps.
--
-- Read-only, and one statement. Run with `psql -At` against the harness's
-- database, where every store shares one schema, so an account's name is
-- read beside its rows. Every account, ordered bytewise (COLLATE "C"), so the
-- output does not depend on the database's collation; and nothing that
-- changes from run to run -- an account's ID, a minted record's ID, a read
-- time, a statement's ID -- is printed.
--
--   position|<account>|<instrument>|<side>|<quantity>|<settle-date quantity>|<market value> <currency>|<in-cash>
--   assumed|<account>|<instrument>|<side>
--   statement|<account>|<source>|<expected rows>|<complete or open>|<buying power>|<margin requirement>|<maintenance excess>|<currency assumed>
--   pending|<account>|<instrument>|<side>|<value date>|<quantity>
--   closed|<account>|<instrument>|<side>|<field>|<kind>
--   amended|<account>|<instrument>|<side>|<contract version>|<field>|<raw record's key>
--
-- <account> is the account's name. <instrument> is its INS- ID, or for a
-- record the deployment minted (contract v10: its LCL- ID, drawn per run)
-- its identifiers in force, sorted: `local(<scheme>:<value>[@<source>], ...)`.
-- With no platform, as in the harness, every record is one the deployment
-- minted. A number is printed at the
-- scale it was stated with (decisions/023), and what was not reported is
-- empty, never zero. <in-cash> is `in-cash` for a position the venue also
-- counts in cash.
--
-- A position is the account's custodial position; one a merged record's
-- replacement removed is kept by the store as a tombstone, and not printed.
-- A statement's figures are those of its set with no segment, the account's
-- as a whole (contract v7 keeps them per margin segment). An `assumed` line is a row
-- of a latest statement whose currency the connector assumed. A statement is
-- the latest each source recorded for each account, by its rows' account: a
-- statement none of whose rows was recorded -- every one refused, its account
-- unlinked -- belongs to no account and is not printed.
--
-- From contract v11: a `pending` line is a position's quantity not yet
-- settled, by its value date; a `closed` line a value of a position the
-- custody plugin closed rather than read, by its path in the row, and the
-- provenance's kind (derived, supplied, second-source, reported); each from
-- the row that last stated the position, with what a backfill added to it.
-- An `amended` line is a backfill journaled beside a row of a latest
-- statement (W2.4), with the version and field that are its cause and the
-- key of the raw record it was re-converted from; the row as first recorded
-- is the one the other lines read where it carried the field.
--
-- The format is the plugin harness's published interface, versioned with
-- the image that carries this file; `make interop` and `make harness-check`
-- in meridian-core hold it.

WITH shown AS (
    -- Each record the deployment minted (an LCL- ID, drawn per run) as its
    -- identifiers in force.
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
    -- The latest statement of each source for each account its rows name.
    SELECT DISTINCT ON (held.account_id, s.source)
           held.account_id, s.statement_id, s.source, s.expected_rows, s.completed_at_ns,
           s.currency_assumed
      FROM statement s
      JOIN (SELECT DISTINCT account_id, statement_id FROM holding) held USING (statement_id)
     -- Read at the same moment, the one recorded last: an ID begins with
     -- the time it was minted, and is compared bytewise.
     ORDER BY held.account_id, s.source, s.read_at_ns DESC, s.statement_id COLLATE "C" DESC
),
edge_pending AS (
    -- A row's pending quantities as first recorded, or as a backfill added
    -- them where it carried none.
    SELECT hp.holding_id, hp.value_date, hp.quantity
      FROM holding_pending hp
     WHERE hp.amended_in = ''
        OR NOT EXISTS (SELECT 1 FROM holding_pending f
                        WHERE f.holding_id = hp.holding_id AND f.amended_in = '')
),
edge_provenance AS (
    SELECT hp.holding_id, hp.field, hp.kind
      FROM holding_provenance hp
     WHERE hp.amended_in = ''
        OR NOT EXISTS (SELECT 1 FROM holding_provenance f
                        WHERE f.holding_id = hp.holding_id AND f.amended_in = '')
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
     WHERE NOT p.removed
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
           || '|' || coalesce(f.buying_power::text || ' ' || f.buying_power_currency, '')
           || '|' || coalesce(f.margin_requirement::text || ' ' || f.margin_requirement_currency, '')
           || '|' || coalesce(f.maintenance_excess::text || ' ' || f.maintenance_excess_currency, '')
           || '|' || l.currency_assumed::text
      FROM latest l
      LEFT JOIN named n USING (account_id)
      LEFT JOIN statement_figures f ON f.statement_id = l.statement_id AND f.segment = ''
    UNION ALL
    SELECT 'pending|' || coalesce(n.name, p.account_id)
           || '|' || coalesce(i.instrument, p.instrument_id)
           || '|' || p.side
           || '|' || e.value_date
           || '|' || e.quantity::text
      FROM custodial_position p
      JOIN edge_pending e ON e.holding_id = p.last_holding_id
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i USING (instrument_id)
     WHERE NOT p.removed
    UNION ALL
    SELECT 'closed|' || coalesce(n.name, p.account_id)
           || '|' || coalesce(i.instrument, p.instrument_id)
           || '|' || p.side
           || '|' || e.field
           || '|' || CASE e.kind WHEN 1 THEN 'reported' WHEN 2 THEN 'second-source'
                                 WHEN 3 THEN 'supplied' WHEN 4 THEN 'derived' ELSE '' END
      FROM custodial_position p
      JOIN edge_provenance e ON e.holding_id = p.last_holding_id
      LEFT JOIN named n USING (account_id)
      LEFT JOIN shown i USING (instrument_id)
     WHERE NOT p.removed
    UNION ALL
    SELECT 'amended|' || coalesce(n.name, h.account_id)
           || '|' || coalesce(i.instrument, h.instrument_id, 'unresolved')
           || '|' || h.side
           || '|' || a.contract_version
           || '|' || a.field
           || '|' || a.raw_key
      FROM holding_amendment a
      JOIN holding h USING (holding_id)
      JOIN latest l ON l.statement_id = h.statement_id AND l.account_id = h.account_id
      LEFT JOIN named n ON n.account_id = h.account_id
      LEFT JOIN shown i ON i.instrument_id = h.instrument_id
)
SELECT line FROM lines ORDER BY line COLLATE "C";
