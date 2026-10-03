-- Contract v11: a record's instrument type within its asset class, a money
-- market fund first, with the fund's attributes from SEC rule 2a-7 (the
-- product owner, 2026-10-02). Each is a value like the asset class: in force
-- once a person set or accepted it, with its source in instrument_value_source
-- and offers in instrument_offer, by field name, which those tables already
-- hold for any field.
--
-- Additions only, `IF NOT EXISTS`, applied after 0003 by the same `migrate`.

-- The type's enum name (INSTRUMENT_TYPE_MONEY_MARKET_FUND), or empty for none.
ALTER TABLE instrument ADD COLUMN IF NOT EXISTS instrument_type text NOT NULL DEFAULT '';

-- A money market fund's four attributes, as their enum names separated by a
-- space (category, investors, nav, liquidity fee), or empty until stated.
ALTER TABLE instrument ADD COLUMN IF NOT EXISTS money_market_fund text NOT NULL DEFAULT '';
