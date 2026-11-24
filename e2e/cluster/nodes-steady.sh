#!/bin/sh
# `nodes-steady.sh CLUSTER STATE`: whether every node of the k3d cluster
# CLUSTER is Ready and none of its node containers is restarting, asked by
# each turn of the Makefile's `ready` wait (e2e-cluster-k3d), so the cluster
# counts as ready only then (v31a item 11).
#
# Queue run 45395079443's failover job (ha, three agents) had agent-2's
# container restart-looping from its first start, every 15 to 20 s, while
# every ready wait passed and every proof was green: the cluster was called
# ready with a node coming and going under it.
#
# A node is steady where its container is running, not restarting, and its
# Docker RestartCount has not changed for STEADY seconds (E2E_K3D_STEADY_SECONDS,
# default 30, longer than that loop's cycle) by this machine's clock. STATE, a
# file the Makefile makes for the bring-up, keeps when the counts were first
# seen as they are and what they were: the Makefile asks once as the cluster
# is made, so on a cluster that never restarted the time has passed by the
# last wait. A count that changes, or a container not running, starts it
# again. Each node's Ready condition is asked now. Exit 0 when all of it
# holds; else 1, saying why on stdout (the `ready` wait shows it where it
# gives up). Every kubectl call is bounded at 10 s; KUBECONFIG is the
# cluster's, as the waits have it.
set -u
cluster=$1 state=$2
steady=${E2E_K3D_STEADY_SECONDS:-30}

k() {
	if command -v timeout >/dev/null 2>&1; then
		timeout 10 kubectl --request-timeout=10s "$@"
	else
		kubectl --request-timeout=10s "$@"
	fi
}

nodes=$(docker ps -a --filter "label=k3d.cluster=$cluster" --format '{{.Names}}' 2>/dev/null |
	grep -E "^k3d-${cluster}-(server|agent)-[0-9]+$" | sort)
if [ -z "$nodes" ]; then
	echo "nodes-steady: no node container of k3d cluster $cluster"
	: >"$state"
	exit 1
fi

now=$(date +%s)
counts="" down=""
for node in $nodes; do
	set -- $(docker inspect -f '{{.State.Running}} {{.State.Restarting}} {{.RestartCount}} {{.State.ExitCode}} {{.State.OOMKilled}}' "$node" 2>/dev/null)
	if [ "$#" -lt 5 ]; then
		down="$down $node (docker inspect did not answer)"
		continue
	fi
	counts="$counts $node=$3"
	if [ "$1" != true ] || [ "$2" != false ]; then
		down="$down $node (running $1, restarting $2, restarts $3, last exit $4, OOMKilled $5)"
	fi
done
if [ -n "$down" ]; then
	echo "nodes-steady: a node container is not running:$down"
	: >"$state"
	exit 1
fi

since=$(sed -n 1p "$state" 2>/dev/null)
before=$(sed -n 2p "$state" 2>/dev/null)
if [ -z "$since" ] || [ "$before" != "$counts" ]; then
	if [ -n "$before" ]; then
		echo "nodes-steady: a node container restarted: its restart counts were$before, now$counts"
	fi
	since=$now
	printf '%s\n%s\n' "$since" "$counts" >"$state"
	[ -z "$before" ] || exit 1
fi

ready=$(k get nodes -o jsonpath='{range .items[*]}{.metadata.name}{" "}{range .status.conditions[?(@.type=="Ready")]}{.status}{end}{"\n"}{end}' 2>/dev/null) || {
	echo "nodes-steady: the cluster's nodes could not be read"
	exit 1
}
unready=""
for node in $nodes; do
	said=$(printf '%s\n' "$ready" | awk -v n="$node" '$1 == n { print $2 }')
	[ "$said" = True ] || unready="$unready $node (Ready ${said:-not listed})"
done
if [ -n "$unready" ]; then
	echo "nodes-steady: a node is not Ready:$unready"
	exit 1
fi

if [ $((now - since)) -lt "$steady" ]; then
	echo "nodes-steady: every node is Ready and running; their restart counts ($counts ) have held for $((now - since))s of ${steady}s"
	exit 1
fi
echo "nodes-steady: every node is Ready, running, and has not restarted for $((now - since))s:$counts"
