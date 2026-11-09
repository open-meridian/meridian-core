#!/bin/sh
# One act on the brought database's stopped volume (templates/database-
# offline.yaml), run in a Job the CLI makes from the suspended CronJob with
# the act named, once every component and the database itself are stopped:
#
#   restore-to-time   `meridian restore --to-time` (step 3b; spec/a-
#                     deployment-scales-out-and-fails-over-by-configuration
#                     section 4): the volume restored from the WAL archive's
#                     newest base backup before MERIDIAN_RESTORE_TO_TIME, the
#                     archive replayed to that time, and the database
#                     promoted there on a timeline of its own, then stopped
#                     cleanly. The archive keeps what came after, on the old
#                     timeline. A time past the archive's end, or before its
#                     oldest backup, fails and leaves the volume restorable.
#   enable-checksums  `meridian upgrade-deployment --enable-checksums` (ruling
#                     1058): PostgreSQL's pg_checksums --enable on the
#                     volume, once it is shut down cleanly (started and
#                     stopped once if it was not). Proportional to its size.
#
# Under Patroni the CLI runs it on member 0's volume. For enable-checksums it
# switches the primary to member 0 first; restore-to-time does not switch
# the primary first -- every member is stopped, and the archive, not a
# member, is what is restored from. After either act the CLI deletes the
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
[ -n "$act" ] || fail "no act named: MERIDIAN_DATABASE_ACT is restore-to-time or enable-checksums, which meridian restore --to-time and meridian upgrade-deployment --enable-checksums set"
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

began=$(date +%s)
case "$act" in
restore-to-time)
	to=${MERIDIAN_RESTORE_TO_TIME:-}
	[ -n "$to" ] || fail "no time named: MERIDIAN_RESTORE_TO_TIME, as 'YYYY-MM-DD HH:MM:SS+00'"
	[ -n "$archive" ] || fail "this deployment keeps no WAL archive (backup.walArchive is off): a backup's own file is restored with meridian restore"
	stale_pid
	say "restoring to $to from the archive's newest base backup before it"
	as_postgres pgbackrest --stanza=meridian restore --delta --type=time "--target=$to" \
		--target-action=promote >/tmp/pgbackrest/restore.log 2>&1 \
		|| {
			grep -q 'unable to find backup set with stop time less than' /tmp/pgbackrest/restore.log \
				&& fail "$to is outside the archive: it is before its oldest base backup ended"
			fail "the restore did not finish: $(grep -m1 ERROR /tmp/pgbackrest/restore.log || tail -1 /tmp/pgbackrest/restore.log)"
		}
	say "the base backup restored; the archive replays to $to, and nothing answers until it is there"
	# The server stopped before the time: outside the archive -- past its
	# last archived segment, or before its oldest base backup's end -- or
	# for a cause of its own, in Postgres's words either way.
	stopped_short() {
		outside=$(said 'recovery ended before configured recovery target|recovery stop point is before consistent recovery point' 1)
		[ -z "$outside" ] || failed "$to is outside the archive: $outside"
		failed "the database stopped before it reached $to: $(said 'FATAL|PANIC|recovery' 3)"
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
			|| failed "recovery $paused before it reached $to: $(said 'recovery has paused|lower setting' 2)"
		[ "$waited" -lt "${MERIDIAN_DATABASE_START_SECONDS:-3600}" ] || failed "the archive was still replaying after ${waited}s"
		sleep 2
		waited=$((waited + 2))
	done
	timeline=$(asked 'select timeline_id from pg_control_checkpoint()')
	stop
	say "promoted at $to on timeline $timeline: $(said 'recovery stopping|last completed transaction' 2)"
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
	fail "$act is no act this Job knows: restore-to-time or enable-checksums"
	;;
esac
say "done in $(( $(date +%s) - began ))s"
