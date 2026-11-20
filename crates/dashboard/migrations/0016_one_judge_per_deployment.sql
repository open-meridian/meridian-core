-- One judge per deployment (the review of v31a-core-1, findings 1 and 3;
-- the architecture review's single-writer judge, M19).
--
-- ha runs two dashboards. Each judged the latency monitor and each
-- component's health on its own, and filed what it judged; two copies
-- judging one shed either side of an hour's boundary filed it twice. Now
-- one copy judges at a time: the one holding the judges' lease. It renews
-- the lease while it runs; another takes it only once it has lapsed, under
-- the next term. The holder files its notices, and keeps each judge's
-- place beside them, in one transaction that first holds the lease's row
-- and finds its own term still there: a copy that lost the lease files
-- nothing more, and a copy that takes it up starts from the place the last
-- holder kept with its last notices, so an episode is filed neither twice
-- nor never as the lease passes.
--
-- Written to be run again harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS dashboard_judge_lease (
    lease          text   PRIMARY KEY,
    -- The copy holding it, and the term it holds it under: one more each
    -- time it passes to another copy, or is taken again after it lapsed.
    holder         text   NOT NULL,
    term           bigint NOT NULL,
    -- By the database's own clock (clock_timestamp()), which alone says
    -- whether the lease has lapsed: never a copy's, so two copies whose
    -- clocks disagree by more than the lease lasts do not both hold it.
    expires_at_ns  bigint NOT NULL
);

CREATE TABLE IF NOT EXISTS dashboard_judge_place (
    judge          text   PRIMARY KEY,
    -- Where the judge is, in its own words (JSON), as the holder of `term`
    -- kept it with the notices it filed at `filed_at_ns`.
    place          text   NOT NULL,
    term           bigint NOT NULL,
    filed_at_ns    bigint NOT NULL
);
