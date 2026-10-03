-- What a deployment admin does at the dashboard's Instruments page before the
-- book can be written (W3.10, contract v10), written straight into the
-- instrument store for `make e2e-book`, which has no dashboard: the records
-- the custody plugin's identifiers meet, AAPL an equity in US dollars and USD
-- cash, each value with its source and the person who set it, and the two
-- versions that made them. Anything else the suite resolves is minted then,
-- with nothing in force, and the book refuses an entry naming it.
--
-- Applied after the schema and before anything starts; harmless to apply
-- again.

INSERT INTO instrument (instrument_id, asset_class, currency, exchange_mic, description,
                        lifecycle_state, version, valid_from_ns, record_time_ns)
VALUES ('LCL-01J8XQ4M7K00000000E2EAAPL', 'ASSET_CLASS_EQUITY', 'USD', '', 'Apple Inc.',
        'INSTRUMENT_LIFECYCLE_STATE_ACTIVE', 2, 0, 2),
       ('LCL-01J8XQ4M7K000000000E2EUSD', 'ASSET_CLASS_CASH', 'USD', '', 'US dollar',
        'INSTRUMENT_LIFECYCLE_STATE_ACTIVE', 2, 0, 2)
ON CONFLICT DO NOTHING;

INSERT INTO instrument_identifier (instrument_id, scheme, value, source, valid_from_ns, valid_to_ns)
SELECT 'LCL-01J8XQ4M7K00000000E2EAAPL', 'symbol', 'AAPL', 'e2e-book', 0, NULL
 WHERE NOT EXISTS (SELECT 1 FROM instrument_identifier
                    WHERE instrument_id = 'LCL-01J8XQ4M7K00000000E2EAAPL');
INSERT INTO instrument_identifier (instrument_id, scheme, value, source, valid_from_ns, valid_to_ns)
SELECT 'LCL-01J8XQ4M7K000000000E2EUSD', 'iso4217', 'USD', 'e2e-book', 0, NULL
 WHERE NOT EXISTS (SELECT 1 FROM instrument_identifier
                    WHERE instrument_id = 'LCL-01J8XQ4M7K000000000E2EUSD');

INSERT INTO instrument_value_source (instrument_id, field, scheme, namespace, value, source,
                                     person, instance_id, recorded_at_ns, note)
VALUES ('LCL-01J8XQ4M7K00000000E2EAAPL', 'identifier', 'symbol', 'e2e-book', 'AAPL',
        'reported by custody-test-1', '', 'custody-test-1', 1, ''),
       ('LCL-01J8XQ4M7K00000000E2EAAPL', 'asset_class', '', '', '', 'the e2e-book statement',
        'local|e2e-book-admin', '', 2, ''),
       ('LCL-01J8XQ4M7K00000000E2EAAPL', 'currency', '', '', '', 'the e2e-book statement',
        'local|e2e-book-admin', '', 2, ''),
       ('LCL-01J8XQ4M7K00000000E2EAAPL', 'description', '', '', '', 'the e2e-book statement',
        'local|e2e-book-admin', '', 2, ''),
       ('LCL-01J8XQ4M7K000000000E2EUSD', 'identifier', 'iso4217', 'e2e-book', 'USD',
        'reported by custody-test-1', '', 'custody-test-1', 1, ''),
       ('LCL-01J8XQ4M7K000000000E2EUSD', 'asset_class', '', '', '', 'ISO 4217',
        'local|e2e-book-admin', '', 2, ''),
       ('LCL-01J8XQ4M7K000000000E2EUSD', 'currency', '', '', '', 'ISO 4217',
        'local|e2e-book-admin', '', 2, ''),
       ('LCL-01J8XQ4M7K000000000E2EUSD', 'description', '', '', '', 'ISO 4217',
        'local|e2e-book-admin', '', 2, '')
ON CONFLICT DO NOTHING;

INSERT INTO instrument_version (instrument_id, version, operation, changes, person, instance_id,
                                note, merged_instrument_id, record_time_ns)
VALUES ('LCL-01J8XQ4M7K00000000E2EAAPL', 1, 'mint', '[]', '', 'custody-test-1', '', '', 1),
       ('LCL-01J8XQ4M7K00000000E2EAAPL', 2, 'complete', '[]', 'local|e2e-book-admin', '', '', '', 2),
       ('LCL-01J8XQ4M7K000000000E2EUSD', 1, 'mint', '[]', '', 'custody-test-1', '', '', 1),
       ('LCL-01J8XQ4M7K000000000E2EUSD', 2, 'complete', '[]', 'local|e2e-book-admin', '', '', '', 2)
ON CONFLICT DO NOTHING;
