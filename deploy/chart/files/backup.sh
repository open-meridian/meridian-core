#!/bin/sh
# One backup of this deployment, sealed to the administrator's recipient
# (task kernel/a-deployment-is-backed-up-and-restored; templates/backup.yaml
# runs it in backup.image, which holds pg_dump, psql and age and nothing of
# the runtime).
#
# What it holds, as one tar sealed with age:
#
#   manifest.json        what was taken, from which release, and which objects
#   checksums.sha256     every other member's SHA-256, which restore checks
#   values/values.yaml   the release's values, its enrolment code removed
#   secrets/<name>/<key> each kept Secret and each the values name, as mounted
#   configmaps/<name>/<key>  the dashboard's public keys
#   key/key.pem          the deployment's key, where the conductor made it
#   database/database.dump   pg_dump --format=custom, when the database is included
#   database/counts.tsv  every table's row count, in the dump's own snapshot
#
# Never prints a value: only names, counts, sizes and the sealed file's hash.
set -eu
set -o pipefail

say() { echo "backup: $*"; }
fail() { echo "backup: FAILED: $*" >&2; exit 1; }

release=${MERIDIAN_BACKUP_RELEASE:?}
recipient=${MERIDIAN_BACKUP_RECIPIENT:-}
keep=${MERIDIAN_BACKUP_KEEP:-14}
[ -n "$recipient" ] || fail "no recipient to seal to; set backup.recipient"
case "$keep" in ''|*[!0-9]*) fail "backup.keep is not a whole number: $keep" ;; esac
[ "$keep" -ge 1 ] || fail "backup.keep is below 1, which would delete the backup just written"
[ -d /backups ] && [ -w /backups ] || fail "/backups is not a directory this Job can write"

umask 077
work=/work/backup
rm -rf "$work"
mkdir -p "$work"
# Plain text, keys included, lives here until it is sealed: removed on any exit.
trap 'rm -rf "$work"' EXIT
taken=$(date -u +%Y%m%dT%H%M%SZ)
name="$release-$taken.meridian-backup"
# What an earlier run left half-written; one run at a time (concurrencyPolicy).
rm -f /backups/."$release"-*.partial

# Each object, key by key, as Kubernetes mounted it. A volume of a Secret that
# does not exist is mounted empty (optional), and is left out.
held_chart=""
held_operator=""
held_maps=""
copy() { # <secrets|configmaps> <name>; says whether it held anything
	from="/in/$1/$2"
	[ -d "$from" ] || return 1
	found=1
	for file in "$from"/* "$from"/.[!.]*; do
		[ -f "$file" ] || continue
		mkdir -p "$work/$1/$2"
		cp -L "$file" "$work/$1/$2/"
		found=0
	done
	return $found
}
for secret in ${MERIDIAN_BACKUP_CHART_SECRETS:-}; do
	copy secrets "$secret" && held_chart="$held_chart $secret"
done
for secret in ${MERIDIAN_BACKUP_OPERATOR_SECRETS:-}; do
	copy secrets "$secret" && held_operator="$held_operator $secret"
done
for map in ${MERIDIAN_BACKUP_CONFIG_MAPS:-}; do
	copy configmaps "$map" && held_maps="$held_maps $map"
done
say "secrets:$held_chart$held_operator"

mkdir -p "$work/values"
cp /in/values/values.yaml "$work/values/"

key=${MERIDIAN_BACKUP_KEY:-secret}
if [ "$key" = generated ]; then
	[ -s /in/key/key.pem ] || fail "the deployment's key is not in its volume yet"
	mkdir -p "$work/key"
	cp /in/key/key.pem "$work/key/key.pem"
	say "the deployment's key, from its volume"
fi

# The database: the one first run made, every store's tables in it.
included=false
brought=false
database=""
prune=true
url_file="/in/secrets/${MERIDIAN_BACKUP_DATABASE_SECRET:-}/url"
if [ ! -s "$url_file" ]; then
	say "no database yet (first run has not written one, or a restore is withholding it): the keys and values alone"
	# Not counted against backup.keep: it must never rotate out a backup
	# that holds a database, as one taken while a restore runs would.
	prune=false
else
	rest=$(sed -e 's|^[a-z]*://||' -e 's|^[^@/]*@||' "$url_file")
	hostport=${rest%%/*}
	host=${hostport%%:*}
	port=${hostport#*:}
	[ "$port" != "$hostport" ] || port=5432
	database=${rest#*/}
	database=${database%%\?*}
	there=${MERIDIAN_BACKUP_BROUGHT_HOST:-}
	if [ -n "$there" ]; then
		case "$host" in "$there" | "$there".*) brought=true ;; esac
	fi
	# backup.database: empty (auto) includes the database this chart brings
	# and not one somebody else runs, whose own backups and availability are
	# its provider's; true or false says so for either (the product owner,
	# 2026-10-11).
	case "${MERIDIAN_BACKUP_DATABASE:-auto}" in
	auto) included=$brought ;;
	true) included=true ;;
	*) included=false ;;
	esac
fi

