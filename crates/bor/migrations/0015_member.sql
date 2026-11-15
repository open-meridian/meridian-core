-- The book's members, as each says it is serving (step 3d; spec/a-
-- deployment-scales-out-and-fails-over-by-configuration sections 6.4 and
-- 6.8; `book_member` ruled 2026-11-15, item 9). Every member -- a writer, a
-- standby, a reader -- keeps its own row in database 0, seen again every
-- few seconds, with the revision it runs: the release's version and each
-- setting that changes where it reads or writes, as `0.1.540/db2/wg2/p4`.
--
-- The control group's sweep moves a partition home to another database
-- only once every member seen within the last lease interval runs the
-- sweep's own revision: a member started before a raised `bor.databases`
-- reads one database, and answered an account whose partition had left it
-- as empty (PR CI 43656183991). A row not seen within the interval is
-- ignored; one not seen for a day is let go.
CREATE TABLE IF NOT EXISTS book_member (
    member        text   PRIMARY KEY,
    revision      text   NOT NULL,
    started_at_ns bigint NOT NULL,
    seen_at_ns    bigint NOT NULL
);
