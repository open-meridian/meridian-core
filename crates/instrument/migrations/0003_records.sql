-- Contract v10: a deployment's instrument records are its own (decisions/030),
-- each value with its source and person, every version kept, offers beside
-- them, conflicts for a person (spec/a-deployment-completes-its-instrument-records).
--
-- Additions only, `IF NOT EXISTS` throughout, applied after 0002 by the same
-- `migrate`; then `migrate` sources what was held before, once (Q8). These
-- tables are the deployment's own and are never rebuilt from the platform:
-- a change to them that is not an addition needs a migration of its own.

-- The identifier set a record was minted for (W3.7): one record per set,
-- decided by the unique index, as one placeholder per set was. Empty for a
-- record applied from the platform before v10.
ALTER TABLE instrument ADD COLUMN IF NOT EXISTS mint_key text;
CREATE UNIQUE INDEX IF NOT EXISTS instrument_mint_key
    ON instrument (mint_key) WHERE mint_key IS NOT NULL;

-- Where each value in force came from. One row per value: the asset class,
-- the currency, the description, and each identifier (by scheme, namespace
-- and value; empty for the others).
CREATE TABLE IF NOT EXISTS instrument_value_source (
    instrument_id  text   NOT NULL REFERENCES instrument (instrument_id) ON DELETE CASCADE,
    field          text   NOT NULL,
    scheme         text   NOT NULL DEFAULT '',
    namespace      text   NOT NULL DEFAULT '',
    value          text   NOT NULL DEFAULT '',
    source         text   NOT NULL,
    person         text   NOT NULL DEFAULT '',
    instance_id    text   NOT NULL DEFAULT '',
    recorded_at_ns bigint NOT NULL,
    note           text   NOT NULL DEFAULT '',
    PRIMARY KEY (instrument_id, field, scheme, namespace, value)
);

-- Values offered beside them, in force only when a person accepts one.
CREATE TABLE IF NOT EXISTS instrument_offer (
    instrument_id text   NOT NULL REFERENCES instrument (instrument_id) ON DELETE CASCADE,
    field         text   NOT NULL,
    scheme        text   NOT NULL DEFAULT '',
    namespace     text   NOT NULL DEFAULT '',
    value         text   NOT NULL,
    source        text   NOT NULL,
    instance_id   text   NOT NULL DEFAULT '',
    offered_at_ns bigint NOT NULL,
    PRIMARY KEY (instrument_id, field, scheme, namespace, value, instance_id, source)
);

-- Every version of a record, append-only (W3.12). Never updated, never
-- deleted. `changes` is the version's changes as JSON, a list of
-- {field, scheme, namespace, before, after, source}.
CREATE TABLE IF NOT EXISTS instrument_version (
    instrument_id        text   NOT NULL,
    version              bigint NOT NULL,
    operation            text   NOT NULL,
    changes              text   NOT NULL DEFAULT '[]',
    person               text   NOT NULL DEFAULT '',
    instance_id          text   NOT NULL DEFAULT '',
    note                 text   NOT NULL DEFAULT '',
    merged_instrument_id text   NOT NULL DEFAULT '',
    record_time_ns       bigint NOT NULL,
    PRIMARY KEY (instrument_id, version)
);

-- Identifiers that met more than one record, listed until a merge settles
-- them. One row per set of identifiers that disagree.
CREATE TABLE IF NOT EXISTS instrument_conflict (
    conflict_key   text   PRIMARY KEY,
    identifiers    text   NOT NULL,
    instrument_ids text   NOT NULL,
    reported_by    text   NOT NULL DEFAULT '',
    first_seen_ns  bigint NOT NULL,
    last_seen_ns   bigint NOT NULL
);

-- Q8: a placeholder not yet replaced becomes an ordinary record, under the ID
-- holdings and book entries already carry, with its identifiers and no values.
-- One replaced before v10 stays replaced, and resolves to what replaced it.
INSERT INTO instrument (instrument_id, asset_class, currency, exchange_mic, description,
                        lifecycle_state, version, valid_from_ns, record_time_ns, mint_key)
     SELECT placeholder.placeholder_id, '', '', '', '', 'INSTRUMENT_LIFECYCLE_STATE_ACTIVE', 1,
            0, placeholder.minted_at_ns, placeholder.identifier_key
       FROM instrument_placeholder placeholder
      WHERE NOT EXISTS (SELECT 1 FROM instrument_replacement replacement
                         WHERE replacement.replaced_id = placeholder.placeholder_id)
ON CONFLICT DO NOTHING;

INSERT INTO instrument_identifier (instrument_id, scheme, value, source, valid_from_ns, valid_to_ns)
     SELECT member.placeholder_id, member.scheme, member.value, member.source, 0, NULL
       FROM instrument_placeholder_identifier member
       JOIN instrument record ON record.instrument_id = member.placeholder_id
      WHERE record.mint_key IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM instrument_identifier held
                         WHERE held.instrument_id = member.placeholder_id
                           AND held.scheme = member.scheme
                           AND held.value = member.value
                           AND held.source = member.source);
