-- The terminal sessions from before delegations are retired (contract v15's
-- sweep; spec/clients-act-on-a-persons-delegation, requirement 22): the CLI
-- connects by delegation since 0.1.25, and the dashboard serves no older CLI.
-- Each session lasted twelve hours at most and named no act of anybody's, so
-- the table goes with them; what a person did through one was recorded where
-- it was done, by the store that did it.
DROP TABLE IF EXISTS dashboard_terminal_session_gone;
DROP TABLE IF EXISTS dashboard_terminal_session;
