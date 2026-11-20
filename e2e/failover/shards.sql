-- The subjects a plugin instance's shards share (step 3d, checkpoint 5, item
-- 33), written straight into the instrument store for e2e/failover's scaled
-- run, which has no dashboard, as e2e/book/instruments.sql's are: thirty-two
-- equities in US dollars, LCL-01J8XQ4M7K00000000E2ESH00 to ...E2ESH31, each
-- value with its source and the person who set it, and the two versions
-- that made them. The shard driver (driver.py, DRIVER_ROLE=shard) records a
-- price of each it serves, and the shard reader's standing want names all
-- thirty-two (run.py's SHARD_SUBJECTS).
--
-- Applied after the schema and before the instrument store's members start
-- again (run.py's driver_fixtures); harmless to apply again.

INSERT INTO instrument (instrument_id, asset_class, currency, exchange_mic, description,
                        lifecycle_state, version, valid_from_ns, record_time_ns)
SELECT 'LCL-01J8XQ4M7K00000000E2ESH' || lpad(n::text, 2, '0'), 'ASSET_CLASS_EQUITY', 'USD', '',
       'Shard proof ' || lpad(n::text, 2, '0'), 'INSTRUMENT_LIFECYCLE_STATE_ACTIVE', 2, 0, 2
  FROM generate_series(0, 31) AS n
ON CONFLICT DO NOTHING;

INSERT INTO instrument_identifier (instrument_id, scheme, value, source, valid_from_ns, valid_to_ns)
SELECT 'LCL-01J8XQ4M7K00000000E2ESH' || lpad(n::text, 2, '0'), 'symbol',
       'SHARD' || lpad(n::text, 2, '0'), 'e2e-failover', 0, NULL
  FROM generate_series(0, 31) AS n
 WHERE NOT EXISTS (SELECT 1 FROM instrument_identifier
                    WHERE instrument_id = 'LCL-01J8XQ4M7K00000000E2ESH' || lpad(n::text, 2, '0'));

INSERT INTO instrument_value_source (instrument_id, field, scheme, namespace, value, source,
                                     person, instance_id, recorded_at_ns, note)
SELECT 'LCL-01J8XQ4M7K00000000E2ESH' || lpad(n::text, 2, '0'), field.name, field.scheme, field.namespace,
       CASE WHEN field.name = 'identifier' THEN 'SHARD' || lpad(n::text, 2, '0') ELSE '' END,
       field.source, field.person, field.instance_id, field.recorded_at_ns, ''
  FROM generate_series(0, 31) AS n,
       (VALUES ('identifier', 'symbol', 'e2e-failover', 'reported by shard-proof', '', 'shard-proof', 1),
               ('asset_class', '', '', 'the e2e-failover fixture', 'local|e2e-failover-admin', '', 2),
               ('currency', '', '', 'the e2e-failover fixture', 'local|e2e-failover-admin', '', 2),
               ('description', '', '', 'the e2e-failover fixture', 'local|e2e-failover-admin', '', 2))
         AS field (name, scheme, namespace, source, person, instance_id, recorded_at_ns)
ON CONFLICT DO NOTHING;

INSERT INTO instrument_version (instrument_id, version, operation, changes, person, instance_id,
                                note, merged_instrument_id, record_time_ns)
SELECT 'LCL-01J8XQ4M7K00000000E2ESH' || lpad(n::text, 2, '0'), made.version, made.operation, '[]',
       made.person, made.instance_id, '', '', made.version
  FROM generate_series(0, 31) AS n,
       (VALUES (1, 'mint', '', 'shard-proof'), (2, 'complete', 'local|e2e-failover-admin', ''))
         AS made (version, operation, person, instance_id)
ON CONFLICT DO NOTHING;
