-- A partition's writer group (step 3d; spec/a-deployment-scales-out-and-
-- fails-over-by-configuration sections 6.3 and 6.8; `writer_group integer`
-- ruled 2026-11-14): the group that owns it, set in a move's switch in the
-- same transaction as the epoch the new owner commits under. NULL is the
-- rendezvous owner of the store's count of groups. The control partition's
-- group fills each NULL with that owner at its start, so a later change of
-- the count moves nothing until a move moves it. A writer that finds
-- another group named here, under the partition's lock, hands the write to
-- it and is never fenced.
ALTER TABLE book_partition_head ADD COLUMN IF NOT EXISTS writer_group integer;
