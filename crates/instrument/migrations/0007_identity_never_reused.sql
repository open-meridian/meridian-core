-- Contract v19 (decisions/034; design/security-identity-is-never-reused,
-- ruled 2026-10-10; plans/the-lake-prices-the-book, Q34, Q35 and Q38).
--
-- An identifier that leaves a record has its window closed in
-- instrument_identifier, which already keeps `valid_to_ns`, and is never
-- deleted: nothing here changes how a window is kept. What is added:
--
-- * a binary event contract's attributes, a value like a money market
--   fund's: the payout's amount, its ISO 4217 code and its cash instrument,
--   and when trading closes, separated by `|`, or empty until stated;
-- * an identifier offer's end: when the platform says the identifier left
--   the record, an offer to close its window, in force once a person
--   accepts it (W3.5); 0 for an offer to add one;
-- * a conflict's reason, a MissReason: ambiguous where a resolve met
--   several records, flagged for a person and never escalated (W3.4,
--   W3.11); 0 where a join or a completion met another record's identifier.
--
-- Additions only, `IF NOT EXISTS` throughout, so it is run again harmlessly.

ALTER TABLE instrument
    ADD COLUMN IF NOT EXISTS binary_event_contract text NOT NULL DEFAULT '';

ALTER TABLE instrument_offer
    ADD COLUMN IF NOT EXISTS valid_until_ns bigint NOT NULL DEFAULT 0;

ALTER TABLE instrument_conflict
    ADD COLUMN IF NOT EXISTS reason integer NOT NULL DEFAULT 0;
