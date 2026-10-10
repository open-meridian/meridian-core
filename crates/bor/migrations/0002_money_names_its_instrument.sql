-- A Money names its instrument (contract v18; decisions/023 as amended,
-- ruling 1 of 2026-10-09). The book keeps each entry whole, as the
-- operations plugin stated it -- a fiat amount by its ISO 4217 code -- and
-- here each code's cash instrument, resolved through the instrument store,
-- so every amount it answers and announces names its instrument.
--
-- Each resolution is its own record (decisions/031): a code held before
-- contract v18 is resolved once, after the upgrade, and says it was filled in
-- (`backfilled`) at the moment it was, never back-dated. No entry is
-- rewritten: the journal is the record.
CREATE TABLE IF NOT EXISTS book_cash_instrument (
    currency_code  text    PRIMARY KEY CHECK (currency_code ~ '^[A-Z]{3}$'),
    instrument_id  text    NOT NULL CHECK (instrument_id <> ''),
    resolved_at_ns bigint  NOT NULL,
    backfilled     boolean NOT NULL
);
