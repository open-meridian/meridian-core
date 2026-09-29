-- An account's custodian, type, owner and note (W6.3): free text, optional and
-- searchable, as the product owner ruled on 2026-09-29. Nullable with no
-- default, so every account already here has none of the four, and an empty
-- one is written as NULL rather than as ''. The bounds, 200 characters and
-- 2,000 for the note, are the conductor's rules, which name the field when
-- they refuse; the checks here are the backstop.
ALTER TABLE config_account
    ADD COLUMN IF NOT EXISTS custodian    text CHECK (char_length(custodian) <= 200),
    ADD COLUMN IF NOT EXISTS account_type text CHECK (char_length(account_type) <= 200),
    ADD COLUMN IF NOT EXISTS owner        text CHECK (char_length(owner) <= 200),
    ADD COLUMN IF NOT EXISTS note         text CHECK (char_length(note) <= 2000);
