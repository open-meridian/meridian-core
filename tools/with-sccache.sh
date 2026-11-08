#!/bin/sh
# A command run with sccache in front of rustc, where the build was given
# GitHub's cache token (ruling 1104; step 3b part 4); as it is otherwise.
#
# The token, and the cache service's addresses beside it, arrive only as
# BuildKit secrets (`docker build --secret id=gha-runtime-token,...`, the
# Makefile's SCCACHE_SECRETS, which CI's compile job sets): mounted for this
# one RUN at /run/secrets, read into this process's environment, never
# written to a file or a layer, never printed. A build without them -- every
# laptop's, and any run that cannot reach the cache -- runs the command
# unwrapped, so the cache can make a build faster and never stop one.
set -eu
secret() { [ -s "/run/secrets/$1" ] && cat "/run/secrets/$1" || true; }
token=$(secret gha-runtime-token)
if [ -z "$token" ]; then
	exec "$@"
fi
ACTIONS_RUNTIME_TOKEN=$token
ACTIONS_RESULTS_URL=$(secret gha-results-url)
ACTIONS_CACHE_URL=$(secret gha-cache-url)
export ACTIONS_RUNTIME_TOKEN ACTIONS_RESULTS_URL ACTIONS_CACHE_URL
export SCCACHE_GHA_ENABLED=true RUSTC_WRAPPER=sccache
# A cache entry is the rustc invocation's; the toolchain is in it already.
export SCCACHE_GHA_VERSION="meridian-core-1"
# Unreachable, it compiles as if there were none.
export SCCACHE_IGNORE_SERVER_IO_ERROR=1
sccache --start-server >/dev/null 2>&1 || { unset RUSTC_WRAPPER; exec "$@"; }
status=0
"$@" || status=$?
sccache --show-stats 2>/dev/null | sed -n '1,12p' >&2 || true
sccache --stop-server >/dev/null 2>&1 || true
exit $status
