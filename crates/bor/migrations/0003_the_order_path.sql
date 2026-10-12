-- The order path's slice A (W11, contract v22;
-- sdk-contract/an-order-reaches-a-venue-contract): the projections of
-- orders, placements, executions, unplaced facts, locates and trading
-- instructions, each rebuilt from the journal like every other projection;
-- and, from v22, a position, a lot and a pending settlement keyed by their
-- sleeve within the account (W9's invariants; empty for the unsleeved part,
-- as every row before is).
--
-- A record a command may name by its ID alone is found here by its ID, so
-- the book holds the command to the record's account (W4.9). The journal is
-- unchanged: an entry keeps its order-path acts in its body.

ALTER TABLE book_position ADD COLUMN IF NOT EXISTS sleeve text NOT NULL DEFAULT '';
ALTER TABLE book_position DROP CONSTRAINT IF EXISTS book_position_pkey;
ALTER TABLE book_position ADD CONSTRAINT book_position_pkey
    PRIMARY KEY (account_id, instrument_id, side, sleeve);

ALTER TABLE book_lot ADD COLUMN IF NOT EXISTS sleeve text NOT NULL DEFAULT '';
ALTER TABLE book_lot DROP CONSTRAINT IF EXISTS book_lot_pkey;
ALTER TABLE book_lot ADD CONSTRAINT book_lot_pkey
    PRIMARY KEY (account_id, instrument_id, side, sleeve, lot_id);

ALTER TABLE book_pending ADD COLUMN IF NOT EXISTS sleeve text NOT NULL DEFAULT '';
ALTER TABLE book_pending DROP CONSTRAINT IF EXISTS book_pending_pkey;
ALTER TABLE book_pending ADD CONSTRAINT book_pending_pkey
    PRIMARY KEY (account_id, instrument_id, side, sleeve, value_date);

-- `account_id` is the book a record is held in: an account's, or for a
-- venue's fact naming no account `~unplaced`, which no plugin's scope holds.

-- Each order whole, its placements, executions and charges in it, its
-- status as the fold derived it at its last change. `expires_at_ns` is when
-- its own time in force ends by the deployment's clock, NULL where it does
-- not or it is already ending (W11.13).
CREATE TABLE IF NOT EXISTS book_order (
    account_id      text    NOT NULL,
    order_id        text    NOT NULL,
    partition       text    NOT NULL,
    sequence        bigint  NOT NULL,
    state           integer NOT NULL,
    instrument_id   text    NOT NULL,
    side            integer NOT NULL,
    sleeve          text    NOT NULL,
    ordered         numeric,
    parent_order_id text    NOT NULL,
    expires_at_ns   bigint,
    record          bytea   NOT NULL,
    PRIMARY KEY (account_id, order_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS book_order_by_id ON book_order (order_id);
CREATE INDEX IF NOT EXISTS book_order_expiring ON book_order (expires_at_ns)
    WHERE expires_at_ns IS NOT NULL;

CREATE TABLE IF NOT EXISTS book_placement (
    account_id               text    NOT NULL,
    placement_id             text    NOT NULL,
    order_id                 text    NOT NULL,
    connectivity_instance_id text    NOT NULL,
    partition                text    NOT NULL,
    sequence                 bigint  NOT NULL,
    state                    integer NOT NULL,
    quantity                 numeric,
    record                   bytea   NOT NULL,
    PRIMARY KEY (account_id, placement_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS book_placement_by_id ON book_placement (placement_id);

CREATE TABLE IF NOT EXISTS book_execution (
    account_id   text   NOT NULL,
    execution_id text   NOT NULL,
    order_id     text   NOT NULL,
    placement_id text   NOT NULL,
    partition    text   NOT NULL,
    sequence     bigint NOT NULL,
    quantity     numeric,
    price        numeric,
    trade_date   text   NOT NULL,
    supersedes   text   NOT NULL,
    record       bytea  NOT NULL,
    PRIMARY KEY (account_id, execution_id)
);

CREATE TABLE IF NOT EXISTS book_unplaced (
    account_id  text    NOT NULL,
    unplaced_id text    NOT NULL,
    partition   text    NOT NULL,
    sequence    bigint  NOT NULL,
    fact_kind   text    NOT NULL,
    reason      integer NOT NULL,
    -- The instance whose storage holds the raw record: the connectivity
    -- plugin whose admin view counts it.
    instance_id text    NOT NULL,
    placed      boolean NOT NULL,
    record      bytea   NOT NULL,
    PRIMARY KEY (account_id, unplaced_id)
);

CREATE TABLE IF NOT EXISTS book_locate (
    account_id       text   NOT NULL,
    locate_record_id text   NOT NULL,
    partition        text   NOT NULL,
    sequence         bigint NOT NULL,
    locate_id        text   NOT NULL,
    record           bytea  NOT NULL,
    PRIMARY KEY (account_id, locate_record_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS book_locate_by_id ON book_locate (locate_record_id);

-- Each trading instruction standing under its key, its target and its
-- setter's level, in the partition it was set in: an account's for a
-- deployment admin's on one account, `control` for every other (W11.15).
CREATE TABLE IF NOT EXISTS book_instruction (
    instruction_key text    PRIMARY KEY,
    account_id      text    NOT NULL,
    partition       text    NOT NULL,
    sequence        bigint  NOT NULL,
    value           integer NOT NULL,
    setter_level    integer NOT NULL,
    record          bytea   NOT NULL
);

-- Where each change request is, for a command naming one by its ID.
CREATE TABLE IF NOT EXISTS book_change_request (
    change_request_id text PRIMARY KEY,
    account_id        text NOT NULL,
    order_id          text NOT NULL,
    placement_id      text NOT NULL
);

-- Each entry that changed an order: its history, read from the journal.
CREATE TABLE IF NOT EXISTS book_order_entry (
    account_id     text   NOT NULL,
    order_id       text   NOT NULL,
    entry_id       text   NOT NULL,
    first_sequence bigint NOT NULL,
    PRIMARY KEY (order_id, entry_id)
);
