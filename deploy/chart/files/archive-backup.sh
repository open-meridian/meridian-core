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
# Each database past the first a store's writer groups write to (step 3d):
# its own server's certificates, in a directory named for its stanza.
for held in /etc/archive-tls-*; do
	[ -d "$held" ] || continue
	stanza=${held#/etc/archive-tls-}
	mkdir -p "/run/archive/tls-$stanza"
	cp "$held/ca.crt" "$held/client.crt" "$held/client.key" "/run/archive/tls-$stanza/"
done
chown -R postgres:postgres /run/archive /tmp/pgbackrest
chmod -R go-rwx /run/archive

as_postgres() { su-exec postgres "$@"; }
# Every stanza the archive holds: meridian, and each database's past the
# first, as the Job's configuration names them.
stanzas=$(sed -n 's/^\[\(.*\)\]$/\1/p' /etc/pgbackrest/pgbackrest.conf | grep -v '^global$')
for stanza in $stanzas; do
# The primary's server answers once the stanza exists; a database just
# chosen, or one restarting under an upgrade, is waited for, two minutes.
# The base backup too, within the same two minutes: a database just
# restored has its stanza moved to it by the archive's own container
# beside it (archive.sh) while this runs, and a backup begun in that moment
# finds no primary (pgBackRest's error 056, CI run 42446040044: `meridian
# backup` straight after `meridian restore`). Asked again until it answers,
# never past the two minutes.
waited=0
until as_postgres pgbackrest "--stanza=$stanza" check >/tmp/pgbackrest/check.log 2>&1 \
	&& as_postgres pgbackrest "--stanza=$stanza" backup --type=full >/tmp/pgbackrest/backup.log 2>&1; do
	if [ "$waited" -ge 120 ]; then
		[ -s /tmp/pgbackrest/backup.log ] \
			&& fail "$stanza: the base backup did not finish within 120s: $(grep -m1 ERROR /tmp/pgbackrest/backup.log || tail -1 /tmp/pgbackrest/backup.log)"
		fail "$stanza: the archive did not answer within 120s: $(grep -m1 ERROR /tmp/pgbackrest/check.log || tail -1 /tmp/pgbackrest/check.log)"
	fi
	[ "$waited" -gt 0 ] || say "$stanza: waiting for the archive to answer and take the base backup"
	sleep 5
	waited=$((waited + 5))
done
as_postgres pgbackrest "--stanza=$stanza" expire >/tmp/pgbackrest/expire.log 2>&1 \
	|| fail "$stanza: the archive's old backups could not be expired: $(grep -m1 ERROR /tmp/pgbackrest/expire.log || tail -1 /tmp/pgbackrest/expire.log)"
# What the archive holds now: its backups, the oldest and newest time it can
# restore to, and its size. Names and times only.
as_postgres pgbackrest "--stanza=$stanza" info --output=json >/tmp/pgbackrest/info.json 2>/dev/null || true
backups=$(grep -o '"label":"[^"]*"' /tmp/pgbackrest/info.json | wc -l | tr -d ' ')
say "$stanza: a full base backup taken; the archive keeps $backups"
done
