-- The instrument store's change log (step 3a; spec/one-data-access-model
-- section 3, "The change log": "the instrument store gains a store-wide
-- sequence"; rulings 1046, 1056, 1057, 1060).
--
-- One partition, `instrument`, holds every committed change -- record
-- versions, replacements, event groups, calendars, venues, templates,
-- companies -- each transaction's rows numbered from its head row with no
-- holes, so a follower can tell a gap where `InstrumentApplied` could not.
-- `control` is the store's own partition beside it (ruling 1060). The rows
-- are captured in their own transaction by a trigger on each table, which
-- `migrate` installs after this file (meridian-changes, `captured`), and
-- which is also the writer's fence: a commit under an epoch other than the
-- head row's is refused.
CREATE TABLE IF NOT EXISTS instrument_change_head (
    partition text   PRIMARY KEY,
    sequence  bigint NOT NULL CHECK (sequence >= 0)
);

CREATE TABLE IF NOT EXISTS instrument_change (
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
