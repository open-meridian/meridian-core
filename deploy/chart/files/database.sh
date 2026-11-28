#!/bin/sh
# The brought database under Patroni (step 3b; deploy/chart/templates/
# database.yaml). Runs Patroni on the static half of its configuration, and
# once its REST API answers, holds the dynamic half -- synchronous or not,
# the timings -- to the chart's (PATRONI_DYNAMIC), which Patroni otherwise
# keeps from the day the cluster began. Every member does it; it is the same
# answer each time. Never prints a password.
set -eu
# Started as root, as the image's own entrypoint is: the data directory made
# the image's postgres user's, as that entrypoint makes it, and Patroni run
# as that user, never as root. A cluster that runs pods as its own user
# (podSecurityContext) starts here as that user instead.
# Internal TLS (tls.internal): the chart's certificate copied
# where the server's user alone reads its key, as Postgres requires of one.
if [ -d /etc/meridian-internal-tls-server ] && [ ! -e /run/meridian-tls/tls.key ]; then
  mkdir -p /run/meridian-tls
  cp /etc/meridian-internal-tls-server/ca.crt /etc/meridian-internal-tls-server/tls.crt \
    /etc/meridian-internal-tls-server/tls.key /run/meridian-tls/
  chmod 0700 /run/meridian-tls
  chmod 0600 /run/meridian-tls/*
  if [ "$(id -u)" = 0 ]; then chown -R postgres:postgres /run/meridian-tls; fi
fi
if [ "$(id -u)" = 0 ]; then
  mkdir -p "$PGDATA"
  chown postgres:postgres /var/lib/postgresql/data "$PGDATA"
  chmod 0700 "$PGDATA"
  exec su-exec postgres /bin/sh "$0"
fi
umask 0077
# Patroni's REST API on this member: over TLS by the member's own name,
# verified against the internal CA, where the chart serves it so (internal
# TLS); in the clear on 127.0.0.1 otherwise.
api() {
  path=$1
  shift
  if [ -n "${PATRONI_RESTAPI_CERTFILE:-}" ]; then
    host=${PATRONI_RESTAPI_CONNECT_ADDRESS%:*}
    curl -fsS --cacert /run/meridian-tls/ca.crt --resolve "${host}:8008:127.0.0.1" "$@" \
      "https://${host}:8008/${path}"
  else
    curl -fsS "$@" "http://127.0.0.1:8008/${path}"
  fi
}
# A record-read replica (contract v30, stage C): its tags -- nosync,
# nofailover -- laid over the static half, for this member alone.
config=/etc/patroni/patroni.yml
if [ -f "/etc/patroni/read-replica-${POD_NAME}" ]; then
  cat /etc/patroni/patroni.yml "/etc/patroni/read-replica-${POD_NAME}" > /tmp/patroni.yml
  config=/tmp/patroni.yml
fi
patroni "$config" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid"; exit $?' TERM INT
(
  for _ in $(seq 1 300); do
    if api liveness -o /dev/null 2>/dev/null; then
      if api config -o /dev/null -u "patroni:${PATRONI_RESTAPI_PASSWORD}" -X PATCH \
          -H 'Content-Type: application/json' --data "${PATRONI_DYNAMIC}" 2>/dev/null; then
        echo "database.sh: Patroni's dynamic settings held to the chart's"
        # A setting Postgres reads only as it starts -- max_connections,
        # counted from what the chart deploys, which raising a count
        # (writer groups, members) raises -- changed by the PATCH leaves
        # servers running on the old one, pending a restart nothing else
        # asks for: a primary still at the old count refuses the new pods'
        # connections, and every migration waits for a server allowing the
        # new one (PR CI 43648472142, ha: 220 of 303 from the writer
        # groups' upgrade on, the roll having made the first member it
        # restarted the leader before Patroni said its restart was
        # pending). So the cluster applies it, as an upgrade's own work,
        # for as long as this member runs: a standby restarts, one at a
        # time -- every other member running, the lowest name first -- and
        # the leader last, by a switchover to a standby already restarted,
        # then restarts as a standby itself; a leader with no standby to
        # hand over to restarts in place. Each member decides for itself
        # from the cluster's own view (`decide`), so no two act at once.
        #
        # Whether a restart is pending here is asked of Postgres itself as
        # well as of Patroni (PR CI 43669650813, ha: the member the roll
        # restarted first took the PATCH while it was still starting,
        # "restarting after failure in progress", and became the leader;
        # Postgres said "max_connections cannot be changed without
        # restarting the server" at each reload from then on, and this loop
        # did nothing for the twelve minutes the run went on -- the
        # migrations gave up waiting for 303 again, so no P1 head and no
        # database 1). And a member that waits says why, once for each
        # reason, where it said nothing.
        decide='
import json, sys
me = sys.argv[1]
members = json.load(sys.stdin).get("members", [])
here = len(sys.argv) > 2 and sys.argv[2] not in ("", "0")
mine = next((m for m in members if m.get("name") == me), None)
if not mine or not (mine.get("pending_restart") or here):
    print("none"); sys.exit()
# Asked of Patroni where it says so itself; where only Postgres does, a
# restart Patroni is asked for whatever it holds pending.
restart = "restart" if mine.get("pending_restart") else "restart-whatever"
switchover = "switchover" if mine.get("pending_restart") else "switchover-whatever"
others = [m for m in members if m.get("name") != me]
def healthy(m):
    return m.get("state") in ("running", "streaming")
def named(some):
    return ", ".join("%s (%s)" % (m.get("name"), m.get("state")) for m in some)
if mine.get("role") in ("leader", "standby_leader"):
    standbys = [m for m in others if m.get("role") not in ("leader", "standby_leader")]
    pending = [m for m in standbys if m.get("pending_restart")]
    if pending:
        print("wait: the leader, after each standby still pending its own: " + named(pending))
    elif any(healthy(m) and not (m.get("tags") or {}).get("nofailover") for m in standbys):
        print(switchover)
    elif not any(not (m.get("tags") or {}).get("nofailover") for m in standbys):
        print(restart)
    else:
        print("wait: the leader, with no standby running to hand over to: " + named(standbys))
elif not all(healthy(m) for m in others):
    print("wait: a standby, until every other member runs: "
          + named([m for m in others if not healthy(m)]))
elif any(m.get("pending_restart") and m.get("role") not in ("leader", "standby_leader")
         and m.get("name", "") < me for m in others):
    print("wait: a standby, after the standbys named before it")
else:
    print(restart)
'
        # Where only Postgres says so, once for as long as this member
        # runs: a setting it can never take (one lowered below its
        # primary's) stays pending whatever is restarted, and is never
        # restarted for, or handed over for, again and again.
        waited=""
        whatever=""
        while sleep 10; do
          # As this member's own server says it: the settings it holds
          # pending a restart, none where it does not answer.
          here=$(psql -X -At -h /var/run/postgresql -U postgres -d postgres \
            -c 'select count(*) from pg_settings where pending_restart' 2>/dev/null || echo 0)
          said=$(api cluster 2>/dev/null | python3 -c "$decide" "${PATRONI_NAME}" "${here:-0}" 2>/dev/null) || continue
          case "$said" in
          *-whatever)
            if [ -n "$whatever" ]; then continue; fi
            ;;
          esac
          case "$said" in
          wait*)
            if [ "$said" != "$waited" ]; then
              echo "database.sh: a setting read only at start is pending here; ${said}"
              waited=$said
            fi
            ;;
          restart|restart-whatever)
            pending='{"restart_pending": true}'
            if [ "$said" = restart-whatever ]; then pending='{}'; fi
            if api restart -o /dev/null -u "patroni:${PATRONI_RESTAPI_PASSWORD}" -X POST \
                -H 'Content-Type: application/json' --data "$pending" 2>/dev/null; then
              echo "database.sh: restarted for a setting read only at start"
              if [ "$said" = restart-whatever ]; then whatever=asked; fi
            else
              echo "database.sh: a setting read only at start is pending and the restart was refused; asked again" >&2
            fi
            ;;
          switchover|switchover-whatever)
            if api switchover -o /dev/null -u "patroni:${PATRONI_RESTAPI_PASSWORD}" -X POST \
                -H 'Content-Type: application/json' --data "{\"leader\": \"${PATRONI_NAME}\"}" 2>/dev/null; then
              echo "database.sh: handed the leader to a standby already restarted, to restart for a setting read only at start"
              if [ "$said" = switchover-whatever ]; then whatever=asked; fi
            else
              echo "database.sh: a setting read only at start is pending and the switchover was refused; asked again" >&2
            fi
            ;;
          esac
        done
        exit 0
      fi
    fi
    sleep 2
  done
  echo "database.sh: Patroni's dynamic settings could not be set; it keeps its own" >&2
) &
wait "$pid"
