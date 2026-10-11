# The image a deployment's scheduled backup runs in (values.yaml's
# backup.image; task kernel/a-deployment-is-backed-up-and-restored).
#
# Tools only, and none of them the runtime's: pg_dump and psql at the major
# version of the database the chart brings, so a dump is made by a client no
# older than its server, and `age`, which seals each backup to the
# administrator's recipient. Encryption stays out of the kernel (the product
# owner, 2026-10-11): no runtime crate links it, and no component but this
# Job's runs it.
#
# What the Job does is the chart's (deploy/chart/files/backup.sh), so a
# change to what a backup holds is a chart release, not a new image.
FROM alpine:3.20
LABEL org.opencontainers.image.source=https://github.com/open-meridian/meridian-core \
      org.opencontainers.image.description="A Meridian deployment's backup: pg_dump and age, nothing else"
RUN apk add --no-cache postgresql16-client age tar coreutils
