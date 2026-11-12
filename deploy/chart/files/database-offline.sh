#!/bin/sh
# One act on the brought database's stopped volume (templates/database-
# offline.yaml), run in a Job the CLI makes from the suspended CronJob with
# the act named, once every component and the database itself are stopped:
#
#   restore-to-time   `meridian restore --to-time` (step 3b; spec/a-
#                     deployment-scales-out-and-fails-over-by-configuration
#                     sections 4 and 9): the volume restored from the WAL
#                     archive's newest base backup before
#                     MERIDIAN_RESTORE_TO_TIME on the timeline that held that
#                     time, the archive replayed along that timeline to it,
#                     and the database promoted there on a timeline of its
#                     own, then stopped cleanly. The archive keeps what came
#                     after, on the timelines it was on.
#   restore-to-archive-end
#                     `meridian restore --to-archive-end` (step 3c, the same
#                     sections): the same, to the last commit the archive
#                     holds, along its newest timeline -- where the
#                     deployment was when it stopped.
#   enable-checksums  `meridian upgrade-deployment --enable-checksums` (ruling
#                     1058): PostgreSQL's pg_checksums --enable on the
#                     volume, once it is shut down cleanly (started and
#                     stopped once if it was not). Proportional to its size.
#
# The timeline is chosen, never asked (step 3c; spec section 9). Each restore
# starts a timeline of its own, and pgBackRest's default follows the newest,
# so a second restore to a time before the first one's would replay the
# wrong history. The archive is read before the volume is touched: its base
# backups, its timelines' histories and when each began (its history file's
# time in the archive, written as it was promoted), and its last commit (the
# newest commit or abort pg_waldump finds in its newest segments). A time is
# held by the newest timeline begun by then; it is restored from the newest
# base backup before it on that timeline's history, named to pgBackRest
# (--set, --target-timeline), since pgBackRest's own choice, the newest
# backup before the time, can lie past where that timeline forked. A time before the oldest base backup ended, or at or past
# the last commit -- a restore to a time stops before the first commit after
# it -- is outside the archive, refused naming the archive's first and last
# times, with the volume as it was; a time no timeline held, while the
# database was stopped, is found by the replay and refused the same way.
#
# Under Patroni the CLI runs it on member 0's volume. For enable-checksums it
# switches the primary to member 0 first; a restore does not switch
# the primary first -- every member is stopped, and the archive, not a
# member, is what is restored from. After any act the CLI deletes the
# standbys' volumes, which Patroni makes again from member 0, and after a
# restore it clears Patroni's leader, sync and failover records, which name
# the primary and the position from before it.
# Never prints the passphrase, a credential or a row.
set -eu
say() { echo "database-offline: $*"; }
fail() { echo "database-offline: FAILED: $*" >&2; exit 1; }
as_postgres() { su-exec postgres "$@"; }

act=${MERIDIAN_DATABASE_ACT:-}
PGDATA=${PGDATA:-/var/lib/postgresql/data/pgdata}
export PGDATA
[ -n "$act" ] || fail "no act named: MERIDIAN_DATABASE_ACT is restore-to-time, restore-to-archive-end or enable-checksums, which meridian restore --to-time, meridian restore --to-archive-end and meridian upgrade-deployment --enable-checksums set"
[ -f "$PGDATA/PG_VERSION" ] || fail "$PGDATA holds no database"

archive=""
[ -f /etc/pgbackrest/pgbackrest.conf ] && archive=yes
mkdir -p /tmp/pgbackrest /var/run/postgresql
chown postgres:postgres /tmp/pgbackrest /var/run/postgresql

# The database as it starts here: on its socket alone, and archiving as it
# does in its pod where the archive is on, so no segment it completes is
# recycled unarchived. Without TLS: a socket needs none, and the
# configuration a base backup restores -- Patroni's, under internal TLS
# (tls.internal) -- names the certificates its pod copies to /run/meridian-
# tls, which this Job has not got, and Postgres will not start on a
# certificate it cannot load. Without hot standby: the configuration a base
# backup restores is the one the database had then, and the WAL replayed
# after it can hold a raise of max_connections (the move to ha raises it),
# max_worker_processes, max_wal_senders, max_prepared_transactions or
# max_locks_per_transaction. Replaying one past this start's own setting,
# hot standby pauses recovery for good; without it, Postgres checks none of
# them and replays on. Nothing is read while it replays, so nothing is lost.
options="-c listen_addresses='' -c unix_socket_directories=/var/run/postgresql -c ssl=off -c hot_standby=off"
if [ -n "$archive" ]; then
	options="$options -c archive_mode=on -c archive_command='pgbackrest --stanza=meridian archive-push %p'"
