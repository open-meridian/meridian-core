-- Contract v28 (W10.1, W10.13; spec/every-data-source-is-licensed-on-the-
-- same-terms, sections 2 and 4; rulings 377 to 389, 754): a person's answer
-- to the professional test and its records, and who could use each dataset
-- in each calendar month, all kept by the dashboard.
--
-- Written to be run again harmlessly, as every migration here is. Nothing
-- is backfilled: nobody was asked the test before this, so nobody answered,
-- and unanswered reads as professional, which is what it was. The counts
-- begin at the month this ran: who could use a dataset before it was not
-- kept, and is not made up.

-- A person's answer that stands (`professional_status_set`'s latest), per
-- test: `standard`, the platform's one text, or a dataset's ID where its
-- terms use the vendor's own definition.
CREATE TABLE IF NOT EXISTS dashboard_professional_status (
    subject            text    NOT NULL CHECK (subject <> ''),
    test               text    NOT NULL CHECK (test <> ''),
    -- `professional` or `non_professional`.
    status             text    NOT NULL CHECK (status IN ('professional', 'non_professional')),
    set_at_ns          bigint  NOT NULL,
    PRIMARY KEY (subject, test)
);

-- Each answer and each change of one, its own record
-- (`professional_status_set`, decisions/031): the person, the test, from
-- and to, who set it -- the person, or a deployment admin with a note --
-- through which delegation and client, when and why. Never updated, never
-- deleted.
CREATE TABLE IF NOT EXISTS dashboard_professional_status_change (
    change_id          bigserial PRIMARY KEY,
    subject            text      NOT NULL,
    test               text      NOT NULL,
    from_said          text      NOT NULL DEFAULT '',
    to_said            text      NOT NULL,
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS dashboard_professional_status_change_by_subject
    ON dashboard_professional_status_change (subject, change_id);

-- Who could use a dataset in a calendar month (W10.13), computed from the
-- records and the entitlements each minute: a person able to read it, a
-- plugin instance entitled to it, a service account able to read it; first
-- and last seen within the month, its classification as last seen (a
-- person's professional status, a plugin's category), and the dataset's
-- underlying source. Kept at least three years: nothing here deletes.
CREATE TABLE IF NOT EXISTS dashboard_data_use (
    -- `YYYY-MM`, in UTC.
    month              text    NOT NULL CHECK (month ~ '^[0-9]{4}-[0-9]{2}$'),
    dataset            text    NOT NULL CHECK (dataset <> ''),
    -- `person`, `plugin` or `service_account`.
    kind               text    NOT NULL CHECK (kind IN ('person', 'plugin', 'service_account')),
    who                text    NOT NULL CHECK (who <> ''),
    classification     text    NOT NULL DEFAULT '',
    underlying_source  text    NOT NULL DEFAULT '',
    first_seen_ns      bigint  NOT NULL,
    last_seen_ns       bigint  NOT NULL,
    PRIMARY KEY (month, dataset, kind, who)
);
