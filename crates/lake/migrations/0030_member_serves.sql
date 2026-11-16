-- What each of the lake's members serves as (step 3d, stage T; spec/a-
-- deployment-scales-out-and-fails-over-by-configuration sections 6.6 and
-- 15; `writer_group` and `mode` approved 2026-11-15). A member says, with
-- its row, the writer group it is of and the mode it serves in -- `writer`,
-- `standby` or `reader` -- empty until it serves.
--
-- An upgrade rolls every writer group's members at once, and each group's
-- writer serves the lake's public topics, handing a write on to its owner.
-- With every group restarting in the same seconds nothing served them (PR
-- CI 43636763392: about 2 s). So a writer of a group past the first, told
-- to stop, serves on until it reads here another group's writer serving
-- that started after it was told (or runs another revision), 20 s at most.
--
-- Nullable and defaulted: a member of the release before, still running as
-- the upgrade rolls, keeps its row without them, and is never read as
-- another group's writer.
ALTER TABLE lake_member ADD COLUMN IF NOT EXISTS writer_group integer;
ALTER TABLE lake_member ADD COLUMN IF NOT EXISTS mode text NOT NULL DEFAULT '';
