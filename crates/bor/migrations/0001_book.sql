-- The book of record (W9; contract v8, revision B of
-- plans/the-book-holds-positions): its journal, partitioned by account, and
-- the projections rebuilt from it.
--
-- Every table is `book_`, because a deployment may put every store in one
-- schema, as the plugin harness does; the history is book_schema_migration.
--
-- The journal is append-only, at the store and not only in the code: a
-- trigger refuses an update or a delete of an entry, and a truncate, whoever
-- asks (W9's first invariant). A correction is a later entry.
--
-- A partition's head is a row, moved in the transaction of each entry, never
-- a database sequence, which skips a number on a rollback that a reader
-- would take for a lost delivery (the plan, design decision 5). Every account
-- is in P0 to start, and `control` holds no account (decisions/024, as noted
-- 2026-10-01).

CREATE TABLE IF NOT EXISTS book_partition_head (
    partition text   PRIMARY KEY,
    sequence  bigint NOT NULL CHECK (sequence >= 0)
);
INSERT INTO book_partition_head (partition, sequence) VALUES ('P0', 0), ('control', 0)
    ON CONFLICT (partition) DO NOTHING;

-- Which partition an account's entries are in: data, recorded with its first.
CREATE TABLE IF NOT EXISTS book_account (
    account_id text PRIMARY KEY,
    partition  text NOT NULL REFERENCES book_partition_head (partition)
);

-- One row per entry. Its records are numbered first_sequence to
-- last_sequence in its partition, one each, no holes. meta and cause are the
-- encoded EntryMeta and ChangeCause; body is what the entry did, as the
-- projection replays it; reply is the command's answer, kept so a duplicate
-- is answered with it; request the command, so a key reused for another is
-- told apart.
CREATE TABLE IF NOT EXISTS book_entry (
    entry_id        text   PRIMARY KEY,
    account_id      text   NOT NULL,
    partition       text   NOT NULL REFERENCES book_partition_head (partition),
    first_sequence  bigint NOT NULL CHECK (first_sequence > 0),
    last_sequence   bigint NOT NULL,
    kind            text   NOT NULL,
    effective_date  text   NOT NULL,
    -- Who answered for it, as text a person reads: a person's subject,
    -- `instance <id>` for a finding the plugin sent as itself, `book` for the
    -- book's own act.
    actor           text   NOT NULL DEFAULT '',
    message_id      text   NOT NULL DEFAULT '',
    idempotency_key text   NOT NULL DEFAULT '',
    committed_at_ns bigint NOT NULL,
    meta            bytea  NOT NULL,
    cause           bytea  NOT NULL,
    body            bytea  NOT NULL,
    reply           bytea  NOT NULL,
    -- The command as it was sent, encoded: a second command under the same
    -- idempotency key is the same command only if this is the same (Q12).
    request         bytea  NOT NULL DEFAULT ''::bytea,
    CONSTRAINT book_entry_numbered CHECK (last_sequence >= first_sequence),
    CONSTRAINT book_entry_once_in_its_partition UNIQUE (partition, first_sequence)
);
-- Applied once, on the envelope's message identifier (decisions/024), and on
-- the idempotency key a plugin sent, per account (Q12).
CREATE UNIQUE INDEX IF NOT EXISTS book_entry_message
    ON book_entry (message_id) WHERE message_id <> '';
CREATE UNIQUE INDEX IF NOT EXISTS book_entry_idempotency_key
    ON book_entry (account_id, idempotency_key) WHERE idempotency_key <> '';
CREATE INDEX IF NOT EXISTS book_entry_by_account ON book_entry (account_id, first_sequence);

CREATE OR REPLACE FUNCTION book_entry_is_never_changed() RETURNS trigger
    LANGUAGE plpgsql AS $f$
BEGIN
    RAISE EXCEPTION 'the book''s journal is append-only: an entry is never updated or deleted; a correction is a later entry (W9)';
END
$f$;

DROP TRIGGER IF EXISTS book_entry_append_only ON book_entry;
CREATE TRIGGER book_entry_append_only
    BEFORE UPDATE OR DELETE ON book_entry
    FOR EACH ROW EXECUTE FUNCTION book_entry_is_never_changed();
DROP TRIGGER IF EXISTS book_entry_never_truncated ON book_entry;
CREATE TRIGGER book_entry_never_truncated
    BEFORE TRUNCATE ON book_entry
    FOR EACH STATEMENT EXECUTE FUNCTION book_entry_is_never_changed();

-- The projections: each record as it stands, encoded, with the columns a
-- read selects and orders by, and the ones a person reading the book at a
-- prompt needs (the harness's book.sql). Rebuilt from the journal by
-- `meridian-bor rebuild`; never what a command is decided against. A number
-- is a numeric at the scale it was stated with (decisions/023), and what
-- is unknown is NULL, never zero.
CREATE TABLE IF NOT EXISTS book_position (
    account_id          text    NOT NULL,
    instrument_id       text    NOT NULL,
    side                integer NOT NULL,
    partition           text    NOT NULL,
    sequence            bigint  NOT NULL,
    removed             boolean NOT NULL,
    trade_date_quantity numeric NOT NULL,
    settled_quantity    numeric,
    not_stated_quantity numeric NOT NULL,
    effective_date      text    NOT NULL,
    record              bytea   NOT NULL,
    PRIMARY KEY (account_id, instrument_id, side)
);

-- A position's open lots and pending settlements, as its record holds them.
CREATE TABLE IF NOT EXISTS book_lot (
    account_id        text    NOT NULL,
    instrument_id     text    NOT NULL,
    side              integer NOT NULL,
    lot_id            text    NOT NULL,
    ordinal           integer NOT NULL,
    open_quantity     numeric NOT NULL,
    original_quantity numeric NOT NULL,
    cost              numeric,
    cost_currency     text,
    acquired_date     text    NOT NULL,
    source            integer NOT NULL,
    PRIMARY KEY (account_id, instrument_id, side, lot_id)
);

CREATE TABLE IF NOT EXISTS book_pending (
    account_id    text    NOT NULL,
    instrument_id text    NOT NULL,
    side          integer NOT NULL,
    value_date    text    NOT NULL,
    quantity      numeric NOT NULL,
    failing       boolean NOT NULL,
    PRIMARY KEY (account_id, instrument_id, side, value_date)
);
CREATE INDEX IF NOT EXISTS book_position_by_instrument ON book_position (instrument_id);

CREATE TABLE IF NOT EXISTS book_break (
    account_id    text    NOT NULL,
    break_id      text    NOT NULL,
    state         integer NOT NULL,
    instrument_id text    NOT NULL DEFAULT '',
    partition     text    NOT NULL,
    sequence      bigint  NOT NULL,
    category      integer NOT NULL,
    subject       text    NOT NULL,
    first_seen    text    NOT NULL,
    last_seen     text    NOT NULL,
    recorded_by   text    NOT NULL,
    -- The confirmed cause's category, 0 while none is confirmed; and how it
    -- was resolved or closed: entries, explanation or cleared, empty while
    -- open.
    cause         integer NOT NULL,
    resolution    text    NOT NULL,
    record        bytea   NOT NULL,
    PRIMARY KEY (account_id, break_id)
);

CREATE TABLE IF NOT EXISTS book_figures (
    account_id    text   NOT NULL,
    agreement_key text   NOT NULL,
    business_date text   NOT NULL,
    partition     text   NOT NULL,
    sequence      bigint NOT NULL,
    statement_id  text   NOT NULL,
    record        bytea  NOT NULL,
    PRIMARY KEY (account_id, agreement_key, business_date)
);

CREATE TABLE IF NOT EXISTS book_attributes (
    account_id         text    PRIMARY KEY,
    partition          text    NOT NULL,
    sequence           bigint  NOT NULL,
    base_currency_code text    NOT NULL,
    lot_relief_default integer NOT NULL,
    -- The standing opening balance's date, empty while none stands.
    opening_as_of      text    NOT NULL,
    record             bytea   NOT NULL
);
