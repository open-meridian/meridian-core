-- Contract v23 (W3.1; the order-path plan's "Approved 2026-10-13, item 6's
-- names"): a plugin gives the security's currency with its provenance --
-- stated by its source, or, where the source states none, derived by a named
-- rule or supplied by a person on its page -- in
-- ResolveIdentifierRequest.provenance. The store keeps the entry with the
-- currency's offer, the Provenance encoded as the plugin sent it, as the
-- street keeps an activity's re-resolution's; empty for a value the source
-- stated and for every offer of the platform's, which is what every offer
-- kept before this was.
--
-- Additions only, `IF NOT EXISTS`, so it is run again harmlessly.

ALTER TABLE instrument_offer
    ADD COLUMN IF NOT EXISTS provenance bytea NOT NULL DEFAULT ''::bytea;
