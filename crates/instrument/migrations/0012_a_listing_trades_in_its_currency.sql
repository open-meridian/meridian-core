-- Contract v25 (W3.1; approved 2026-10-14 for v25): a listing identifier --
-- an exchange-level or composite FIGI on a security's record -- carries the
-- ISO 4217 currency that listing trades in, as the platform's master holds
-- it per listing (Identifier.trading_currency). Empty on every other
-- identifier, and where the master holds none: every identifier kept before
-- v25, since no answer carried one. The record's own currency stays the
-- security's.
--
-- Additions only, `IF NOT EXISTS`, so it is run again harmlessly.

ALTER TABLE instrument_identifier
    ADD COLUMN IF NOT EXISTS trading_currency text NOT NULL DEFAULT '';
