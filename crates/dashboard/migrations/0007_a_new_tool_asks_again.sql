-- A tool added to a consented row asks again before it changes anything
-- (contract v18; spec/a-deployment-serves-its-mcp, ruled 2026-10-09 on v17's
-- security review, Nit 1). A delegation consented row by row keeps the
-- tools that change something its consent page listed; one a release or a
-- plugin adds later is not listed to it until the person consents afresh.
-- A delegation covering everything the person holds follows their grants,
-- new tools included, and keeps none (NULL).
--
-- Delegations narrowed before this release kept no such list: which tools
-- their pages showed was not recorded. Each is filled once, the first time
-- it is evaluated after the upgrade, with the tools that change something
-- it reached then, and says so (acting_backfilled_at_ns, the moment it was
-- filled, never back-dated; decisions/031).
--
-- Written to be run again harmlessly, as every migration here is.
ALTER TABLE dashboard_delegation ADD COLUMN IF NOT EXISTS covers_acting text[];
ALTER TABLE dashboard_delegation ADD COLUMN IF NOT EXISTS acting_backfilled_at_ns bigint;
