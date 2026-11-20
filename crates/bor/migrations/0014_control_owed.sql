-- The records the book's control journal is owed by a member that does not
-- write it (step 3d; spec/a-deployment-scales-out-and-fails-over-by-
-- configuration section 6.8; decisions/031; `book_control_owed` approved
-- 2026-11-14): a member of a writer group past the first -- an account's
-- move aborted there, and its digest's finding -- keeps each here, in its
-- own database, by the key it is journalled under, in the transaction
-- that decides it. The control partition's group's writer journals each
-- once on `~control`, the book's own act, idempotent by that key, as it
-- journals the takeovers and findings kept beside it: each record keeps
-- its own time, and its entry is committed at the journalling's.
--
-- Empty with one writer group, the default: that writer journals each
-- record itself.
CREATE TABLE IF NOT EXISTS book_control_owed (
    key        text   PRIMARY KEY,
    kind       text   NOT NULL,
    body       bytea  NOT NULL,
    reason     text   NOT NULL,
    kept_at_ns bigint NOT NULL
);
