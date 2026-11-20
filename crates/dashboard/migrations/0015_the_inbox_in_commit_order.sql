-- A person's inbox read in the order its notices were committed (W6.24,
-- "each client a person connects sees every new notice once"; review-E F2).
--
-- Until now a reader's place was the last notice ID it read, a ULID: the
-- time the notice was minted, not when its transaction committed. A notice
-- minted before another but committed after it -- two copies of the
-- dashboard, or one slow transaction -- fell behind a place already past
-- it and was never read. Now each of a person's ticket notices takes the
-- next number of the person's own sequence from a head row, inside the
-- transaction that inserts it (the changes crate's pattern): the head's row
-- is held until that transaction ends, so a person's numbers are committed
-- in their order and none is skipped; one rolled back gives its number
-- back. A reader's place is the last number it read.
--
-- Taken by a trigger, so every writer of a ticket notice takes one, a
-- dashboard of the release before this one among them while an upgrade
-- rolls. Only a ticket's notices: the others (a prepared change's, a
-- service account's, a latency, health or gap notice) are not read by a
-- place.
--
-- The notices already kept are numbered in their IDs' order, the order
-- their readers read them in, and each reader's place is set to the number
-- of the last notice at or before the ID it held: nothing it had not read
-- is skipped by the change, and nothing it read is read again. Adding the
-- column takes the notices' table whole until this commits, so no notice is
-- inserted while they are numbered.
--
-- While an upgrade rolls, a dashboard of the release before still moves a
-- reader's place by its notice ID alone, never its number. A trigger on
-- the places keeps the number with it: where a place's notice ID moves and
-- its number does not, the number is moved on only past what the release
-- before delivered, the notices after the number it had whose IDs lie
-- between the place's old ID and its new one, and stops before the first
-- that does not (the review of v31a-core-1, finding 2; the delta review of
-- 77afd5d0, finding 1). A notice numbered after the place but with an ID
-- below the place's was never read by the release before, and this one
-- still reads it: while the upgrade rolls a notice may be read twice, and
-- none is lost. A new place the release before makes is moved on the same
-- way, from nothing; a place made with no notice ID is left at 0.
--
-- Written to be run again harmlessly, as every migration here is.

ALTER TABLE dashboard_notice ADD COLUMN IF NOT EXISTS inbox_seq bigint;

CREATE TABLE IF NOT EXISTS dashboard_inbox_head (
    subject  text   PRIMARY KEY,
    -- The last number taken for this person's notices.
    seq      bigint NOT NULL
);

ALTER TABLE dashboard_inbox_cursor ADD COLUMN IF NOT EXISTS seq bigint NOT NULL DEFAULT 0;

WITH numbered AS (
    SELECT n.notice_id,
           coalesce(h.seq, 0)
             + row_number() OVER (PARTITION BY n.subject ORDER BY n.notice_id) AS seq
      FROM dashboard_notice n
      LEFT JOIN dashboard_inbox_head h ON h.subject = n.subject
     WHERE n.ticket_id IS NOT NULL AND n.inbox_seq IS NULL
)
UPDATE dashboard_notice n SET inbox_seq = numbered.seq
  FROM numbered WHERE n.notice_id = numbered.notice_id;

INSERT INTO dashboard_inbox_head (subject, seq)
SELECT subject, max(inbox_seq) FROM dashboard_notice
 WHERE inbox_seq IS NOT NULL GROUP BY subject
ON CONFLICT (subject) DO UPDATE SET seq = greatest(dashboard_inbox_head.seq, excluded.seq);

UPDATE dashboard_inbox_cursor c SET seq = coalesce((
    SELECT max(n.inbox_seq) FROM dashboard_notice n
     WHERE n.subject = c.subject AND n.inbox_seq IS NOT NULL AND n.notice_id <= c.notice_id
), 0)
 WHERE c.seq = 0;

CREATE UNIQUE INDEX IF NOT EXISTS dashboard_notice_inbox_order
    ON dashboard_notice (subject, inbox_seq) WHERE inbox_seq IS NOT NULL;

CREATE OR REPLACE FUNCTION dashboard_notice_takes_its_number() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.ticket_id IS NOT NULL AND NEW.inbox_seq IS NULL THEN
        INSERT INTO dashboard_inbox_head AS head (subject, seq) VALUES (NEW.subject, 1)
        ON CONFLICT (subject) DO UPDATE SET seq = head.seq + 1
        RETURNING head.seq INTO NEW.inbox_seq;
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS dashboard_notice_takes_its_number ON dashboard_notice;
CREATE TRIGGER dashboard_notice_takes_its_number
    BEFORE INSERT ON dashboard_notice
    FOR EACH ROW EXECUTE FUNCTION dashboard_notice_takes_its_number();

CREATE OR REPLACE FUNCTION dashboard_inbox_cursor_keeps_its_number() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    had  bigint := 0;
    was  text := '';
    upto bigint;
BEGIN
    IF coalesce(NEW.notice_id, '') = '' THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        IF NEW.notice_id IS NOT DISTINCT FROM OLD.notice_id OR NEW.seq <> OLD.seq THEN
            RETURN NEW;
        END IF;
        had := OLD.seq;
        was := coalesce(OLD.notice_id, '');
    ELSIF NEW.seq <> 0 THEN
        RETURN NEW;
    END IF;
    -- The first notice after the number it had that the release before
    -- did not deliver: its ID at or below the old one, or past the new.
    SELECT min(n.inbox_seq) - 1 INTO upto FROM dashboard_notice n
     WHERE n.subject = NEW.subject AND n.inbox_seq > had
       AND (n.notice_id <= was OR n.notice_id > NEW.notice_id);
    IF upto IS NULL THEN
        SELECT max(n.inbox_seq) INTO upto FROM dashboard_notice n
         WHERE n.subject = NEW.subject AND n.inbox_seq IS NOT NULL;
    END IF;
    NEW.seq := greatest(had, coalesce(upto, 0));
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS dashboard_inbox_cursor_keeps_its_number ON dashboard_inbox_cursor;
CREATE TRIGGER dashboard_inbox_cursor_keeps_its_number
    BEFORE INSERT OR UPDATE ON dashboard_inbox_cursor
    FOR EACH ROW EXECUTE FUNCTION dashboard_inbox_cursor_keeps_its_number();
