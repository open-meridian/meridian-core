-- The book's takeovers (step 3b; spec/a-deployment-scales-out-and-fails-
-- over-by-configuration section 3; decisions/031): one row per epoch raised,
-- written in the transaction that raises it, naming the member that took
-- the book over, the partitions it took (every account partition and control), why (its
-- writer's lease lapsed, was released, or nobody held it) and when. The
-- `WriterTookOver` record, kept for good beside the store's control
-- partition; never rewritten. Its wire form is 3c's (the contract revision).
CREATE TABLE IF NOT EXISTS book_writer_took_over (
    epoch      bigint PRIMARY KEY CHECK (epoch > 0),
    member     text   NOT NULL,
    partitions text[] NOT NULL,
    why        text   NOT NULL,
    origin     text   NOT NULL,
    at_ns      bigint NOT NULL
);
