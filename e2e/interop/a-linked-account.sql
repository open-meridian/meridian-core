-- What a deployment admin does through the dashboard before a connector's rows
-- can land (W6.4, W4.11), written straight into the configuration store for
-- the interop suite, which has no admin: an open account, the interop
-- plugin's external account linked to it, and a permission that puts it in
-- the plugin's write scope.
--
-- Applied by `make interop` after the schema and before anything starts, so
-- the conductor's first answer already holds it; and harmless to apply to a
-- database that already has it.

INSERT INTO config_account (account_id, name, state, created_at_ns)
VALUES ('ACC-INTEROP', 'Interop', 1, 1)
ON CONFLICT DO NOTHING;

INSERT INTO config_external_account_link (plugin_instance_id, external_account_id, account_id)
VALUES ('custody-test-1', 'ext-interop', 'ACC-INTEROP')
ON CONFLICT DO NOTHING;

INSERT INTO config_user_group (user_group_id, name) VALUES ('ug-interop', 'Interop')
ON CONFLICT DO NOTHING;
INSERT INTO config_account_group (account_group_id, name, account_ids)
VALUES ('ag-interop', 'Interop', '{ACC-INTEROP}')
ON CONFLICT DO NOTHING;
INSERT INTO config_access_group (access_group_id, name) VALUES ('grp-interop', 'Interop')
ON CONFLICT DO NOTHING;
-- AccessLevel 2: write.
INSERT INTO config_access_entry (access_group_id, position, plugin_instance_id, tag, level)
VALUES ('grp-interop', 0, 'custody-test-1', 'holdings', 2)
ON CONFLICT DO NOTHING;
INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
VALUES ('perm-interop', 'ug-interop', 'ag-interop', 'grp-interop')
ON CONFLICT DO NOTHING;
