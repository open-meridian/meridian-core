-- A value core computed keeps its basis (contract v22; W3.20,
-- spec/settlement-calendars-are-core-reference-data, requirement 8 and Q10):
-- a provenance of kind PROVENANCE_KIND_COMPUTED carries the date rule and
-- each calendar, with their versions, the date read worked it out on, kept
-- here as the plugin sent it, encoded as the wire's DateBasis. Empty for
-- every other kind, and for every provenance recorded before v22, none of
-- which was computed: nothing is backfilled, since nothing is missing.
--
-- Written to be run again harmlessly.
ALTER TABLE statement_provenance ADD COLUMN IF NOT EXISTS date_basis bytea NOT NULL DEFAULT '\x';
ALTER TABLE holding_provenance ADD COLUMN IF NOT EXISTS date_basis bytea NOT NULL DEFAULT '\x';
