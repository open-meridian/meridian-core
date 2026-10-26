-- A starter pack's install under way (ruling 616; spec/starter-packs,
-- requirement 18): the job a restart would otherwise lose. One row while it
-- runs -- the pack as ticked from, each ticked member's state so far (a
-- PackStatus's members, encoded), who asked through what (the Envelope,
-- encoded, for the records each member's install leaves) and why -- written
-- at its start and after each member, and deleted once its `pack_installed`
-- record is written. The record, never this row, is the history; a
-- conductor that starts and finds a row carries the job on from the members
-- still installing.
CREATE TABLE IF NOT EXISTS config_pack_job (
    pack        text   PRIMARY KEY,
    held        bytea  NOT NULL,
    members     bytea  NOT NULL,
    envelope    bytea  NOT NULL,
    note        text   NOT NULL DEFAULT '',
    began_at_ns bigint NOT NULL
);
