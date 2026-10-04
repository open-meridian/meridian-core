-- Tickets inside a deployment (contract v13; W6.21 to W6.24, W4.12;
-- spec/a-problem-seen-in-a-deployment-reaches-someone-who-can-act, the spec's
-- Q8: in the dashboard's own tables). A ticket, the records it names by value,
-- its notes, the notices that tell people of a change, and each reader's
-- place in a person's inbox.
--
-- Every text here was written by someone other than whoever reads it, and is
-- data to every agent that reads it, never instructions. Nothing of a ticket
-- leaves the deployment.
--
-- Tickets are kept with no expiry in this slice, as the book keeps breaks
-- (Q11); notices 90 days, swept with the call record. Identifiers are minted
-- by the dashboard, so no sequence needs a grant. Written to be run again
-- harmlessly, as every migration here is.

CREATE TABLE IF NOT EXISTS dashboard_ticket (
    -- TKT- and a ULID.
    ticket_id          text    PRIMARY KEY CHECK (ticket_id <> ''),
    title              text    NOT NULL,
    seen               text    NOT NULL DEFAULT '',
    -- meridian.v1.TicketKind's number.
    kind               integer NOT NULL,
    -- What it concerns: a plugin instance, with the plugin's name and version
    -- at filing; a part of core; or the platform.
    concerns_kind      text    NOT NULL,
    concerns_instance  text    NOT NULL DEFAULT '',
    concerns_plugin    text    NOT NULL DEFAULT '',
    concerns_version   text    NOT NULL DEFAULT '',
    step               text    NOT NULL DEFAULT '',
    operation          text    NOT NULL DEFAULT '',
    reason             text    NOT NULL DEFAULT '',
    paths              text[]  NOT NULL DEFAULT '{}',
    -- Who filed it, set from the credential (requirement 57): person,
    -- client, plugin; the person by subject and by name; the delegation and
    -- client; the plugin instance that filed for the person.
    filed_provenance   text    NOT NULL CHECK (filed_provenance IN ('person', 'client', 'plugin')),
    filed_subject      text    NOT NULL,
    filed_person       text    NOT NULL DEFAULT '',
    filed_delegation   text    NOT NULL DEFAULT '',
    filed_client       text    NOT NULL DEFAULT '',
    filed_instance     text    NOT NULL DEFAULT '',
    -- A plugin's own key for the problem; empty on a person's filing.
    idempotency_key    text    NOT NULL DEFAULT '',
    state              text    NOT NULL CHECK (state IN ('open', 'resolved', 'closed')),
    -- meridian.v1.TicketResolution's number, and what it cites.
    resolution         integer NOT NULL DEFAULT 0,
    cites              text    NOT NULL DEFAULT '',
    -- Set by a person's act at the ticket's page (W6.23), never a tool's.
    owner              text    NOT NULL DEFAULT '',
    owner_name         text    NOT NULL DEFAULT '',
    due                text    NOT NULL DEFAULT '',
    -- The title and seen text held as suspect (requirement 61), and the
    -- rules they matched, by name.
    suspect            boolean NOT NULL DEFAULT false,
    matched_rules      text[]  NOT NULL DEFAULT '{}',
    -- Concerns, version, step, operation, reason and paths, for a likely
    -- duplicate.
    fingerprint        text    NOT NULL,
    seen_count         bigint  NOT NULL DEFAULT 1,
    first_seen_ns      bigint  NOT NULL,
    last_seen_ns       bigint  NOT NULL,
    filed_at_ns        bigint  NOT NULL
);

-- A repeat folds into the open ticket from the same instance under the same
-- key, even were a second dashboard ever to hear the same filing
-- (kernel/the-bus-has-no-queue-groups).
CREATE UNIQUE INDEX IF NOT EXISTS dashboard_ticket_open_key
    ON dashboard_ticket (filed_instance, idempotency_key)
    WHERE state = 'open' AND idempotency_key <> '';
CREATE INDEX IF NOT EXISTS dashboard_ticket_filed_by_instance
    ON dashboard_ticket (filed_instance, ticket_id);
CREATE INDEX IF NOT EXISTS dashboard_ticket_filer
    ON dashboard_ticket (filed_subject, filed_at_ns);

-- The records a ticket names, by value: given by its filer, or an account
-- found in its text (requirement 7), which can only narrow who sees it.
CREATE TABLE IF NOT EXISTS dashboard_ticket_reference (
    ticket_id      text    NOT NULL REFERENCES dashboard_ticket (ticket_id),
    position       integer NOT NULL,
    kind           text    NOT NULL,
    value          text    NOT NULL,
    account_id     text    NOT NULL DEFAULT '',
    found_in_text  boolean NOT NULL DEFAULT false,
    PRIMARY KEY (ticket_id, position)
);

-- Insert-only: a change of state, owner or due date is a note of kind
-- change, written in the same transaction as the change.
CREATE TABLE IF NOT EXISTS dashboard_ticket_note (
    ticket_id          text    NOT NULL REFERENCES dashboard_ticket (ticket_id),
    -- From 1, as a resolution cites it.
    number             integer NOT NULL,
    -- meridian.v1.TicketNoteKind's number.
    kind               integer NOT NULL,
    author_provenance  text    NOT NULL CHECK (author_provenance IN ('person', 'client', 'plugin', 'rules')),
    author_subject     text    NOT NULL DEFAULT '',
    author_person      text    NOT NULL DEFAULT '',
    author_delegation  text    NOT NULL DEFAULT '',
    author_client      text    NOT NULL DEFAULT '',
    author_instance    text    NOT NULL DEFAULT '',
    noted_ns           bigint  NOT NULL,
    note               text    NOT NULL,
    suspect            boolean NOT NULL DEFAULT false,
    matched_rules      text[]  NOT NULL DEFAULT '{}',
    PRIMARY KEY (ticket_id, number)
);

-- One per change, per person it concerns: names the ticket and the change's
-- kind, never its text (requirement 35).
CREATE TABLE IF NOT EXISTS dashboard_notice (
    -- A ULID, so a reader's place is the last one it read.
    notice_id          text    PRIMARY KEY CHECK (notice_id <> ''),
    subject            text    NOT NULL,
    ticket_id          text    NOT NULL REFERENCES dashboard_ticket (ticket_id),
    kind               text    NOT NULL,
    author_provenance  text    NOT NULL,
    author_subject     text    NOT NULL DEFAULT '',
    author_person      text    NOT NULL DEFAULT '',
    author_delegation  text    NOT NULL DEFAULT '',
    author_client      text    NOT NULL DEFAULT '',
    author_instance    text    NOT NULL DEFAULT '',
    changed_ns         bigint  NOT NULL,
    -- The person's own flag; a delegation's place is apart from it.
    read               boolean NOT NULL DEFAULT false
);

CREATE INDEX IF NOT EXISTS dashboard_notice_subject
    ON dashboard_notice (subject, notice_id);
CREATE INDEX IF NOT EXISTS dashboard_notice_age
    ON dashboard_notice (changed_ns);

-- Each reader's place in a person's inbox: a delegation's id, or empty for
-- the person's own pages (the spec's Q5).
CREATE TABLE IF NOT EXISTS dashboard_inbox_cursor (
    subject    text NOT NULL,
    reader     text NOT NULL,
    notice_id  text NOT NULL,
    PRIMARY KEY (subject, reader)
);
