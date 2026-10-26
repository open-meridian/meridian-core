-- The minimal company kind (contract v28; W1.32, W3.32; FDP Q6; ruling 594,
-- the ENT- prefix; ruling 612, the store's ask): the platform's company
-- records the deployment holds, each version the platform answered kept and
-- never changed, its own record (decisions/031). The deployment mints no
-- company record of its own: every row here is the platform's `ENT-` record.
-- Each identifier any version carried is indexed for a resolve, its validity
-- window read from the record itself, so a resolve as of a date before an
-- identifier left still finds the company that held it.
--
-- Additions only, `IF NOT EXISTS`, so it is run again harmlessly.

CREATE TABLE IF NOT EXISTS instrument_company (
    company_id  text   NOT NULL,
    version     bigint NOT NULL,
    record      bytea  NOT NULL,
    kept_at_ns  bigint NOT NULL,
    PRIMARY KEY (company_id, version)
);

CREATE TABLE IF NOT EXISTS instrument_company_identifier (
    scheme      text NOT NULL,
    value       text NOT NULL,
    company_id  text NOT NULL,
    PRIMARY KEY (scheme, value, company_id)
);
