-- An underlying's expirations read from the listing alone (contract v28,
-- amended 2026-10-28; rulings 856 and 858, ListExpirations approved 859).
-- Found on dev with sample-options 0.3.4: every options page listed its
-- underlying's expirations by reading every series in a 400-day window --
-- about 2,000 rows for TQQQ, 3 to 4 calls, 0.8 to 1.7 s -- to keep one date
-- of each.
--
-- What a series row is listed by, read from its own body: its expiry (its
-- expiration_date, else its last_trade_date, else its final_settlement_date)
-- and its listed type (a future where it names a product code, else an
-- option; an option on a future record is told at read, by its underlying).
-- Both are derived and change nothing recorded; a row without them (one
-- recorded before this, or by a release before it during a rolling upgrade)
-- is read whole, as before, and `migrate` fills them in.
--
-- lake_row_listing holds only series rows, by underlying, dataset and row
-- key, newest version first, with what an expirations read counts, so a
-- read takes each row key's version in force from the index without
-- reading a body.

ALTER TABLE lake_row ADD COLUMN IF NOT EXISTS expires date;
ALTER TABLE lake_row ADD COLUMN IF NOT EXISTS listed_type smallint;

CREATE INDEX IF NOT EXISTS lake_row_listing
    ON lake_row (subject, dataset, row_key, version DESC)
    INCLUDE (expires, listed_type, latest_key, recorded_at_ns, valid_from_ns, quality, sequence)
    WHERE data_type = 9;

-- lake_row_unlisted holds the series rows not yet given them, which the
-- backfill `migrate` runs every time reads, never a scan of every row.
CREATE INDEX IF NOT EXISTS lake_row_unlisted
    ON lake_row (dataset, sequence) WHERE data_type = 9 AND listed_type IS NULL;
