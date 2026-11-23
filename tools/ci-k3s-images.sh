#!/bin/sh
# k3s's own images for a cluster job, so making the cluster pulls nothing
# from Docker Hub: the node image (rancher/k3s) and the system images k3s
# runs in kube-system (pause, CoreDNS, local-path-provisioner,
# metrics-server, Traefik and the Helm and service-LB images that install
# it), the second as the release's airgap tarball, pinned by its digest in
# tools/ci-k3s-images.txt. Cluster jobs died from 2026-11-17 in a `ready`
# wait on kube-system while those images came from Docker Hub
# (coordinator-records 2026-11-23-runner-deaths, cause 2): with them here,
# that cause is ruled in or out. The k3s-images action restores DIR from
# GitHub's cache, where main keeps an entry for the Makefile's k3s alone, one
# per architecture; every other k3s is fetched from its release each run (the
# repository's cache is over its 10 GB limit, and an entry per k3s, 176-263 MB
# each, would push out the Rust build caches). The Makefile hands the tarball
# to every node (E2E_K3S_AIRGAP), where k3s imports it before it starts a
# pod, and k3d finds the node image on this host and pulls nothing.
#
#   ci-k3s-images.sh tag [K3S]       the k3s tag: K3S, or the Makefile's pin
#   ci-k3s-images.sh key TAG         the cache key, for this architecture
#   ci-k3s-images.sh fetch DIR TAG   each in DIR, fetched where missing or
#                                    not matching its digest
#   ci-k3s-images.sh load DIR TAG    the node image into Docker, the tarball
#                                    checked; prints the tarball's path
#   ci-k3s-images.sh rows            the Makefile's k3s's rows, one per
#                                    architecture, as a JSON matrix for main's
#                                    cargo-cache workflow, which keeps each
#                                    entry where every run reads it
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
arch="$(uname -m)"
case "$arch" in arm64) arch=aarch64 ;; amd64) arch=x86_64 ;; esac
case "$arch" in aarch64) k3s_arch=arm64 ;; *) k3s_arch=amd64 ;; esac

# The row's digest for TAG on this architecture, or a refusal naming it.
sum() {
	found="$(awk -v t="$1" -v a="$arch" '$1 == t && $2 == a { print $3 }' "$here/ci-k3s-images.txt")"
	[ -n "$found" ] || {
		echo "ci-k3s-images: no row for k3s $1 on $arch in tools/ci-k3s-images.txt; add its airgap tarball's published digest" >&2
		exit 1
	}
	echo "$found"
}
airgap() { echo "k3s-airgap-images-$k3s_arch.tar.zst"; }
node() { echo "rancher-k3s-$1-$k3s_arch.tar"; }
checked() { [ -f "$1" ] && echo "$2  $1" | sha256sum -c - >/dev/null 2>&1; }

case "${1:-}" in
tag)
	if [ -n "${2:-}" ]; then
		echo "$2"
	else
		echo "v$(sed -n 's/^KUBERNETES_VERSION := //p' "$here/../Makefile")-k3s1"
	fi
	;;
key)
	want="$(sum "$2")"
	echo "k3s-images-$arch-$2-$(echo "$want" | cut -c1-16)"
	;;
fetch)
	dir="$2" tag="$3"
	want="$(sum "$tag")"
	mkdir -p "$dir"
	file="$dir/$(airgap)"
	if checked "$file" "$want"; then
		echo "ci-k3s-images: $(airgap) for $tag from the cache"
	else
		rm -f "$file"
		# The release's tag is the image's with + for its last -.
		release="$(echo "$tag" | sed 's/-\(k3s[0-9]*\)$/+\1/')"
		url="https://github.com/k3s-io/k3s/releases/download/$(echo "$release" | sed 's/+/%2B/')/$(airgap)"
		curl -fsSL --retry 6 --retry-all-errors --retry-delay 10 --retry-max-time 300 -o "$file.part" "$url"
		echo "$want  $file.part" | sha256sum -c - >/dev/null \
			|| { rm -f "$file.part"; echo "ci-k3s-images: $(airgap) for $tag does not match its digest" >&2; exit 1; }
		mv "$file.part" "$file"
		echo "ci-k3s-images: $(airgap) for $tag fetched"
	fi
	# The node image, saved as Docker holds it. Fetching is the one time it
	# comes from Docker Hub, before any cluster is made, three tries at most.
	saved="$dir/$(node "$tag")"
	if [ -s "$saved" ]; then
		echo "ci-k3s-images: rancher/k3s:$tag from the cache"
	else
		for wait in 0 20 60; do
			sleep "$wait"
			if docker pull -q "rancher/k3s:$tag" >/dev/null; then break; fi
			echo "ci-k3s-images: rancher/k3s:$tag was not pulled (attempt after a ${wait}s wait)" >&2
			[ "$wait" != 60 ] || exit 1
		done
		docker save -o "$saved.part" "rancher/k3s:$tag"
		mv "$saved.part" "$saved"
		echo "ci-k3s-images: rancher/k3s:$tag pulled and saved"
	fi
	;;
load)
	dir="$2" tag="$3"
	file="$dir/$(airgap)" want="$(sum "$tag")"
	checked "$file" "$want" \
		|| { echo "ci-k3s-images: $file is missing or does not match its digest; fetch it first" >&2; exit 1; }
	docker load -q -i "$dir/$(node "$tag")" >&2
	docker image inspect "rancher/k3s:$tag" >/dev/null
	echo "$file"
	;;
rows)
	pin="$("$0" tag)"
	awk -v pin="$pin" '!/^#/ && NF == 3 && $1 == pin {
		printf "%s{\"k3s\":\"%s\",\"runner\":\"%s\"}", (n++ ? "," : "["), $1, ($2 == "aarch64" ? "ubuntu-24.04-arm" : "ubuntu-24.04")
	} END { print "]" }' "$here/ci-k3s-images.txt"
	;;
*)
	echo "usage: ci-k3s-images.sh tag [K3S] | key TAG | fetch DIR TAG | load DIR TAG | rows" >&2
	exit 2
	;;
esac
