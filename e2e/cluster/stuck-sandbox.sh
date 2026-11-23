#!/bin/sh
# `stuck-sandbox.sh SEEN WHAT START`: whether a kube-system pod's sandbox is
# stuck, asked once by each turn of the Makefile's `ready` waits
# (e2e-cluster-k3d), so a wait gives up in about a minute rather than at
# E2E_K3D_READY_SECONDS.
#
# About 1% of cluster jobs from 2026-11-17 left one pod in ContainerCreating
# on a node -- traefik, sometimes svclb, the last sandboxes k3s makes there,
# just after its helm-install pods finish -- until the wait gave up at 300 s;
# the runner's `ip`, Docker and uploads then hung to the job's limit, and
# nothing said why. A sandbox starts in seconds, so a pod that is scheduled,
# still Pending, has a container waiting in ContainerCreating and its sandbox
# not made (its PodReadyToStartContainers condition not True, which leaves
# out a pod pulling an image) for more than 60 s is stuck. This says which
# pod, on which node and for how long; then `kubectl describe pod` for each
# one stuck (three at most), kube-system's events by time and every pod in
# the cluster; and exits 1.
# Exit 0: none is stuck, or the cluster could not be asked.
#
# How long is measured by this machine's clock from when a wait first saw the
# pod so, kept in SEEN (a file the Makefile makes for the bring-up), and never
# by the cluster's timestamps, which a laptop's VM may keep by another clock.
# The cluster's own time it was scheduled is said beside it. Every kubectl
# call is bounded at 10 s. KUBECONFIG is the cluster's, as the waits have it.
set -u
seen=$1 what=$2 start=$3
limit=60

k() {
	if command -v timeout >/dev/null 2>&1; then
		timeout 10 kubectl --request-timeout=10s "$@"
	else
		kubectl --request-timeout=10s "$@"
	fi
}

# One line a candidate: name, node, when it was scheduled. A pod is one where
# it is Pending, scheduled, its sandbox not made, and a container of its
# waiting in ContainerCreating.
pods=$(k -n kube-system get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{.spec.nodeName}{"\t"}{.status.phase}{"\t"}{range .status.conditions[?(@.type=="PodScheduled")]}{.status}{"@"}{.lastTransitionTime}{end}{"\t"}{range .status.conditions[?(@.type=="PodReadyToStartContainers")]}{.status}{end}{"\t"}{range .status.containerStatuses[*]}{.state.waiting.reason}{" "}{end}{"\n"}{end}' 2>/dev/null) || exit 0
now=$(date +%s)
candidates=$(printf '%s\n' "$pods" | awk -F '\t' '
	$3 == "Pending" && $4 ~ /^True@/ && $5 != "True" && (" " $6 " ") ~ / ContainerCreating / {
		sub(/^True@/, "", $4); print $1 "\t" $2 "\t" $4
	}')

# SEEN keeps "name<TAB>first seen" for the candidates of the last look only,
# so a pod that leaves ContainerCreating and comes back starts again.
next=$(mktemp)
stuck=""
if [ -n "$candidates" ]; then
	while IFS='	' read -r name node scheduled; do
		first=$(awk -F '\t' -v n="$name" '$1 == n { print $2; exit }' "$seen" 2>/dev/null)
		[ -n "$first" ] || first=$now
		printf '%s\t%s\n' "$name" "$first" >>"$next"
		waited=$((now - first))
		if [ "$waited" -gt "$limit" ]; then
			stuck="$stuck$name	$node	$scheduled	$waited
"
		fi
	done <<EOF
$candidates
EOF
fi
mv "$next" "$seen"
[ -n "$stuck" ] || exit 0

first_line=$(printf '%s' "$stuck" | head -n 1)
name=$(printf '%s' "$first_line" | cut -f1)
node=$(printf '%s' "$first_line" | cut -f2)
waited=$(printf '%s' "$first_line" | cut -f4)
echo "e2e-cluster-k3d: gave up on $what after $((now - start))s: kube-system/$name has been in ContainerCreating for ${waited}s on ${node:-no node}, its sandbox not made (a sandbox starts in seconds; this gives up at ${limit}s)"
printf '%s' "$stuck" | while IFS='	' read -r name node scheduled waited; do
	echo "e2e-cluster-k3d: stuck: kube-system/$name on ${node:-no node}, in ContainerCreating without its sandbox for ${waited}s, scheduled at $scheduled by the cluster's clock"
done
printf '%s' "$stuck" | head -n 3 | while IFS='	' read -r name node scheduled waited; do
	echo "-- kubectl describe pod kube-system/$name"
	k -n kube-system describe pod "$name" 2>&1 || echo "(kubectl failed, or did not answer in 10 s)"
done
echo "-- kube-system's events, by time"
k -n kube-system get events --sort-by=.lastTimestamp 2>&1 || echo "(kubectl failed, or did not answer in 10 s)"
echo "-- the cluster's pods"
k get pods -A -o wide 2>&1 || echo "(kubectl failed, or did not answer in 10 s)"
exit 1