fi
# Started, and waited for until it accepts connections: without hot
# standby, only once its recovery has ended (for a restore, promoted at the
# time). Fails at once if the server stops first.
start() { as_postgres pg_ctl -D "$PGDATA" -w -t "${MERIDIAN_DATABASE_START_SECONDS:-3600}" -l /tmp/postgres.log -o "$options" start >/dev/null; }
stop() { as_postgres pg_ctl -D "$PGDATA" -w -t 600 -m fast stop >/dev/null; }
running() { as_postgres pg_ctl -D "$PGDATA" status >/dev/null 2>&1; }
said() { grep -E "$1" /tmp/postgres.log 2>/dev/null | sed -e 's/^.*\(LOG\|FATAL\|PANIC\|ERROR\|WARNING\|DETAIL\): *//' | tail -"${2:-3}" | tr '\n' ' '; }
# Postgres's own last lines, where `meridian logs database-offline` reads
# them, before the act fails: what it said of why. Left out: pgBackRest's
# line for each segment it fetched, and the connections refused while it
# replays without hot standby, with nothing to say.
failed() {
	say "Postgres's log, its last lines:"
	grep -v -e ' P00 ' -e 'not accepting connections' -e 'Hot standby mode is disabled' /tmp/postgres.log 2>/dev/null \
		| tail -20 | sed -e 's/^/database-offline: | /'
	fail "$*"
}
asked() { as_postgres psql -X -At -h /var/run/postgresql -U postgres -d postgres -c "$1"; }
control() { as_postgres pg_controldata "$PGDATA" | sed -n "s/^$1: *//p"; }

# Left by a server that stopped without its shutdown (the volume is not in
# use: the CLI stopped the database before making this Job).
stale_pid() {
	if [ -f "$PGDATA/postmaster.pid" ]; then
		say "the database was not shut down cleanly: its postmaster.pid is removed, and the act recovers it"
		rm -f "$PGDATA/postmaster.pid"
	fi
}

