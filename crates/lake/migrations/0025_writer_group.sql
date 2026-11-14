-- A partition's writer group (step 3d; spec/a-deployment-scales-out-and-
-- fails-over-by-configuration sections 6.1 and 6.8; `writer_group integer`
-- ruled 2026-11-14): the group that owns the range, set in a move's switch
-- in the same transaction as the epoch the new owner commits under. NULL is
-- the rendezvous owner of the range's dataset. The control partition's
-- group fills each NULL with that owner at its start, so a later change of
-- the count moves nothing until a move moves it; a split's children take
-- their parent's. A writer that finds another group named here, under the
-- range's lock, hands the write to it and is never fenced.
ALTER TABLE lake_partition ADD COLUMN IF NOT EXISTS writer_group integer;
