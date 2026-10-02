-- What a deployment admin does through the dashboard before the book can be
-- written (W6.3, W6.4, W4.11), written straight into the configuration store
-- for `make e2e-book`, which has no admin: two open accounts, each with the
-- custody plugin's external account linked to it; a permission putting both
-- in the custody plugin's and the operations plugin's write scopes; and
-- another putting the first alone in the reporting plugin's read scope.
-- operations-test-2 is named by no entry here, and its scope holds neither.
--
-- Applied after the schema and before anything starts, so the conductor's
-- first answer holds it; harmless to apply again.

INSERT INTO config_account (account_id, name, state, created_at_ns)
VALUES ('ACC-BOOK-A', 'Book Brokerage', 1, 1), ('ACC-BOOK-B', 'Book Futures', 1, 1)
ON CONFLICT DO NOTHING;

INSERT INTO config_external_account_link (plugin_instance_id, external_account_id, account_id)
VALUES ('custody-test-1', 'ext-book-a', 'ACC-BOOK-A'),
       ('custody-test-1', 'ext-book-b', 'ACC-BOOK-B')
ON CONFLICT DO NOTHING;

INSERT INTO config_user_group (user_group_id, name) VALUES ('ug-book', 'Book desk')
ON CONFLICT DO NOTHING;
INSERT INTO config_account_group (account_group_id, name, account_ids)
VALUES ('ag-book', 'Book accounts', '{ACC-BOOK-A,ACC-BOOK-B}'),
       ('ag-book-a', 'Book Brokerage alone', '{ACC-BOOK-A}')
ON CONFLICT DO NOTHING;
INSERT INTO config_access_group (access_group_id, name)
VALUES ('grp-book', 'Book writers'), ('grp-book-reading', 'Book readers')
ON CONFLICT DO NOTHING;
-- AccessLevel 2: write; 1: read.
INSERT INTO config_access_entry (access_group_id, position, plugin_instance_id, level)
VALUES ('grp-book', 0, 'custody-test-1', 2),
       ('grp-book', 1, 'operations-test-1', 2),
       ('grp-book-reading', 0, 'reporting-test-1', 1)
ON CONFLICT DO NOTHING;
INSERT INTO config_permission (permission_id, user_group_id, account_group_id, access_group_id)
VALUES ('perm-book', 'ug-book', 'ag-book', 'grp-book'),
       ('perm-book-reading', 'ug-book', 'ag-book-a', 'grp-book-reading')
ON CONFLICT DO NOTHING;
