-- Writer groups (step 3d; the resilience spec's sections 6.1 and 6.7,
-- `lake_writer_group` approved 2026-11-12). Each writer group past the
-- first keeps its epoch here: raised at its takeover with its datasets', and
-- what a dataset its writer begins is checked against, as group 0's are
-- against the control partition's head row. A deposed writer of the group
-- beginning a dataset after the takeover is refused by it.
--
-- Empty with one writer group, the default: nothing reads or writes it.
CREATE TABLE IF NOT EXISTS lake_writer_group (
    writer_group integer PRIMARY KEY,
    epoch        bigint  NOT NULL,
    origin       text    NOT NULL DEFAULT ''
);
