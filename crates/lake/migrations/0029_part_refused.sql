-- A writer group's part of a write refused by the lake's rule 5 (step 3d;
-- spec/a-deployment-scales-out-and-fails-over-by-configuration section
-- 6.7; `refused integer` and `refused_detail text` ruled 2026-11-15). A
-- group that does not answer a write may refuse its own rows of it: a row
-- key restated under another valid time or first subject. It commits none
-- of them, and until now the group answering knew nothing of it and
-- answered a success counting the rest.
--
-- The refusing group keeps, with its part and in the same transaction as
-- its position in the stream, how many rows it refused (`refused`) and the
-- refusal as the write's plugin hears it, naming the row and the field
-- (`refused_detail`). The group answering reads it once that group has
-- passed the write, and answers the write refused by name, as it answers
-- its own part's refusal. 0 and empty where the part refused nothing.
ALTER TABLE lake_write_ahead_part ADD COLUMN IF NOT EXISTS refused integer NOT NULL DEFAULT 0;
ALTER TABLE lake_write_ahead_part ADD COLUMN IF NOT EXISTS refused_detail text NOT NULL DEFAULT '';
