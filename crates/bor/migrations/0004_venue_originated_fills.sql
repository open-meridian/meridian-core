-- Venue-originated fills (W11.11, W11.23, W11.24, contract v23;
-- sdk-contract/venue-originated-fills-and-the-lakes-outcomes-contract).
--
-- An execution's state is derived from the journal, never stored in it
-- (decisions/035 as amended); the projection keeps it beside the record so
-- the listing filters by it (ListOrdersRequest.execution_state, W11.17), as
-- it keeps an order's. Every execution v22 recorded is on a placement or its
-- order, so matched (2); `meridian-bor rebuild` writes each from the journal.
-- An accept or a DK names executions by their IDs alone, so the book finds
-- an execution's account by its ID, as it finds an order's.

ALTER TABLE book_execution ADD COLUMN IF NOT EXISTS state integer NOT NULL DEFAULT 2;
CREATE UNIQUE INDEX IF NOT EXISTS book_execution_by_id ON book_execution (execution_id);
CREATE INDEX IF NOT EXISTS book_execution_superseding ON book_execution (account_id, supersedes)
    WHERE supersedes <> '';
