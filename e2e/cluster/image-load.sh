#!/bin/sh
# `image-load.sh K3D CLUSTER IMAGE...`: hand each image to a k3d cluster and
# prove it arrived (v27 stage E, the plan's "Added 2026-10-19").
#
# `k3d image import` can log an error -- the tarball missing in the node --
# and still exit 0, and the e2e targets discarded its output, so a pod later
# pulled a tag that existed nowhere. Here each import's output is kept; the
# image is then looked for in the server node's containerd (`crictl images`),
# the import tried once more where it is not there, and the run stopped with
# the reason and what the import said where it is still missing.
set -u
k3d="$1"
cluster="$2"
shift 2
node="k3d-${cluster}-server-0"
log=$(mktemp)
trap 'rm -f "$log"' EXIT

# Whether the node's containerd holds IMAGE, named as Docker names it:
# `name:tag`, a name with no tag being `:latest`, held under its registry's
# full name (docker.io/library/name) or as given.
present() {
	want="$1"
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
	loaded=
	for attempt in 1 2; do
		"$k3d" image import -c "$cluster" "$image" >"$log" 2>&1
		if present "$image"; then
			loaded=yes
			break
		fi
		echo "image-load: $image is not in $node after import attempt $attempt of 2" >&2
	done
	if [ -z "$loaded" ]; then
		echo "image-load: $image never arrived in $node; k3d image import said:" >&2
		tail -20 "$log" >&2
		echo "image-load: and the node holds:" >&2
		docker exec "$node" crictl images >&2 2>&1 || echo "image-load: $node could not be asked" >&2
		exit 1
	fi
	echo "image-load: $image is in $node"
done