# The archive read before a restore touches the volume, and the restore
# planned from it (the header's "the timeline is chosen"): its first and
# last times said, the time named refused where it is outside them, and the
# timeline and base backup to restore along and from written to
# /tmp/pgbackrest/plan for the act to read. Python, as Patroni is in this
# image. $1 is the time named, or nothing for the archive's end.
plan() {
	python3 - "$1" <<'PY'
import json, os, re, shutil, subprocess, sys
from datetime import datetime, timezone

to_named = sys.argv[1]
# Where each history file and segment read is fetched to, the database's
# user's, as pgBackRest writes as it.
WAL = "/tmp/pgbackrest/wal"
os.makedirs(WAL, exist_ok=True)
shutil.chown(WAL, "postgres", "postgres")
# Segments read back from the newest for the last commit: an archive whose
# newest this many hold none is said to have no last commit found.
MOST_SEGMENTS = 64


def say(text):
    print(f"database-offline: {text}", flush=True)


def fail(text):
    # Every refusal here comes before the restore: the volume is untouched.
    say("nothing was restored: the database's volume is as it was")
    print(f"database-offline: FAILED: {text}", file=sys.stderr, flush=True)
    sys.exit(1)


def as_postgres(*command):
    return subprocess.run(["su-exec", "postgres", *command], capture_output=True, text=True,
                          env={**os.environ, "TZ": "UTC"})


def pgbackrest(*args):
    done = as_postgres("pgbackrest", "--stanza=meridian", *args)
    if done.returncode != 0:
        said = (done.stderr + done.stdout).strip()
        said = said[said.find("ERROR"):] if "ERROR" in said else said or "it said nothing"
        fail(f"the archive could not be read: {' '.join(said.split())[:400]}")
    return done.stdout


MOMENT = re.compile(r"(\d{4})-(\d\d)-(\d\d)[ T](\d\d):(\d\d):(\d\d)(?:\.(\d{1,6}))?\s*"
                    r"(?:(UTC|Z)|([+-])(\d\d)(?::?(\d\d))?)$")


def moment(text):
    """A time as the CLI names it ('YYYY-MM-DD HH:MM:SS[.f]+00') or pg_waldump
    writes it ('... UTC'), as seconds since the epoch."""
    m = MOMENT.match(text.strip())
    if not m:
        fail(f"no time this act reads: {text!r}, as 'YYYY-MM-DD HH:MM:SS+00'")
    y, mo, d, h, mi, s, frac, utc, sign, oh, om = m.groups()
    at = datetime(int(y), int(mo), int(d), int(h), int(mi), int(s), tzinfo=timezone.utc).timestamp()
    at += int((frac or "0").ljust(6, "0")) / 1e6
    if sign:
        at -= (1 if sign == "+" else -1) * (int(oh) * 3600 + int(om or 0) * 60)
    return at


def said(at):
    whole = datetime.fromtimestamp(int(at), timezone.utc).strftime("%Y-%m-%d %H:%M:%S")
    micro = round((at - int(at)) * 1e6)
    return f"{whole}.{micro:06d}+00" if micro else f"{whole}+00"


def lsn(text):
    high, low = text.split("/")
    return (int(high, 16) << 32) | int(low, 16)


info = json.loads(pgbackrest("info", "--output=json") or "[]")
stanza = next((s for s in info if s.get("name") == "meridian"), None)
if not stanza or not stanza.get("db"):
    fail("the archive holds no stanza yet: the database's archive container makes it once the database is primary")
current = max(d["id"] for d in stanza["db"])
archive_of = {a["database"]["id"]: a["id"] for a in stanza.get("archive", [])}
backups = [{"label": b["label"], "db": b["database"]["id"], "timeline": int(b["archive"]["start"][:8], 16),
            "lsn": lsn(b["lsn"]["stop"]), "stop": b["timestamp"]["stop"]}
           for b in stanza.get("backup", []) if b.get("archive") and b.get("lsn")]
if not backups:
    fail(("the time named" if to_named else "the archive's end") +
         " is outside the archive: it holds no base backup yet, which `meridian backup` or its schedule takes")

# WAL segments are 16 MiB unless initdb said otherwise, as pg_control keeps.
control = as_postgres("pg_controldata", os.environ["PGDATA"]).stdout
segment_bytes = int((re.search(r"Bytes per WAL segment:\s*(\d+)", control) or [0, 16 << 20])[1])
per_log = 0x100000000 // segment_bytes
SEGMENT = re.compile(r"(?:^|/)([0-9A-F]{8})([0-9A-F]{8})([0-9A-F]{8})-[0-9a-f]{40}(?:\.\w+)?$")


def timelines_of(db):
    """One database's timelines in the archive: when each began, its
    history (each ancestor and where it left it), and its segments."""
    archive_id = archive_of.get(db)
    listed = json.loads(pgbackrest("repo-ls", f"archive/meridian/{archive_id}", "--recurse",
                                   "--output=json") or "{}") if archive_id else {}
    began, history, segments = {1: None}, {1: []}, {}
    for name, entry in listed.items():
        if entry.get("type") != "file":
            continue
        if re.fullmatch(r"[0-9A-F]{8}\.history", name):
            timeline = int(name[:8], 16)
            began[timeline] = entry.get("time")
            got = os.path.join(WAL, name)
            pgbackrest("archive-get", name, got)
            with open(got) as held:
                history[timeline] = [(int(f[0]), lsn(f[1])) for f in
                                     (line.split() for line in held if line.strip() and not line.startswith("#"))]
            os.unlink(got)
        elif (m := SEGMENT.search(name)):
            segments[(int(m[1], 16), int(m[2], 16) * per_log + int(m[3], 16))] = m[1] + m[2] + m[3]
    for timeline, _ in segments:
        began.setdefault(timeline, None if timeline == 1 else float("inf"))
        history.setdefault(timeline, [])
    return began, history, segments


def on_history(backup, timeline, history):
    """Whether a base backup lies on a timeline's history: taken on it, or
    on an ancestor before the timeline left it."""
    return backup["timeline"] == timeline or any(
        parent == backup["timeline"] and backup["lsn"] <= left for parent, left in history[timeline])


def last_commit(timeline, history, segments):
    """The newest commit or abort along a timeline's history, from its
    newest segments back: its time, or None where none is found."""
    path = [(timeline, None)] + [(parent, left) for parent, left in reversed(history[timeline])]
    read = []
    for along, (on, upto) in enumerate(path):
        for (t, number), name in segments.items():
            if t == on and (upto is None or number <= upto // segment_bytes):
                read.append((number, -along, name, upto))
    read.sort(reverse=True)
    for number, _, name, upto in read[:MOST_SEGMENTS]:
        got = os.path.join(WAL, name)
        pgbackrest("archive-get", name, got)
        dumped = as_postgres("pg_waldump", got).stdout
        os.unlink(got)
        found = None
        for line in dumped.splitlines():
            m = re.search(r"lsn: ([0-9A-F]+/[0-9A-F]+),.*desc: (?:COMMIT|ABORT) (\d{4}-\d\d-\d\d \d\d:\d\d:\d\d(?:\.\d+)? \w+)", line)
            if m and (upto is None or lsn(m[1]) <= upto):
                found = m[2]
        if found:
            return moment(found)
    return None


newest_db = {current: timelines_of(current)}
began, history, segments = newest_db[current]
newest = max(began)
last = last_commit(newest, history, segments)
first = min(b["stop"] for b in backups)
holds = f"{said(first)} to {said(last)}" if last is not None else f"{said(first)} on"
say(f"the archive holds {holds}" + ("" if last is not None else
    f" (no commit in its newest {MOST_SEGMENTS} segments)") + f"; its newest timeline is {newest}")

if to_named:
    to = moment(to_named)
    if to <= first:
        fail(f"{to_named} is outside the archive, which holds {holds}: it is before its oldest base backup ended")
    if last is not None and to >= last:
        fail(f"{to_named} is outside the archive, which holds {holds}: a restore to a time stops before the "
             f"first commit after it, so the latest it reaches is just before the last commit; the archive's "
             f"end itself is restored with meridian restore --to-archive-end")
    # The newest database with a base backup before the time, and in it the
    # newest timeline begun by then; the time held by it only if that
    # timeline's last commit is after it, and not while the database was
    # stopped between it and the next.
    db = max(b["db"] for b in backups if b["stop"] < to)
    if db not in newest_db:
        newest_db[db] = timelines_of(db)
    began, history, segments = newest_db[db]
    along = max(t for t, at in began.items() if at is None or at <= to)
    if (db, along) != (current, newest):
        ended = last_commit(along, history, segments)
        if ended is not None and to >= ended:
            after = min((at, t) for t, at in began.items() if at is not None and at > to)
            fail(f"{to_named} is outside the archive, which holds {holds}: no timeline held it -- timeline "
                 f"{along} ended at its last commit, {said(ended)}, and timeline {after[1]} began at "
                 f"{said(after[0])}, while the database was stopped between them")
    candidates = [b for b in backups if b["db"] == db and b["stop"] < to and on_history(b, along, history)]
    if not candidates:
        fail(f"{to_named} is outside the archive, which holds {holds}: timeline {along} held it, and no base "
             f"backup before it lies on that timeline's history")
else:
    along = newest
    candidates = [b for b in backups if b["db"] == current and on_history(b, along, history)]
    if not candidates:
        fail(f"the archive's end is on timeline {along}, and no base backup lies on that timeline's history")
chosen = max(candidates, key=lambda b: b["stop"])
with open("/tmp/pgbackrest/plan", "w") as plan:
    plan.write(f"along={along}\nbackup_set={chosen['label']}\nholds='{holds}'\n"
               f"last='{said(last) if last is not None else ''}'\n")
PY
}

# The restore along the timeline and from the base backup planned, the
# database started and waited for until it has promoted, and stopped: what
# both restores do once planned. $1 is the time named, or nothing for the
# archive's end.
restored() {
	to=$1
	stale_pid
	if [ -n "$to" ]; then
		say "restoring to $to along timeline $along, the one that held it, from base backup $backup_set"
		set -- --type=time "--target=$to" --target-action=promote
		reached=$to
	else
		say "restoring to the archive's end${last:+, its last commit at $last,} along timeline $along from base backup $backup_set"
		set --
		reached="the archive's end"
	fi
	as_postgres pgbackrest --stanza=meridian restore --delta "--set=$backup_set" "--target-timeline=$along" "$@" \
		>/tmp/pgbackrest/restore.log 2>&1 \
		|| fail "the restore did not finish: $(grep -m1 ERROR /tmp/pgbackrest/restore.log || tail -1 /tmp/pgbackrest/restore.log)"
	say "the base backup restored; the archive replays to $reached, and nothing answers until it is there"
	# The server stopped before the time: outside the archive -- a time no
	# timeline held, while the database was stopped -- or for a cause of
	# its own, in Postgres's words either way.
	stopped_short() {
		outside=$(said 'recovery ended before configured recovery target|recovery stop point is before consistent recovery point' 1)
		[ -z "$outside" ] || failed "$reached is outside the archive, which holds $holds: no timeline held it ($outside)"
		failed "the database stopped before it reached $reached: $(said 'FATAL|PANIC|recovery' 3)"
	}
	if ! start; then
		running || stopped_short
		failed "the archive was still replaying after ${MERIDIAN_DATABASE_START_SECONDS:-3600}s"
	fi
	# Answering, it has promoted. Were hot standby on, it would answer
	# while replaying, and a pause is said at once, not waited out.
	waited=0
	until [ "$(asked 'select pg_is_in_recovery()' 2>/dev/null)" = f ]; do
		running || stopped_short
		paused=$(asked 'select pg_get_wal_replay_pause_state()' 2>/dev/null || true)
		[ -z "$paused" ] || [ "$paused" = "not paused" ] \
			|| failed "recovery $paused before it reached $reached: $(said 'recovery has paused|lower setting' 2)"
		[ "$waited" -lt "${MERIDIAN_DATABASE_START_SECONDS:-3600}" ] || failed "the archive was still replaying after ${waited}s"
		sleep 2
		waited=$((waited + 2))
	done
	timeline=$(asked 'select timeline_id from pg_control_checkpoint()')
	stop
	say "promoted at $reached on timeline $timeline, recovered along timeline $along: $(said 'recovery stopping|last completed transaction' 2)"
}

began=$(date +%s)
case "$act" in
restore-to-time | restore-to-archive-end)
	to=""
	if [ "$act" = restore-to-time ]; then
		to=${MERIDIAN_RESTORE_TO_TIME:-}
		[ -n "$to" ] || fail "no time named: MERIDIAN_RESTORE_TO_TIME, as 'YYYY-MM-DD HH:MM:SS+00'"
	fi
	[ -n "$archive" ] || fail "this deployment keeps no WAL archive (backup.walArchive is off): a backup's own file is restored with meridian restore"
	rm -f /tmp/pgbackrest/plan
	plan "$to" || exit 1
	. /tmp/pgbackrest/plan
	restored "$to"
	;;
