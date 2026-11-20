-- The lake's members, as each says it is serving (step 3d; spec/a-
-- deployment-scales-out-and-fails-over-by-configuration sections 6.4 and
-- 6.8; `lake_member` ruled 2026-11-15, item 9). Every member -- a writer, a
-- standby, a reader -- keeps its own row in database 0, seen again every
-- few seconds, with the revision it runs: the release's version and each
-- setting that changes where it reads or writes, as `0.1.540/db2/wg2`.
--
-- The control group's sweep moves a range home to another database only
-- once every member seen within the last lease interval runs the sweep's
-- own revision: a member started before a raised `lake.databases` reads
-- one database, and would answer for a range that left it as though it
-- held nothing. A row not seen within the interval is ignored; one not
-- seen for a day is let go.

CREATE TABLE IF NOT EXISTS lake_member (
    member        text   PRIMARY KEY,
    revision      text   NOT NULL,
    started_at_ns bigint NOT NULL,
    seen_at_ns    bigint NOT NULL
);
