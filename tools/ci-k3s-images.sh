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
# pod, and k3d finds the node image on this host and pulls nothing. k3d's
# own two images (its loadbalancer and tools node) are kept and loaded the
# same way, so making the cluster pulls from no registry at all; the
# Makefile says what the host pulled while it was made, and CI's cluster
# jobs fail a run where that is not none.
#
#   ci-k3s-images.sh tag [K3S]       the k3s tag: K3S, or the Makefile's pin
#   ci-k3s-images.sh key TAG         the cache key, for this architecture
#   ci-k3s-images.sh fetch DIR TAG   each in DIR, fetched where missing or
#                                    not matching its digest
#   ci-k3s-images.sh load DIR TAG    the node image and k3d's into Docker,
#                                    the tarball checked; prints its path
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
# Where an image is saved in DIR: its name with / and : as -.
saved_as() { echo "$(echo "$1" | tr '/:' '--')-$k3s_arch.tar"; }
checked() { [ -f "$1" ] && echo "$2  $1" | sha256sum -c - >/dev/null 2>&1; }

# k3d's own images, which `k3d cluster create` pulls where Docker has none:
# its loadbalancer (k3d-proxy) and its tools node (k3d-tools), each tagged
# with k3d's version, the one tools/ci-tools.txt pins. Run 45300919431 died
# making the cluster when its runner could not pull them; loaded here, k3d
# finds them on this host and pulls nothing.
k3d_version() {
	v="$(sed -n 's#.*/k3d-io/k3d/releases/download/v\([0-9][0-9.]*\)/.*#\1#p' "$here/ci-tools.txt" | sort -u)"
	[ "$(echo "$v" | grep -c .)" = 1 ] || {
		echo "ci-k3s-images: tools/ci-tools.txt pins no one k3d version (found: $(echo $v))" >&2
		exit 1
	}
	echo "$v"
}
k3d_images() { echo "ghcr.io/k3d-io/k3d-proxy:$(k3d_version) ghcr.io/k3d-io/k3d-tools:$(k3d_version)"; }

# A tarball from the k3s release, on a miss in the cache. Tries after waits
# that grow (back-off), FETCH_WAITS seconds, and only while the next try can
# begin inside FETCH_WITHIN seconds of the first: the 300 that bounded
# curl's own retries before. Each try also ends a transfer that stalls (under
# 10 kB/s for 60 s, curl exit 28). Queue run 45307348738 met a 403 (curl
# exit 22) on all seven of its tries, 10 s apart, inside one minute.
fetched() {
	url="$1" out="$2" began="$(date +%s)" try=0
	for wait in ${FETCH_WAITS:-0 15 45 120}; do
		if [ $(( $(date +%s) + wait - began )) -gt "${FETCH_WITHIN:-300}" ]; then break; fi
		sleep "$wait"
		try=$((try + 1))
		rc=0
		curl -fsSL --connect-timeout 20 --speed-limit 10000 --speed-time 60 -o "$out" "$url" || rc=$?
		[ "$rc" != 0 ] || return 0
		rm -f "$out"
		echo "ci-k3s-images: try $try for $url failed (curl exit $rc) after a ${wait}s wait" >&2
	done
	echo "ci-k3s-images: $url was not fetched in $try tries, within ${FETCH_WITHIN:-300}s" >&2
	exit 1
}

# An image from its registry, before any cluster is made: three tries at
# most, after waits of 0, 20 and 60 s.
pulled() {
	for wait in 0 20 60; do
		sleep "$wait"
		if docker pull -q "$1" >/dev/null; then return 0; fi
		echo "ci-k3s-images: $1 was not pulled (attempt after a ${wait}s wait)" >&2
	done
	exit 1
}

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
	echo "k3s-images-$arch-$2-$(echo "$want" | cut -c1-16)-k3d-$(k3d_version)"
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
		fetched "$url" "$file.part"
		echo "$want  $file.part" | sha256sum -c - >/dev/null \
			|| { rm -f "$file.part"; echo "ci-k3s-images: $(airgap) for $tag does not match its digest" >&2; exit 1; }
		mv "$file.part" "$file"
		echo "ci-k3s-images: $(airgap) for $tag fetched"
	fi
	# The node image and k3d's own two, saved as Docker holds them. Fetching
	# is the one time they come from a registry, before any cluster is made.
	for image in "rancher/k3s:$tag" $(k3d_images); do
		saved="$dir/$(saved_as "$image")"
		if [ -s "$saved" ]; then
			echo "ci-k3s-images: $image from the cache"
		else
			pulled "$image"
			docker save -o "$saved.part" "$image"
			mv "$saved.part" "$saved"
			echo "ci-k3s-images: $image pulled and saved"
		fi
	done
	;;
load)
	dir="$2" tag="$3"
	file="$dir/$(airgap)" want="$(sum "$tag")"
	checked "$file" "$want" \
		|| { echo "ci-k3s-images: $file is missing or does not match its digest; fetch it first" >&2; exit 1; }
	for image in "rancher/k3s:$tag" $(k3d_images); do
		docker load -q -i "$dir/$(saved_as "$image")" >&2
		docker image inspect "$image" >/dev/null
	done
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