enable-checksums)
	version=$(control 'Data page checksum version')
	if [ "${version:-0}" != 0 ]; then
		say "page checksums are on already (version $version): nothing to do"
		exit 0
	fi
	# Shut down cleanly, as a primary or as a standby, pg_checksums takes it.
	state=$(control 'Database cluster state')
	if [ "$state" != "shut down" ] && [ "$state" != "shut down in recovery" ]; then
		# A standby's volume, started without hot standby, would wait for a
		# primary and never answer: said now, not waited out.
		[ ! -f "$PGDATA/standby.signal" ] \
			|| fail "member 0's volume is a standby's ($state): the CLI makes member 0 the primary before this act"
		stale_pid
		say "started once to recover, and stopped, so its shutdown is clean"
		start || failed "the database did not start to recover: $(said 'FATAL|PANIC' 3)"
		stop
	fi
	size=$(du -sk "$PGDATA" | cut -f1)
	say "enabling page checksums on ${size} KiB: every page read, and written where its checksum is not yet right"
	as_postgres pg_checksums --enable -D "$PGDATA" >/tmp/checksums.log 2>&1 \
		|| fail "pg_checksums did not finish: $(tail -2 /tmp/checksums.log)"
	[ "$(control 'Data page checksum version')" != 0 ] || fail "pg_checksums finished and the control file says checksums are off"
	say "page checksums enabled: $(grep -E 'Files (scanned|written)|Blocks (scanned|written)' /tmp/checksums.log | tr '\n' ' ')"
	;;
*)
	fail "$act is no act this Job knows: restore-to-time, restore-to-archive-end or enable-checksums"
	;;
esac
say "done in $(( $(date +%s) - began ))s"
