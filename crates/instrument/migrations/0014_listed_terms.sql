-- Contract v28 (sdk-contract/listed-options-and-futures-contract; rulings
-- 559, 575 and 579): a listed series' full terms on its record, as the
-- platform's record states them -- a listed option's or a future option's
-- (ListedOption), or a future's (FutureContract) -- kept as the message's
-- bytes in hex, or empty for any other record. An adjustment is the next
-- version of the record, the INS- ID kept. A series past its last lifecycle
-- date is marked expired in lifecycle_state, which holds the enum's name
-- already, so nothing is added for it.
--
-- Additions only, `IF NOT EXISTS` throughout, so it is run again harmlessly.

ALTER TABLE instrument
    ADD COLUMN IF NOT EXISTS listed_terms text NOT NULL DEFAULT '';
