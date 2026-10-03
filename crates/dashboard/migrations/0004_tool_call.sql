-- The deployment's MCP surface's call record (W6.20, contract v12;
-- spec/a-deployment-serves-its-mcp, requirement 21 and Q8): every call made
-- through /mcp -- when, the person, the delegation and its client, the tool
-- and its owner, the level, the outcome and its reason, and how long it took.
--
-- Never an argument and never an answer: either may carry account data, and
-- a deployment admin, who reaches no account's data, reads this table. What
-- changed is each store's to record, with the person and the delegation.
--
-- Kept 90 days, the longest a delegation lives; the dashboard's sweep removes
-- older rows. Written to be run again harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS dashboard_tool_call (
    -- Minted by the dashboard, so no sequence needs a grant.
    call_id        text    PRIMARY KEY CHECK (call_id <> ''),
    called_at_ns   bigint  NOT NULL,
    subject        text    NOT NULL,
    delegation_id  text    NOT NULL,
    client_name    text    NOT NULL DEFAULT '',
    -- `dashboard` for core's own tools; a plugin's instance otherwise.
    owner          text    NOT NULL,
    -- The tool's name on the surface, `{owner}__{name}`.
    tool           text    NOT NULL,
    -- The level a plugin's tool opened at; empty for core's.
    level          text    NOT NULL DEFAULT '',
    -- made, unchanged or refused, and the refusal's reason.
    outcome        text    NOT NULL,
    reason         text    NOT NULL DEFAULT '',
    duration_ms    bigint  NOT NULL
);

CREATE INDEX IF NOT EXISTS dashboard_tool_call_delegation
    ON dashboard_tool_call (delegation_id, called_at_ns DESC);
CREATE INDEX IF NOT EXISTS dashboard_tool_call_subject
    ON dashboard_tool_call (subject, called_at_ns DESC);
CREATE INDEX IF NOT EXISTS dashboard_tool_call_owner
    ON dashboard_tool_call (owner, called_at_ns DESC);
CREATE INDEX IF NOT EXISTS dashboard_tool_call_age
    ON dashboard_tool_call (called_at_ns);
