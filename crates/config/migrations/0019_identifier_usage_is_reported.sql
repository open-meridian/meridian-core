-- The deployment admin's setting for the usage statistics, and the reports
-- the conductor sent (contract v24; W3.28; the intent's ruling 15).
--
-- Each change to the setting, its own record (decisions/031): on or off,
-- who, through which delegation and client, when and why. None is "on",
-- the default. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_reports_usage_change (
    change_id          bigserial PRIMARY KEY,
    reports_usage      boolean   NOT NULL,
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    prepared_call_id   text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);

-- Each report sent, its own record (SentUsageReport, encoded): the counts
-- sent, when, and whether the platform received it. Counts only, never a
-- value, a holding, a quantity or an account. Never updated, never deleted.
CREATE TABLE IF NOT EXISTS config_usage_report (
    report_id  bigserial PRIMARY KEY,
    sent_at_ns bigint    NOT NULL,
    report     bytea     NOT NULL
);
