-- Placeholders and replacements. W3.7 and W3.8.
--
-- The first thing this store holds that the platform cannot send again. An
-- instrument is the platform's and comes back on demand; a placeholder is the
-- deployment's own, and holding rows elsewhere are recorded against it. Lose
-- one and the same identifier set is given a new placeholder, the old one is
-- never announced again, and whatever was recorded against it waits for an
-- identity nobody is asking for. So these tables are not rebuilt, and the
-- "no history to preserve" of 0001 holds for it and not for them: a change to
-- them that is not an addition needs a migration history like the street
-- store's first.
--
-- Additions only, `IF NOT EXISTS` throughout, applied after 0001 by the same
-- `migrate`.

CREATE TABLE IF NOT EXISTS instrument_placeholder (
    -- LCL-, minted here.
    placeholder_id text   PRIMARY KEY,

    -- The identifier set, sorted and deduplicated and written so no value can
    -- pass for a separator. Unique, which is what makes one placeholder per set
    -- a property of the table rather than of whichever resolve got there first.
    identifier_key text   NOT NULL UNIQUE,

    source         text   NOT NULL DEFAULT '',
    asset_class    text   NOT NULL DEFAULT '',

    -- The date the resolve that minted it asked about, which an escalation
    -- targets.
    as_of_ns       bigint NOT NULL,
    minted_at_ns   bigint NOT NULL
);

CREATE TABLE IF NOT EXISTS instrument_placeholder_identifier (
    placeholder_id text NOT NULL
        REFERENCES instrument_placeholder (placeholder_id) ON DELETE CASCADE,
    scheme         text NOT NULL,
    value          text NOT NULL,

    -- Empty for a global scheme.
    source         text NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS instrument_placeholder_identifier_owner
    ON instrument_placeholder_identifier (placeholder_id);

-- What an ID became. Kept apart from the placeholder rather than as a column on
-- it, because what is replaced is not always a placeholder minted here: an
-- LCL- instrument the platform minted before it minted only INS- is replaced
-- the same way, and so is a placeholder a restore lost the row for.
--
-- Never deleted and never updated. The first pairing stands, and a reader
-- holding a replaced ID can learn what it became for good.
CREATE TABLE IF NOT EXISTS instrument_replacement (
    replaced_id    text   PRIMARY KEY,
    replaced_by    text   NOT NULL,
    replaced_at_ns bigint NOT NULL
);
