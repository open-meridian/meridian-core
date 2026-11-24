#!/bin/sh
# `node-state.sh LINES NODE...`: what Docker says of each k3d node container
# -- running, restarting, its restart count, its last exit code, OOMKilled,
# when it started and finished -- and its log's last LINES lines (none where
# LINES is 0). Said by image-load.sh for a node an import never reached, and
# by the watchdog's evidence for every node of a cluster a failed make left
# (tools/ci-watchdog.sh), v31a item 11: queue run 45395079443's agent-2
# restart-looped and nothing kept said why. Every docker call is bounded at
# 10 s where `timeout` exists. Exit 0 always.
lines=$1
shift
d() {
	if command -v timeout >/dev/null 2>&1; then timeout 10 docker "$@"; else docker "$@"; fi
}
for node in "$@"; do
	state=$(d inspect -f 'running {{.State.Running}}, restarting {{.State.Restarting}}, restarts {{.RestartCount}}, last exit {{.State.ExitCode}}, OOMKilled {{.State.OOMKilled}}, started {{.State.StartedAt}}, finished {{.State.FinishedAt}}' "$node" 2>&1) ||
		state="docker inspect failed or did not answer in 10 s: $state"
	echo "node-state: $node: $state"
	if [ "$lines" -gt 0 ]; then
		echo "node-state: $node, its log's last $lines lines:"
		d logs --timestamps --tail "$lines" "$node" 2>&1 | sed 's/^/    /' ||
			echo "    (docker logs failed or did not answer in 10 s)"
	fi
done
exit 0
