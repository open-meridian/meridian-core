#!/usr/bin/env bash
# The release build's cargo caches, carried from one CI run to the next
# (ruling 556).
#
# The runtime image compiles in Dockerfile's `build` stage against BuildKit
# cache mounts (meridian-core-target, -registry, -git). On a developer's
# machine those outlive a run, so a change compiles only what changed; a
# GitHub-hosted runner starts with none, and compiled every dependency again
# on every run. CI keeps them with actions/cache, as one tarball:
#
#   ci-cargo-cache.sh put  <dir>   <dir>/cargo.tar, if there is one, into the
#                                  cache mounts, before anything builds
#   ci-cargo-cache.sh take <dir>   the cache mounts out to <dir>/cargo.tar,
#                                  after the run
#
# Only the release profile, the registry and git: what the runtime image
# builds from. The debug profile the tests compile is 6.8 GB, past what
# actions/cache keeps well; the release one is 1.9. Put before the gate
# starts, so its budgets measure from the caches in place, as they do on a
# developer's machine; the gate keeps the cache mounts between its stages and
# bounds them by age (GATE_CACHE_DAYS).
#
# The cache mounts are found by their ids alone, so the targets here need not
# be the Dockerfiles'. Each build carries a value of its own, so neither step
# is answered from BuildKit's layer cache.

set -euo pipefail

usage() { echo "usage: $0 put|take <dir>" >&2; exit 2; }
[ $# -eq 2 ] || usage
dir=$2

mounts="--mount=type=cache,id=meridian-core-target,target=/w/target \
--mount=type=cache,id=meridian-core-registry,target=/usr/local/cargo/registry \
--mount=type=cache,id=meridian-core-git,target=/usr/local/cargo/git"
base=debian:bookworm-slim
once=$(date +%s%N)

case "$1" in
  put)
    if [ ! -s "$dir/cargo.tar" ]; then
      echo "ci-cargo-cache: nothing kept from an earlier run; this one compiles from nothing"
      exit 0
    fi
    printf '%s\n' "FROM $base" "ARG ONCE" \
      "RUN --mount=type=bind,target=/kept $mounts echo \"\$ONCE\" >/dev/null && tar -xf /kept/cargo.tar -C /" \
      | DOCKER_BUILDKIT=1 docker build -q --build-arg ONCE="$once" -f - "$dir" >/dev/null
    echo "ci-cargo-cache: put $(du -h "$dir/cargo.tar" | cut -f1) into the cargo caches"
    ;;
  take)
    mkdir -p "$dir"
    empty=$(mktemp -d)
    printf '%s\n' "FROM $base AS take" "ARG ONCE" \
      "RUN $mounts echo \"\$ONCE\" >/dev/null && mkdir /out && mkdir -p /w/target/release && tar -cf /out/cargo.tar -C / w/target/release usr/local/cargo/registry usr/local/cargo/git" \
      "FROM scratch" "COPY --from=take /out/ /" \
      | DOCKER_BUILDKIT=1 docker build -q --build-arg ONCE="$once" -f - --output type=local,dest="$dir" "$empty" >/dev/null
    rmdir "$empty"
    echo "ci-cargo-cache: took $(du -h "$dir/cargo.tar" | cut -f1) from the cargo caches"
    ;;
  *) usage ;;
esac
