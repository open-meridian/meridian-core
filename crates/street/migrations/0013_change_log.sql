-- The street store's change log (step 3a; spec/one-data-access-model
-- section 3, "The change log"; rulings 1046, 1056, 1057, 1060).
--
-- One partition, `street`, as the store has numbered its changes since
-- 0005: each change takes the head's next sequence in its own transaction,
-- with no holes. What the log adds is every row that transaction writes,
-- captured beside its sequence by a trigger on each table, which `migrate`
-- installs after this file (meridian-changes, `captured`) and which is also
-- the writer's fence: a commit under an epoch other than the head row's is
-- refused. `control` is the store's own partition beside it (ruling 1060).
CREATE TABLE IF NOT EXISTS street_change (
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
