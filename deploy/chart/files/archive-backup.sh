#!/bin/sh
# A full base backup of the brought database into its WAL archive
# (values.yaml's backup.walArchive), taken by the backup Job before it seals
# its file, through the primary's pgBackRest server over TLS; then the
# backups beyond backup.keep expired with the WAL only they needed. Skipped,
# and said, where first run has not written the database's URL yet: there
# is no database to back up. Never prints the passphrase or a credential.
set -eu
say() { echo "archive: $*"; }
fail() { echo "archive: FAILED: $*" >&2; exit 1; }

if [ ! -s "/in/database/${MERIDIAN_ARCHIVE_URL_KEY:-url}" ]; then
	say "no database yet (first run has not written one, or a restore is withholding it): no base backup"
	exit 0
fi

mkdir -p /run/archive/tls /tmp/pgbackrest
cp /etc/archive-tls/ca.crt /etc/archive-tls/client.crt /etc/archive-tls/client.key /run/archive/tls/
chown -R postgres:postgres /run/archive /tmp/pgbackrest
chmod 0700 /run/archive/tls
chmod 0600 /run/archive/tls/*

as_postgres() { su-exec postgres "$@"; }
# The primary's server answers once the stanza exists; a database just
# chosen, or one restarting under an upgrade, is waited for, two minutes.
waited=0
until as_postgres pgbackrest --stanza=meridian check >/tmp/pgbackrest/check.log 2>&1; do
	[ "$waited" -lt 120 ] || fail "the archive did not answer within 120s: $(grep -m1 ERROR /tmp/pgbackrest/check.log || tail -1 /tmp/pgbackrest/check.log)"
	[ "$waited" -gt 0 ] || say "waiting for the archive to answer"
	sleep 5
	waited=$((waited + 5))
done
as_postgres pgbackrest --stanza=meridian backup --type=full >/tmp/pgbackrest/backup.log 2>&1 \
	|| fail "the base backup did not finish: $(grep -m1 ERROR /tmp/pgbackrest/backup.log || tail -1 /tmp/pgbackrest/backup.log)"
as_postgres pgbackrest --stanza=meridian expire >/tmp/pgbackrest/expire.log 2>&1 \
	|| fail "the archive's old backups could not be expired: $(grep -m1 ERROR /tmp/pgbackrest/expire.log || tail -1 /tmp/pgbackrest/expire.log)"
# What the archive holds now: its backups, the oldest and newest time it can
# restore to, and its size. Names and times only.
as_postgres pgbackrest --stanza=meridian info --output=json >/tmp/pgbackrest/info.json 2>/dev/null || true
backups=$(grep -o '"label":"[^"]*"' /tmp/pgbackrest/info.json | wc -l | tr -d ' ')
say "a full base backup taken; the archive keeps $backups"
