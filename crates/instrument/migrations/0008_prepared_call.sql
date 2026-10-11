-- Contract v20: a value set, accepted or merged by a person from a prepared
-- change is recorded with the prepared change's `PC-` ID beside the
-- delegation and client (W4.9, W6.27; plans/an-agents-prepared-change-waits-
-- for-the-person, Q16), stamped from the command's envelope, never typed.
-- Empty for everything else, and for everything recorded before v20: nothing
-- was prepared before it, so nothing is backfilled.
--
-- Additions only, `IF NOT EXISTS` throughout, so it is run again harmlessly.

ALTER TABLE instrument_value_source
    ADD COLUMN IF NOT EXISTS prepared_call_id text NOT NULL DEFAULT '';

ALTER TABLE instrument_version
    ADD COLUMN IF NOT EXISTS prepared_call_id text NOT NULL DEFAULT '';
