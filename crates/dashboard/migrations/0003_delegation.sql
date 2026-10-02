-- Delegations (decisions/029; spec/clients-act-on-a-persons-delegation;
-- W6.14, W6.17, W6.18): the clients that registered, the delegations people
-- made to them, and the tokens issued on those, kept here so a restart or an
-- upgrade leaves them standing and revoking one ends it on every replica.
--
-- No token is kept: a token's SHA-256, in unpadded base64url, is, and there
-- is no column the token could go in, so a dump of these tables acts for
-- nobody. A delegation names a person, a client and what it covers, and
-- nothing the person may do; access is evaluated per request from the
-- records, intersected with what it covers.
--
-- The bounds -- ten minutes, 90 days, a week's notice, a week's groups --
-- are in the binary, as decisions/015's are, not here.
--
-- Written to be run again harmlessly, as every migration here is.

-- A registered client (RFC 7591). Public: there is no secret to keep.
CREATE TABLE IF NOT EXISTS dashboard_oauth_client (
    client_id         text    PRIMARY KEY CHECK (client_id <> ''),
    -- Its own choice, and shown on the consent page as that.
    name              text    NOT NULL,
    redirect_uris     text[]  NOT NULL,
    -- What it says it is (`meridian-cli`), trusted for nothing but defaults.
    software_id       text    NOT NULL DEFAULT '',
    registered_at_ns  bigint  NOT NULL,
    -- Whether anybody has consented to it. One nobody has is removed after
    -- a day, and first when registrations reach their cap.
    consented         boolean NOT NULL DEFAULT false
);

CREATE INDEX IF NOT EXISTS dashboard_oauth_client_unconsented
    ON dashboard_oauth_client (registered_at_ns) WHERE NOT consented;

-- A person's access, or part of it, lent to one client (requirement 4).
CREATE TABLE IF NOT EXISTS dashboard_delegation (
    delegation_id      text    PRIMARY KEY CHECK (delegation_id <> ''),
    subject            text    NOT NULL,
    display_name       text    NOT NULL DEFAULT '',
    client_id          text    NOT NULL
                       REFERENCES dashboard_oauth_client (client_id) ON DELETE CASCADE,

    -- What it covers: everything the person holds, as that changes; or the
    -- plugin levels (`instance:level`), the account groups and the
    -- deployment admin flag named at consent, intersected per request.
    covers_everything        boolean NOT NULL,
    covers_deployment_admin  boolean NOT NULL DEFAULT false,
    covers_plugins           text[]  NOT NULL DEFAULT '{}',
    covers_account_groups    text[]  NOT NULL DEFAULT '{}',

    made_at_ns         bigint  NOT NULL,
    renewed_at_ns      bigint  NOT NULL,
    expires_at_ns      bigint  NOT NULL,

    revoked_at_ns      bigint,
    revoked_by         text,
    revoked_why        text,

    last_used_at_ns    bigint,
    last_refused_at_ns bigint,
    last_refusal       text,

    -- The directory groups it is evaluated with, and when they were read:
    -- at a sign-in (the firm's provider), or again by the dashboard's own
    -- bind whenever a token is issued (LDAP).
    directory_groups   text[]  NOT NULL DEFAULT '{}',
    groups_read_at_ns  bigint  NOT NULL
);

-- A person's own list, and a deployment admin revoking all of a person's.
CREATE INDEX IF NOT EXISTS dashboard_delegation_subject
    ON dashboard_delegation (subject);

-- At most one standing delegation per person and client: consenting again
-- renews it.
CREATE UNIQUE INDEX IF NOT EXISTS dashboard_delegation_standing
    ON dashboard_delegation (subject, client_id) WHERE revoked_at_ns IS NULL;

-- A token issued on a delegation, by fingerprint.
CREATE TABLE IF NOT EXISTS dashboard_delegation_token (
    fingerprint    text    PRIMARY KEY CHECK (fingerprint <> ''),
    kind           text    NOT NULL CHECK (kind IN ('access', 'refresh')),
    -- The one resource an access token is accepted at (RFC 8707).
    resource       text    NOT NULL,
    delegation_id  text    NOT NULL
                   REFERENCES dashboard_delegation (delegation_id) ON DELETE CASCADE,
    client_id      text    NOT NULL,
    expires_at_ns  bigint  NOT NULL,
    -- A refresh token is spent by its first use; presented again, it revokes
    -- the delegation.
    spent_at_ns    bigint
);

CREATE INDEX IF NOT EXISTS dashboard_delegation_token_delegation
    ON dashboard_delegation_token (delegation_id);
