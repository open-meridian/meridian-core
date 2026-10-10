-- A Money names its instrument (contract v18; decisions/023 as amended,
-- ruling 1 of 2026-10-09). The street keeps each amount as the custodian
-- stated it -- a fiat currency by its ISO 4217 code, which the amount's
-- currency column holds as before -- and here each code's cash instrument,
-- resolved through the instrument store, so every amount it answers names
-- its instrument. A token's amount, which a plugin names by its instrument
-- alone, keeps the instrument's ID in its currency column.
--
-- Each resolution is its own record (decisions/031): a code recorded before
-- contract v18 is resolved once, after the upgrade, and says it was filled
-- in (`backfilled`) at the moment it was, never back-dated.
--
-- Written to be run again harmlessly.
CREATE TABLE IF NOT EXISTS street_cash_instrument (
    currency_code  text    PRIMARY KEY CHECK (currency_code ~ '^[A-Z]{3}$'),
    instrument_id  text    NOT NULL CHECK (instrument_id <> ''),
    resolved_at_ns bigint  NOT NULL,
    backfilled     boolean NOT NULL
);
