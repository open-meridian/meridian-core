#!/bin/sh
# `image-load.sh K3D CLUSTER IMAGE...`: hand each image to a k3d cluster and
# prove it arrived (v27 stage E, the plan's "Added 2026-10-19").
#
# `k3d image import` can log an error -- the tarball missing in the node --
# and still exit 0, and the e2e targets discarded its output, so a pod later
# pulled a tag that existed nowhere. Here each import's output is kept; the
# image is then looked for in every node's containerd (`crictl images`) --
# each server's and each agent's, since a pod is placed on any of them (PR
# CI 43152673832: the scaled run's street and settings-key pods on agent-0
# pulled meridian-runtime:local, which the import had left on the server
# alone) -- the import tried once more where it is not on every one, and the
# run stopped with the reason and what the import said where one still
# lacks it.
set -u
k3d="$1"
cluster="$2"
shift 2
log=$(mktemp)
trap 'rm -f "$log"' EXIT

# The cluster's nodes that run pods: its servers and its agents.
nodes=$(docker ps --filter "label=k3d.cluster=$cluster" --format '{{.Names}}' |
	grep -E "^k3d-${cluster}-(server|agent)-[0-9]+$" | sort)
[ -n "$nodes" ] || nodes="k3d-${cluster}-server-0"

# Whether NODE's containerd holds IMAGE, named as Docker names it:
# `name:tag`, a name with no tag being `:latest`, held under its registry's
# full name (docker.io/library/name) or as given.
present() {
	node="$1"
	want="$2"
	case "$want" in
	*@*) ;;
	*:*) case "${want##*/}" in *:*) ;; *) want="$want:latest" ;; esac ;;
	*) want="$want:latest" ;;
	esac
	docker exec "$node" crictl images 2>/dev/null | awk 'NR > 1 { print $1 ":" $2 }' |
		while read -r held; do
			case "$held" in
			"$want" | */"$want") echo yes; break ;;
			esac
		done | grep -q yes
}

# The nodes that do not hold IMAGE.
lacking() {
	for node in $nodes; do
		present "$node" "$1" || echo "$node"
	done
}

for image in "$@"; do
	# An image this host never built or pulled (postgres:16-alpine) is not
	# there for k3d to hand over: pulled first, said so.
	if ! docker image inspect "$image" >/dev/null 2>&1; then
		echo "image-load: $image is not on this host; pulling it"
		docker pull -q "$image" >"$log" 2>&1 || {
			echo "image-load: $image could not be pulled:" >&2
			tail -20 "$log" >&2
			exit 1
		}
	fi
	missing=
	for attempt in 1 2; do
		"$k3d" image import -c "$cluster" "$image" >"$log" 2>&1
		missing=$(lacking "$image")
		[ -z "$missing" ] && break
		echo "image-load: $image is not in $(echo $missing) after import attempt $attempt of 2" >&2
	done
	if [ -n "$missing" ]; then
		echo "image-load: $image never arrived in $(echo $missing); k3d image import said:" >&2
		tail -20 "$log" >&2
		for node in $missing; do
			echo "image-load: and $node holds:" >&2
			docker exec "$node" crictl images >&2 2>&1 || echo "image-load: $node could not be asked" >&2
		done
		exit 1
	fi
	echo "image-load: $image is in every node ($(echo $nodes))"
done
