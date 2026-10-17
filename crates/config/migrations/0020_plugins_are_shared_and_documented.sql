-- Contract v25 (sdk-contract/plugins-are-shared-and-documented-contract):
-- where a version came from, and its retirement.
--
-- Where a version came from (W8.10, W8.14): `local` for one uploaded or
-- tracked from a branch -- every version recorded before v25 was, so the
-- default is their fact, not a guess -- `community` or `verified` for one
-- installed from a registry or carried in by a bundle; the listing it
-- belongs to, `organisation/plugin`, empty for a local version; and, for a
-- bundle's, when the signing keys' status it was checked against was issued.
ALTER TABLE config_plugin_version
    ADD COLUMN IF NOT EXISTS source_registry         text   NOT NULL DEFAULT 'local',
    ADD COLUMN IF NOT EXISTS listing                 text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS key_status_issued_at_ns bigint NOT NULL DEFAULT 0;

-- A version retired from the catalogue (W8.15; decision 59): who, through
-- which delegation and client, from which prepared change, when and why,
-- written once beside the version, whose row is kept. No version is retired
-- before v25, so nothing is owed to the past.
ALTER TABLE config_plugin_version
    ADD COLUMN IF NOT EXISTS retired_by                 text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_at_ns              bigint NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS retired_note               text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_through_delegation text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_client_name        text   NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS retired_prepared_call_id   text   NOT NULL DEFAULT '';

-- An install from a registry is its own record (decisions/031; decided
-- 2026-10-17 under agreement A): beside uploaded_by and uploaded_at_ns,
-- the delegation and client the admin acted through, the prepared change
-- it was made from, and the install's note, written once with the version.
-- Empty for a local upload, and for every version recorded before v25,
-- which carried no note.
ALTER TABLE config_plugin_version
    ADD COLUMN IF NOT EXISTS uploaded_through_delegation text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS uploaded_client_name        text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS uploaded_prepared_call_id   text NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS uploaded_note               text NOT NULL DEFAULT '';

-- The identifier schemes the deployment declares it is licensed for (W3.28),
-- each declaration its own record (decisions/031): the whole list, who,
-- through which delegation and client, from which prepared change, when and
-- why. None is "none declared", which v24's reports sent; nothing was
-- declared before v25, so nothing is owed to the past. Never updated, never
-- deleted.
CREATE TABLE IF NOT EXISTS config_licensed_schemes_change (
    change_id          bigserial PRIMARY KEY,
    licensed_schemes   text[]    NOT NULL DEFAULT '{}',
    changed_by         text      NOT NULL DEFAULT '',
    through_delegation text      NOT NULL DEFAULT '',
    client_name        text      NOT NULL DEFAULT '',
    prepared_call_id   text      NOT NULL DEFAULT '',
    changed_at_ns      bigint    NOT NULL,
    note               text      NOT NULL DEFAULT ''
);

-- A version's documentation as uploaded (W8.1, contract v25; the plan's
-- `config_plugin_documentation`): each section the vendor wrote, its
-- Markdown exactly as the image's docs/ held it, in the declaration's order,
-- written with the version and never replaced; the declaration names each
-- by digest and size. No version before v25 carried any.
CREATE TABLE IF NOT EXISTS config_plugin_documentation (
    name     text    NOT NULL,
    version  text    NOT NULL,
    section  text    NOT NULL CHECK (section IN ('overview', 'setup', 'troubleshooting', 'terms', 'pages')),
    ordinal  integer NOT NULL,
    markdown text    NOT NULL,
    PRIMARY KEY (name, version, section),
    FOREIGN KEY (name, version) REFERENCES config_plugin_version
);

-- A registry version's listing statement (W8.10, W8.3; contract v25), the
-- platform's DSSE envelope as the resolution answered it, kept with the
-- version for the verification again at launch. Empty for a local version.
ALTER TABLE config_plugin_version
    ADD COLUMN IF NOT EXISTS listing_statement text NOT NULL DEFAULT '';

-- A deployment admin keeping an instance whose version was pulled (W8.12):
-- the PullAcknowledgement, encoded, kept on its launch. NULL while none.
ALTER TABLE config_plugin_launch
    ADD COLUMN IF NOT EXISTS pull_acknowledgement bytea;

-- The conductor's latest copy of the platform's catalogue (W8.10): one row,
-- replaced at each pull -- the listings (a PullCatalogueReply's, encoded),
-- the sequence the next pull reads after, the signing keys' status as the
-- platform signed it (its JSON) and when it was pulled.
CREATE TABLE IF NOT EXISTS config_registry_catalogue (
    id           smallint PRIMARY KEY CHECK (id = 1),
    listings     bytea    NOT NULL,
    sequence     bigint   NOT NULL,
    key_status   text     NOT NULL,
    pulled_at_ns bigint   NOT NULL
);

-- The plugin usage report's setting (W5.19, contract v25), each change its
-- own record (decisions/031): on or off, who, through which delegation and
-- client, when and why. None is "on", the default. Never updated, never
-- deleted.
CREATE TABLE IF NOT EXISTS config_reports_plugin_usage_change (
    change_id            bigserial PRIMARY KEY,
    reports_plugin_usage boolean   NOT NULL,
    changed_by           text      NOT NULL DEFAULT '',
    through_delegation   text      NOT NULL DEFAULT '',
    client_name          text      NOT NULL DEFAULT '',
    prepared_call_id     text      NOT NULL DEFAULT '',
    changed_at_ns        bigint    NOT NULL,
    note                 text      NOT NULL DEFAULT ''
);

-- Each plugin usage report sent, its own record (SentPluginUsageReport,
-- encoded): the counts, when, and whether the platform received it. Never
-- updated, never deleted.
CREATE TABLE IF NOT EXISTS config_plugin_usage_report (
    report_id  bigserial PRIMARY KEY,
    sent_at_ns bigint    NOT NULL,
    report     bytea     NOT NULL
);
