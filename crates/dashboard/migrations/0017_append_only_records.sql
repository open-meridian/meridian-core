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
-- Now is the database's statement_timestamp(), or the deployment's time
-- where the connection names an earlier one (meridian.dashboard_now_ns, as
-- the sweep does): a named time only tightens it (review-core-3c N2). And
-- when a prepared change settled is set once, no earlier than now, and
-- kept as written, so it cannot be moved back to let its steps go early.
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
        -- The earlier of the two: a time named on the connection only
        -- tightens the database's own (least ignores one not named).
        now_ns := least(nullif(current_setting('meridian.dashboard_now_ns', true), '')::bigint,
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

-- When a prepared change settled, kept as written once set. Its trigger is
-- made before any change table's, so this takes the prepared change's table
-- first and a change table after, as the runtime does (a settle holds the
-- prepared change's row, then adds its step): with the release before
-- still serving, the two never deadlock (review-core-3c N1).
--
-- And it settles when it is settled: set the first time -- or added
-- already settled -- it is no earlier than the database's own time, less a
-- skew for a replica's clock a little behind and the moments a settle
-- takes, so a prepared change is never said to have settled long ago to let
-- its steps go early (review-core-3c N2, review-core-3d F6). A restore loads
-- the rows before it makes this trigger, so what settled long ago comes
-- back as it was.
CREATE OR REPLACE FUNCTION dashboard_settled_once() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    -- Five minutes.
    settle_skew_ns CONSTANT bigint := 300000000000;
    settled_first boolean := TG_OP = 'INSERT';
BEGIN
    IF TG_OP = 'UPDATE' THEN
        IF OLD.settled_at_ns IS NOT NULL AND NEW.settled_at_ns IS DISTINCT FROM OLD.settled_at_ns THEN
            RAISE EXCEPTION 'a prepared change settles once: when it settled is kept as written'
                USING ERRCODE = 'insufficient_privilege';
        END IF;
        settled_first := OLD.settled_at_ns IS NULL;
    END IF;
    IF settled_first AND NEW.settled_at_ns
           < (extract(epoch FROM statement_timestamp()) * 1000000000)::bigint - settle_skew_ns THEN
        RAISE EXCEPTION 'a prepared change settles when it is settled: % is before now', NEW.settled_at_ns
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS dashboard_settled_once ON dashboard_prepared_call;
CREATE TRIGGER dashboard_settled_once BEFORE INSERT OR UPDATE ON dashboard_prepared_call
    FOR EACH ROW EXECUTE FUNCTION dashboard_settled_once();

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
