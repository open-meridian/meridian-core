#!/usr/bin/env bash
# A GitHub-hosted runner's disk, freed of what core's jobs never use, before
# a job that builds images.
#
# A standard ubuntu runner could start with 14-20 GB free on /, most of the rest
# held by toolsets preinstalled for other people's builds -- .NET, Android,
# Haskell, CodeQL -- and by images it preloads. ci-local's own CI budgets
# (GATE_BUDGETS_CI in the Makefile) run to 15 GB with a 2 GB floor, so a
# runner as it comes could stop the gate on its first push. Run as the first
# step after checkout; it prints `df -h /` before and after, so each run shows
# the headroom it really had.
#
# Only on a GitHub-hosted runner: never on a developer's machine, and never on
# a self-hosted runner, whose disk is somebody's.

set -euo pipefail

if [ "${GITHUB_ACTIONS:-}" != true ]; then
  echo "free-runner-disk: refused -- this deletes system directories, and runs only on a GitHub-hosted runner" >&2
  exit 1
fi
if [ "${RUNNER_ENVIRONMENT:-}" != github-hosted ]; then
  echo "free-runner-disk: a ${RUNNER_ENVIRONMENT:-unnamed} runner, not GitHub's own; nothing removed"
  df -h /
  exit 0
fi

# Only when it is needed. Since 2026-11 a standard runner starts with about
# 86 GB free of a 145 GB disk, five times the largest stage's budget, and
# freeing took two minutes of every job (ruling 1069): so above
# FREE_RUNNER_DISK_GB free (40 by default: the largest CI budget, its floor
# and the images a job loads, twice over) nothing is removed.
want=${FREE_RUNNER_DISK_GB:-40}
have=$(df -Pk / | awk 'NR == 2 { printf "%d", $4 / 1048576 }')
if [ "$have" -ge "$want" ]; then
  echo "free-runner-disk: $have GB free on /, at least the $want GB a job needs; nothing removed"
  df -h /
  exit 0
fi

echo "free-runner-disk: $have GB free on /, under the $want GB a job needs; before"
df -h /
sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL
docker image prune -af >/dev/null
echo "free-runner-disk: after"
df -h /