if [ "$included" = true ]; then
	mkdir -p "$work/database"
	export PGCONNECT_TIMEOUT=10
	if [ "$brought" = true ]; then
		# As the superuser the chart made, which reads every role's tables.
		export PGHOST="$host" PGPORT="$port" PGUSER=postgres PGDATABASE="$database"
		PGPASSWORD=$(cat "/in/secrets/${MERIDIAN_BACKUP_BROUGHT_PASSWORDS:?}/superuser")
		export PGPASSWORD
		target="$database"
	else
		# As the migrating role, which owns the tables.
		target=$(cat "/in/secrets/$MERIDIAN_BACKUP_DATABASE_SECRET/migrate-url" 2>/dev/null || cat "$url_file")
	fi
	# Waited for, every two seconds for at most two minutes, before the dump's
	# one connection. A pod's first connections can be refused while the
	# cluster's network policy has not yet admitted its address: on k3s every
	# scheduled backup failed so ("Connection refused", within a second of
	# starting), while a pod a second older reached the same database. And a
	# database restarting under an upgrade answers again within the wait.
	waited=0
	until pg_isready -q -t 5 -d "$target"; do
		[ "$waited" -lt 120 ] || fail "the database $database did not answer within 120s"
		[ "$waited" -gt 0 ] || say "waiting for the database $database to answer"
		sleep 2
		waited=$((waited + 2))
	done
	export MERIDIAN_DUMP="$work/database/database.dump" MERIDIAN_TARGET="$target"
	# The dump and the counts in one snapshot, exported by the transaction the
	# counts are read in, so what restore checks is what the dump holds.
	psql -X -q -v ON_ERROR_STOP=1 -d "$target" <<'SQL'
begin isolation level repeatable read read only;
select pg_export_snapshot() as snapshot \gset
\setenv MERIDIAN_SNAPSHOT :snapshot
\! pg_dump --format=custom --no-password --snapshot="$MERIDIAN_SNAPSHOT" --file="$MERIDIAN_DUMP" -d "$MERIDIAN_TARGET" || touch "$MERIDIAN_DUMP.failed"
\pset format unaligned
\pset tuples_only on
\pset fieldsep '\t'
\o /work/backup/database/counts.tsv
select n.nspname || '.' || c.relname,
       (xpath('/row/n/text()', query_to_xml(format('select count(*) as n from %I.%I', n.nspname, c.relname), false, true, '')))[1]::text
  from pg_class c join pg_namespace n on n.oid = c.relnamespace
 where c.relkind in ('r', 'p')
   and n.nspname not in ('pg_catalog', 'information_schema')
   and n.nspname not like 'pg\_toast%'
 order by n.nspname || '.' || c.relname collate "C";
\o
commit;
SQL
	[ ! -e "$MERIDIAN_DUMP.failed" ] || fail "pg_dump did not finish"
	[ -s "$MERIDIAN_DUMP" ] || fail "pg_dump wrote nothing"
	unset PGPASSWORD
	rows=$(awk -F'\t' '{ n += $2 } END { print n + 0 }' "$work/database/counts.tsv")
	say "the database $database: $(wc -l <"$work/database/counts.tsv" | tr -d ' ') tables, $rows rows, dump $(wc -c <"$MERIDIAN_DUMP" | tr -d ' ') bytes"
elif [ -n "$database" ]; then
	if [ "$brought" = true ]; then
		say "the database $database is not included (backup.database is false)"
	else
		say "the database $database is not included: one this chart does not bring is its provider's to back up, unless backup.database is true"
	fi
fi

list() { # space-separated names as a JSON array
	out=""
	for item in $1; do out="$out${out:+, }\"$item\""; done
	printf '[%s]' "$out"
}
cat >"$work/manifest.json" <<EOF
{
  "format": 1,
  "release": "$release",
  "namespace": "${MERIDIAN_BACKUP_NAMESPACE:-}",
  "chart_version": "${MERIDIAN_BACKUP_CHART_VERSION:-}",
  "app_version": "${MERIDIAN_BACKUP_APP_VERSION:-}",
  "deployment_id": "${MERIDIAN_BACKUP_DEPLOYMENT_ID:-}",
  "taken_at": "$taken",
  "key": "$key",
  "database": { "included": $included, "brought": $brought, "name": "$database", "secret": "${MERIDIAN_BACKUP_DATABASE_SECRET:-}" },
  "chart_secrets": $(list "$held_chart"),
  "operator_secrets": $(list "$held_operator"),
  "config_maps": $(list "$held_maps")
}
EOF
(cd "$work" && find . -type f ! -name checksums.sha256 | LC_ALL=C sort | sed 's|^\./||' | xargs sha256sum) >"$work/checksums.sha256"

partial="/backups/.$name.partial"
tar -C "$work" -cf - . | age -r "$recipient" -o "$partial" || { rm -f "$partial"; fail "sealing did not finish"; }
[ "$(head -c 21 "$partial")" = "age-encryption.org/v1" ] || { rm -f "$partial"; fail "what was written is not an age file"; }
mv "$partial" "/backups/$name"
rm -rf "$work"
say "wrote $name $(wc -c <"/backups/$name" | tr -d ' ') bytes sha256 $(sha256sum "/backups/$name" | cut -d' ' -f1)"

# The newest `keep` of this release's, once this one is written; never on a
# failure, which exits above.
ls -1 /backups | grep -E "^$release-[0-9]{8}T[0-9]{6}Z\.meridian-backup$" | LC_ALL=C sort >/tmp/held || true
excess=$(($(wc -l </tmp/held) - keep))
if [ "$prune" = true ] && [ "$excess" -gt 0 ]; then
	head -n "$excess" /tmp/held | while read -r old; do
		rm -f "/backups/$old"
		say "removed $old, older than the newest $keep"
	done
fi

# `meridian backup --to` copies the file out of this pod before it goes.
if [ -n "${MERIDIAN_BACKUP_HOLD:-}" ]; then
	say "holding $name for copying"
	waited=0
	while [ ! -e /tmp/copied ] && [ "$waited" -lt 600 ]; do
		sleep 1
		waited=$((waited + 1))
	done
fi
