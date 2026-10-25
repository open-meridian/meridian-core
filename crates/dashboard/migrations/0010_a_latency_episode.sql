-- A notice names a ticket, a prepared change, a service account or, from
-- contract v28 (W4.16; rulings 427 and 606), a latency or backlog episode
-- of a plugin's session or a dataset's hop: opened (`latency_breached`,
-- `backlog_breached`) or closed (`latency_recovered`, `backlog_recovered`),
-- each its own record (decisions/031), to each person told. `about` is the
-- dashboard's own words for the series and what the minute held; never a
-- plugin's text.
ALTER TABLE dashboard_notice
    ADD COLUMN IF NOT EXISTS plugin_instance_id text NOT NULL DEFAULT '';
ALTER TABLE dashboard_notice
    ADD COLUMN IF NOT EXISTS about text NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS dashboard_notice_latency
    ON dashboard_notice (subject, notice_id) WHERE plugin_instance_id <> '';
