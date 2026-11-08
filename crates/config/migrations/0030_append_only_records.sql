-- Each administrative act's record is kept as it was written (ruling 1084,
-- audit logging; step 3b part 4). A person's or an agent's change to a
-- plugin's settings, to who holds access, to a hold, to the archive, to a
-- licence, an entitlement, a priority or a deployment setting, and every
-- move, is a row of its own in one of the configuration store's change
-- tables (decisions/031), captured in the store's change log with its
-- digest (step 3b, integrity at rest) and read back at the pages and tools
-- that draw it. The store only ever adds to them; from here the database
-- refuses anything else, so a record is changed by nobody, the store's own
-- roles included, short of a superuser dropping this trigger.
--
-- One exception, the product owner's (2026-10-05), and only its value: a
-- setting's values blanked on every change that held one (the redaction,
-- recorded as its own change), every other column as it was.
CREATE OR REPLACE FUNCTION config_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND TG_TABLE_NAME = 'config_plugin_setting_change' THEN
        IF to_jsonb(NEW) -> 'value' = 'null'::jsonb
           AND (to_jsonb(NEW) - 'value') = (to_jsonb(OLD) - 'value') THEN
            RETURN NEW;
        END IF;
    END IF;
    RAISE EXCEPTION 'the record % is append-only: an act is recorded once and kept as written',
        TG_TABLE_NAME
        USING ERRCODE = 'insufficient_privilege';
END
$$;

DO $$
DECLARE
    kept text;
BEGIN
    FOR kept IN
        SELECT tablename FROM pg_tables
         WHERE schemaname = current_schema()
           AND (tablename LIKE 'config\_%\_change' OR tablename = 'config_record_move')
    LOOP
        EXECUTE format('DROP TRIGGER IF EXISTS config_append_only ON %I', kept);
        EXECUTE format('CREATE TRIGGER config_append_only BEFORE UPDATE OR DELETE ON %I '
                       'FOR EACH ROW EXECUTE FUNCTION config_append_only()', kept);
    END LOOP;
END
$$;
