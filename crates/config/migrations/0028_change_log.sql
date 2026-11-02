-- The configuration store's change log (step 3a; spec/one-data-access-model
-- section 3, "The change log"; rulings 1046, 1056, 1057, 1060).
--
-- One partition, `config`, holds every committed change the conductor makes
-- to what a deployment admin authors and what it records of it, each
-- transaction's rows numbered from its head row with no holes, so the
-- conductor's standby (step 3b) can follow it. `control` is the store's own
-- partition beside it (ruling 1060). The rows are captured in their own
-- transaction by a trigger on each table, which `migrate` installs after
-- this file (meridian-changes, `captured`), and which is also the writer's
-- fence: a commit under an epoch other than the head row's is refused.
CREATE TABLE IF NOT EXISTS config_change_head (
    partition text   PRIMARY KEY,
    sequence  bigint NOT NULL CHECK (sequence >= 0)
);

CREATE TABLE IF NOT EXISTS config_change (
    partition       text    NOT NULL,
    sequence        bigint  NOT NULL CHECK (sequence > 0),
    ordinal         integer NOT NULL CHECK (ordinal >= 0),
    epoch           bigint  NOT NULL,
    -- NULL for a change made here: the partition's own origin.
    origin          text,
    committed_at_ns bigint  NOT NULL,
    kind            text    NOT NULL,
    body            jsonb   NOT NULL,
    PRIMARY KEY (partition, sequence, ordinal)
);
