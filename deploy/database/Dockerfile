# The brought database's image when it runs as more than one member, or
# archives its WAL (values.yaml's database.brought; step 3b, spec/a-
# deployment-scales-out-and-fails-over-by-configuration section 4).
#
# The Postgres the chart has always brought, postgres:16-alpine -- so a data
# directory made by it is the same server's, its collation the same musl's --
# with Patroni, which runs it as a primary or a standby and fails it over with
# Kubernetes objects as its store (namespaced ConfigMaps and pod labels, no
# CRD: decisions/016), and pgBackRest, which archives its WAL off the cluster
# and restores it to a point in time, checking page checksums as it copies.
# Nothing of the runtime's.
#
# What the database does is the chart's (deploy/chart/files/database.sh), so
# a change to how it runs is a chart release, not a new image.
ARG POSTGRES=postgres:16-alpine
FROM ${POSTGRES}
LABEL org.opencontainers.image.source=https://github.com/open-meridian/meridian-core \
      org.opencontainers.image.description="A Meridian deployment's brought Postgres: Postgres 16, Patroni and pgBackRest"
ARG PATRONI=4.0.6
ARG YDIFF=1.4.2
RUN apk add --no-cache python3 py3-psycopg2 py3-yaml py3-urllib3 py3-dateutil py3-psutil \
        py3-click py3-prettytable pgbackrest curl \
 && apk add --no-cache --virtual .pip py3-pip \
 && pip install --no-cache-dir --break-system-packages --no-deps "patroni==${PATRONI}" "ydiff==${YDIFF}" \
 && apk del .pip \
 && patroni --version && pgbackrest version
