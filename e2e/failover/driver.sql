-- What a deployment admin does before the book can be written (W6.3, W6.4,
-- W4.11), written straight into the configuration store for e2e/failover's
-- write driver (driver.py), which runs with no platform and no admin: twelve
-- open accounts, one key each in the driver's history; a user group, an
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
       ('ACC-DRIVE-11', 'Driver 11', 1, 1)
ON CONFLICT DO NOTHING;

INSERT INTO config_user_group (user_group_id, name) VALUES ('ug-drive', 'Driver desk')
ON CONFLICT DO NOTHING;
INSERT INTO config_account_group (account_group_id, name, account_ids)
VALUES ('ag-drive', 'Driver accounts', '{ACC-DRIVE-00,ACC-DRIVE-01,ACC-DRIVE-02,ACC-DRIVE-03,ACC-DRIVE-04,ACC-DRIVE-05,ACC-DRIVE-06,ACC-DRIVE-07,ACC-DRIVE-08,ACC-DRIVE-09,ACC-DRIVE-10,ACC-DRIVE-11}')
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
