-- What a deployment admin does before the book can be written (W6.3, W6.4,
-- W4.11), written straight into the configuration store for e2e/failover's
-- write driver (driver.py), which runs with no platform and no admin:
-- thirty-six open accounts (run.py's DRIVER_ACCOUNTS), one key each in the
-- driver's history, enough that the driver writes to the run's end (part 10:
-- twelve were full before the ha run's rebalance began); a user group, an
-- account group holding them and an access group; and a permission putting
-- them in the write scope of the driver's oms instance (drive-oms). The
-- instrument its orders and prices name is e2e/book/instruments.sql's.
--
-- Applied after the schema and before the stores serve the driver, as
-- e2e/book/accounts.sql is; harmless to apply again.

INSERT INTO config_account (account_id, name, state, created_at_ns)
VALUES ('ACC-DRIVE-00', 'Driver 00', 1, 1),
       ('ACC-DRIVE-01', 'Driver 01', 1, 1),
       ('ACC-DRIVE-02', 'Driver 02', 1, 1),
       ('ACC-DRIVE-03', 'Driver 03', 1, 1),
       ('ACC-DRIVE-04', 'Driver 04', 1, 1),
       ('ACC-DRIVE-05', 'Driver 05', 1, 1),
       ('ACC-DRIVE-06', 'Driver 06', 1, 1),
       ('ACC-DRIVE-07', 'Driver 07', 1, 1),
       ('ACC-DRIVE-08', 'Driver 08', 1, 1),
       ('ACC-DRIVE-09', 'Driver 09', 1, 1),
       ('ACC-DRIVE-10', 'Driver 10', 1, 1),
       ('ACC-DRIVE-11', 'Driver 11', 1, 1),
       ('ACC-DRIVE-12', 'Driver 12', 1, 1),
       ('ACC-DRIVE-13', 'Driver 13', 1, 1),
       ('ACC-DRIVE-14', 'Driver 14', 1, 1),
       ('ACC-DRIVE-15', 'Driver 15', 1, 1),
       ('ACC-DRIVE-16', 'Driver 16', 1, 1),
       ('ACC-DRIVE-17', 'Driver 17', 1, 1),
       ('ACC-DRIVE-18', 'Driver 18', 1, 1),
       ('ACC-DRIVE-19', 'Driver 19', 1, 1),
       ('ACC-DRIVE-20', 'Driver 20', 1, 1),
       ('ACC-DRIVE-21', 'Driver 21', 1, 1),
       ('ACC-DRIVE-22', 'Driver 22', 1, 1),
       ('ACC-DRIVE-23', 'Driver 23', 1, 1),
       ('ACC-DRIVE-24', 'Driver 24', 1, 1),
       ('ACC-DRIVE-25', 'Driver 25', 1, 1),
       ('ACC-DRIVE-26', 'Driver 26', 1, 1),
       ('ACC-DRIVE-27', 'Driver 27', 1, 1),
       ('ACC-DRIVE-28', 'Driver 28', 1, 1),
       ('ACC-DRIVE-29', 'Driver 29', 1, 1),
       ('ACC-DRIVE-30', 'Driver 30', 1, 1),
       ('ACC-DRIVE-31', 'Driver 31', 1, 1),
       ('ACC-DRIVE-32', 'Driver 32', 1, 1),
       ('ACC-DRIVE-33', 'Driver 33', 1, 1),
       ('ACC-DRIVE-34', 'Driver 34', 1, 1),
       ('ACC-DRIVE-35', 'Driver 35', 1, 1)
ON CONFLICT DO NOTHING;

INSERT INTO config_user_group (user_group_id, name) VALUES ('ug-drive', 'Driver desk')
ON CONFLICT DO NOTHING;
INSERT INTO config_account_group (account_group_id, name, account_ids)
VALUES ('ag-drive', 'Driver accounts', '{ACC-DRIVE-00,ACC-DRIVE-01,ACC-DRIVE-02,ACC-DRIVE-03,ACC-DRIVE-04,ACC-DRIVE-05,ACC-DRIVE-06,ACC-DRIVE-07,ACC-DRIVE-08,ACC-DRIVE-09,ACC-DRIVE-10,ACC-DRIVE-11,ACC-DRIVE-12,ACC-DRIVE-13,ACC-DRIVE-14,ACC-DRIVE-15,ACC-DRIVE-16,ACC-DRIVE-17,ACC-DRIVE-18,ACC-DRIVE-19,ACC-DRIVE-20,ACC-DRIVE-21,ACC-DRIVE-22,ACC-DRIVE-23,ACC-DRIVE-24,ACC-DRIVE-25,ACC-DRIVE-26,ACC-DRIVE-27,ACC-DRIVE-28,ACC-DRIVE-29,ACC-DRIVE-30,ACC-DRIVE-31,ACC-DRIVE-32,ACC-DRIVE-33,ACC-DRIVE-34,ACC-DRIVE-35}')
ON CONFLICT DO NOTHING;
INSERT INTO config_access_group (access_group_id, name) VALUES ('grp-drive', 'Driver writers')
ON CONFLICT DO NOTHING;
-- AccessLevel 2: write, on the role the instance holds (v15).
INSERT INTO config_access_entry (access_group_id, position, plugin_instance_id, level, role)
VALUES ('grp-drive', 0, 'drive-oms', 2, 'oms')
ON CONFLICT DO NOTHING;
INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
VALUES ('perm-drive', 'ug-drive', 'ag-drive', 'grp-drive')
ON CONFLICT DO NOTHING;

-- Contract v30 (W12.4): the standing-coordinator class's subject,
-- drive-standing, holding reporting: read on the driver's accounts, so a
-- standing job over one of them is within its read scope.
INSERT INTO config_access_group (access_group_id, name) VALUES ('grp-drive-standing', 'Driver standing reader')
ON CONFLICT DO NOTHING;
-- AccessLevel 1: read.
INSERT INTO config_access_entry (access_group_id, position, plugin_instance_id, level, role)
VALUES ('grp-drive-standing', 0, 'drive-standing', 1, 'reporting')
ON CONFLICT DO NOTHING;
INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
VALUES ('perm-drive-standing', 'ug-drive', 'ag-drive', 'grp-drive-standing')
ON CONFLICT DO NOTHING;

-- The ems stand-in's own account (the order-plugin class's subject,
-- drive-desk, holding oms and ems): in its write scope on both roles, and
-- no other instance's.
INSERT INTO config_account (account_id, name, state, created_at_ns)
VALUES ('ACC-DRIVE-DESK', 'Driver desk', 1, 1)
ON CONFLICT DO NOTHING;
INSERT INTO config_account_group (account_group_id, name, account_ids)
VALUES ('ag-drive-desk', 'Driver desk account', '{ACC-DRIVE-DESK}')
ON CONFLICT DO NOTHING;
INSERT INTO config_access_group (access_group_id, name) VALUES ('grp-drive-desk', 'Driver desk')
ON CONFLICT DO NOTHING;
INSERT INTO config_access_entry (access_group_id, position, plugin_instance_id, level, role)
VALUES ('grp-drive-desk', 0, 'drive-desk', 2, 'oms'),
       ('grp-drive-desk', 1, 'drive-desk', 2, 'ems')
ON CONFLICT DO NOTHING;
INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
VALUES ('perm-drive-desk', 'ug-drive', 'ag-drive-desk', 'grp-drive-desk')
ON CONFLICT DO NOTHING;
