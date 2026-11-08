#!/bin/sh
# CI's tools from GitHub's release downloads (tools/ci-tools.txt), each kept
# under its release's file name in one directory, which the ci-tools action
# keeps in GitHub's cache: a file already there is checked against its
# digest and used; one missing is fetched, retried with backoff, and
# checked. Nothing that does not match its digest is ever left in place.
#
#   ci-tools.sh key                 the cache key: every tool for this machine's
#                                   architecture, one entry, so a run's restore
#                                   is whole whichever tools it needs
#   ci-tools.sh names               every tool for this machine's architecture
#   ci-tools.sh fetch DIR NAME...   each in DIR, fetched where missing
#
# GitHub's cache scopes an entry to the ref that wrote it, and every run
# reads its own ref's and main's: a pull request's or the merge queue's
# entry is no use to the next one. main's cargo-cache workflow fetches
# every tool and keeps the entry in main's scope, which every run reads.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
arch="$(uname -m)"
case "$arch" in arm64) arch=aarch64 ;; amd64) arch=x86_64 ;; esac
lines() {
	for name in "$@"; do
		found="$(awk -v n="$name" -v a="$arch" '$1 == n && $2 == a { print $3, $4 }' "$here/ci-tools.txt")"
		[ -n "$found" ] || { echo "ci-tools: no $name for $arch in tools/ci-tools.txt" >&2; exit 1; }
		echo "$found"
	done
}
case "${1:-}" in
key)
	echo "ci-tools-$arch-$(awk -v a="$arch" '$2 == a { print $1, $3, $4 }' "$here/ci-tools.txt" | sha256sum | cut -c1-16)"
	;;
names)
	awk -v a="$arch" '$2 == a { print $1 }' "$here/ci-tools.txt" | sort -u | tr '\n' ' '
	echo
	;;
fetch)
	dir="$2"
	shift 2
	mkdir -p "$dir"
	lines "$@" | while read -r url sum; do
		file="$dir/${url##*/}"
		if [ -f "$file" ] && echo "$sum  $file" | sha256sum -c - >/dev/null 2>&1; then
			echo "ci-tools: ${url##*/} from the cache"
			continue
		fi
		rm -f "$file"
		curl -fsSL --retry 6 --retry-all-errors --retry-delay 10 --retry-max-time 300 -o "$file.part" "$url"
		echo "$sum  $file.part" | sha256sum -c - >/dev/null \
			|| { rm -f "$file.part"; echo "ci-tools: ${url##*/} does not match its digest" >&2; exit 1; }
		mv "$file.part" "$file"
		echo "ci-tools: ${url##*/} fetched"
	done
	;;
*)
	echo "usage: ci-tools.sh key | names | fetch DIR NAME..." >&2
	exit 2
	;;
esac
