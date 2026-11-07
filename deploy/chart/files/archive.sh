#!/bin/sh
# The WAL archive beside a member of the brought database (values.yaml's
# backup.walArchive; templates/database.yaml's `archive` container). The
# database's own container archives each segment (archive_command); this
# one serves pgBackRest's protocol over TLS, through which the backup Job
# takes base backups, and makes the archive's stanza once this member is
# primary -- or moves it to the database a restore made again, which has a
# new system identifier (stanza-upgrade keeps the old one's history).
# Never prints the passphrase or a credential.
set -eu
say() { echo "archive: $*"; }

# The certificates, readable by the database's user alone, as pgBackRest
# requires of a key: a Secret's volume makes them root's.
mkdir -p /run/archive/tls /tmp/pgbackrest
cp /etc/archive-tls/ca.crt /etc/archive-tls/tls.crt /etc/archive-tls/tls.key /run/archive/tls/
chown -R postgres:postgres /run/archive /tmp/pgbackrest
chmod 0700 /run/archive/tls
chmod 0600 /run/archive/tls/*

# Beside the sealed backups: the directory made, the database's user's.
if [ -n "${MERIDIAN_ARCHIVE_DIRECTORY:-}" ]; then
	mkdir -p "$MERIDIAN_ARCHIVE_DIRECTORY"
	chown postgres:postgres "$MERIDIAN_ARCHIVE_DIRECTORY"
	chmod 0700 "$MERIDIAN_ARCHIVE_DIRECTORY"
fi

as_postgres() { su-exec postgres "$@"; }

# The stanza, once the database answers as a primary; a standby asks every
# ten seconds, and makes it the moment it is promoted. Until it exists the
# database keeps each segment it could not archive, and archives it then.
(
	said=""
	while :; do
		primary=$(as_postgres psql -X -At -h /var/run/postgresql -U postgres -d postgres \
			-c 'select not pg_is_in_recovery()' 2>/dev/null || true)
		if [ "$primary" = t ]; then
			if as_postgres pgbackrest --stanza=meridian stanza-create >/tmp/pgbackrest/stanza.log 2>&1; then
				say "the stanza holds this database"
				break
			fi
			# "backup and archive info files exist but do not match the
			# database" (pgBackRest's 028): a database made again.
			if grep -q '\[028\]' /tmp/pgbackrest/stanza.log \
				&& as_postgres pgbackrest --stanza=meridian stanza-upgrade >>/tmp/pgbackrest/stanza.log 2>&1; then
				say "the stanza moved to this database, made again; the one before it is kept as history"
				break
			fi
			now=$(grep -m1 -E 'ERROR|WARN' /tmp/pgbackrest/stanza.log || tail -1 /tmp/pgbackrest/stanza.log)
			[ "$now" = "$said" ] || say "the stanza could not be made yet: $now"
			said=$now
		fi
		sleep 10
	done
) &

# The server, started again if it ever stops: this container's readiness is
# the database pod's, and the database must never be unreachable because
# its archive's server is (a failed base backup says so instead).
trap 'kill -TERM "$server" 2>/dev/null; exit 0' TERM INT
while :; do
	su-exec postgres pgbackrest server &
	server=$!
	wait "$server" && status=0 || status=$?
	say "pgBackRest's server stopped (exit $status); started again in 10s"
	sleep 10
done
