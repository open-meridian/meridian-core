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
# Internal TLS (tls.internal; ruling 1084): the chart's certificate copied
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
# TLS; ruled 2026-11-08); in the clear on 127.0.0.1 otherwise.
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
patroni /etc/patroni/patroni.yml &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid"; exit $?' TERM INT
(
  for _ in $(seq 1 300); do
    if api liveness -o /dev/null 2>/dev/null; then
      if api config -o /dev/null -u "patroni:${PATRONI_RESTAPI_PASSWORD}" -X PATCH \
          -H 'Content-Type: application/json' --data "${PATRONI_DYNAMIC}" 2>/dev/null; then
        echo "database.sh: Patroni's dynamic settings held to the chart's"
        # A setting Postgres reads only as it starts -- max_connections,
        # counted from what the chart deploys -- changed by the PATCH
        # leaves this member's server running on the old one, pending a
        # restart nothing else asks for: a primary still at the old count
        # refuses the new pods' connections. Restarted once, in place, on
        # this member alone; members started after the PATCH start on the
        # new one, and a standby takes the primary's where it is higher.
        for _ in $(seq 1 10); do
          sleep 2
          if api patroni 2>/dev/null | grep -q '"pending_restart": *true'; then
            if api restart -o /dev/null -u "patroni:${PATRONI_RESTAPI_PASSWORD}" -X POST \
                -H 'Content-Type: application/json' --data '{"restart_pending": true}' 2>/dev/null; then
              echo "database.sh: restarted for a setting read only at start"
            else
              echo "database.sh: a setting read only at start is pending and the restart was refused" >&2
            fi
            break
          fi
        done
        exit 0
      fi
    fi
    sleep 2
  done
  echo "database.sh: Patroni's dynamic settings could not be set; it keeps its own" >&2
) &
wait "$pid"
