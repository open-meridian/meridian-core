-- Booking, the alert and the escape hatch (W11.26 to W11.37, contract v24;
-- sdk-contract/an-execution-is-booked-to-its-accounts-contract).
--
-- A booking and an alert are records of their account's partition, projected
-- as every order-path record is, whole, with what a read filters and pages by
-- beside them; their states are derived from the journal and kept here only
-- for the read (decisions/035 as amended at v24). A command naming a booking
-- or an alert by its ID alone finds its account by it. A block order's
-- members are indexed so a proposal finds a member already in a live block
-- (W11.1). `meridian-bor rebuild` writes all three from the journal.

CREATE TABLE IF NOT EXISTS book_booking (
    account_id text    NOT NULL,
    booking_id text    NOT NULL,
    partition  text    NOT NULL,
    sequence   bigint  NOT NULL,
    state      integer NOT NULL,
    order_id   text    NOT NULL,
    record     bytea   NOT NULL,
    PRIMARY KEY (account_id, booking_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS book_booking_by_id ON book_booking (booking_id);

CREATE TABLE IF NOT EXISTS book_alert (
    account_id text    NOT NULL,
    alert_id   text    NOT NULL,
    partition  text    NOT NULL,
    sequence   bigint  NOT NULL,
    state      integer NOT NULL,
    kind       integer NOT NULL,
    record     bytea   NOT NULL,
    PRIMARY KEY (account_id, alert_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS book_alert_by_id ON book_alert (alert_id);

CREATE TABLE IF NOT EXISTS book_block_member (
    block_order_id  text NOT NULL,
    member_order_id text NOT NULL,
    account_id      text NOT NULL,
    PRIMARY KEY (block_order_id, member_order_id)
);
CREATE INDEX IF NOT EXISTS book_block_member_by_member ON book_block_member (member_order_id);
