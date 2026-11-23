-- Each record the dashboard keeps of a change is kept as it was written
-- (ruling 1084, as config migration 30 made the configuration store's;
-- review-core-3 3): a delegation's changes, a service account's, a prepared
-- change's steps, what agents may do, a person's own settings and a
-- professional status. The dashboard only ever adds to these tables; from
-- here the database refuses anything else, whoever writes, the dashboard's
-- own roles included -- short of the tables' owner (the role that migrates)
-- or a superuser dropping this trigger, which either may (review-core-3b
-- H). Every table named dashboard_*_change, and one added later, takes it
-- by running this again.
--
-- One exception, and only a removal: a prepared change's steps are removed
-- with it once it settled and was kept its 90 days (W6.28,
-- prepared::KEPT_AFTER_SETTLING_NS), so a step is removed only where its
-- prepared change settled more than 90 days before now; a step of one still
-- pending or settled since, and any update, is refused (review-core-3b A).
-- Now is the deployment's time where the connection names it
-- (meridian.dashboard_now_ns, as the sweep does), else the database's
-- statement_timestamp(). And when a prepared change settled is kept as
-- written once set, so it cannot be moved back to let its steps go early.
--
-- Erasure by key (tasks/sdk-contract/erasure-after-a-hold) needs no further
-- exception: a person's identity data in a record is to be held under a key
-- of that person's and erased by destroying the key, the erasure its own
-- record, so no record here is updated or deleted for it. The free-text
-- names these tables hold today are queued against that plan.
CREATE OR REPLACE FUNCTION dashboard_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    now_ns bigint;
BEGIN
    IF TG_OP = 'DELETE' AND TG_TABLE_NAME = 'dashboard_prepared_call_change' THEN
        now_ns := coalesce(nullif(current_setting('meridian.dashboard_now_ns', true), '')::bigint,
                           (extract(epoch FROM statement_timestamp()) * 1000000000)::bigint);
        -- Settled, and kept its 90 days.
        IF EXISTS (SELECT 1 FROM dashboard_prepared_call settled
                    WHERE settled.prepared_call_id = OLD.prepared_call_id
                      AND settled.settled_at_ns < now_ns - 7776000000000000) THEN
            RETURN OLD;
        END IF;
    END IF;
    RAISE EXCEPTION 'the record % is append-only: a change is recorded once and kept as written',
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
           AND tablename LIKE 'dashboard\_%\_change'
    LOOP
        EXECUTE format('DROP TRIGGER IF EXISTS dashboard_append_only ON %I', kept);
        EXECUTE format('CREATE TRIGGER dashboard_append_only BEFORE UPDATE OR DELETE ON %I '
                       'FOR EACH ROW EXECUTE FUNCTION dashboard_append_only()', kept);
    END LOOP;
END
$$;

CREATE OR REPLACE FUNCTION dashboard_settled_once() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.settled_at_ns IS NOT NULL AND NEW.settled_at_ns IS DISTINCT FROM OLD.settled_at_ns THEN
        RAISE EXCEPTION 'a prepared change settles once: when it settled is kept as written'
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS dashboard_settled_once ON dashboard_prepared_call;
CREATE TRIGGER dashboard_settled_once BEFORE UPDATE ON dashboard_prepared_call
    FOR EACH ROW EXECUTE FUNCTION dashboard_settled_once();
