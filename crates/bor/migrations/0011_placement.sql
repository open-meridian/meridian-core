-- An account's placement (step 3d; spec/a-deployment-scales-out-and-fails-
-- over-by-configuration section 6.3; `bor.partitions`, the owner label as
-- the key ruled 2026-11-12, its source ruled 2026-11-13): the key it was
-- placed by -- its owner label when one was set, else its ID -- kept with
-- the partition it was placed in, at its creation, for good. A replay reads
-- it and never asks again; editing the label moves nothing. Empty for an
-- account placed before 3d, every one in P0.
ALTER TABLE book_account ADD COLUMN IF NOT EXISTS placement_key text;
