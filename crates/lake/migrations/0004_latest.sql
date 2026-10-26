-- The latest in force, chosen in SQL (found on dev 2026-10-26: a chain's
-- latest quotes read every row the dataset kept for the underlying --
-- 81,720 of them in two hours -- and chose a few hundred in memory).
--
-- What a read of the latest chooses per, besides the columns kept already
-- (subject, dataset, kind, venue and length): the row's asset and its
-- point -- a rate's tenor or maturity and convention, a derived value's
-- series, a chain row's series key -- as one key read from the row's own
-- body, and the quality state it was recorded with, so the last row not
-- invalid is found there too (ruling 619). Both are derived from the body
-- and change nothing recorded; a row without them (one recorded before
-- this, or by a release before it during a rolling upgrade) is a key of its
-- own, read as before, and `migrate` fills them in.

ALTER TABLE lake_row ADD COLUMN IF NOT EXISTS latest_key text;
ALTER TABLE lake_row ADD COLUMN IF NOT EXISTS quality smallint;
