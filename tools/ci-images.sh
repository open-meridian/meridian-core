#!/usr/bin/env bash
# The gate's images, carried from CI's jobs that build them to the jobs that
# run them (ruling 1068): built once per run, never compiled again.
#
#   ci-images.sh save <dir> <name>...   each image, by its name in the
#       Makefile's IMAGE_TARGETS (`make ci-images` lists them: runtime,
#       tests, ...), written to <dir>/<name>.tar.zst with its image ID beside
#       it, for the job to upload as the run's artifact
#   ci-images.sh load <dir> <name>...   each loaded from what `save` wrote,
#       under the local name every make target runs, and refused unless its
#       image ID is the one `save` recorded
#
# An artifact of the run rather than a registry: what a pull request built
# never reaches a package a deployment pulls from, and nothing is kept past
# the run's retention. The release candidates publish promotes are pushed to
# the registry by digest, from the merged tree (ruling 1071).

set -euo pipefail

usage() { echo "usage: $0 save|load <dir> <name>..." >&2; exit 2; }
[ $# -ge 3 ] || usage
mode=$1 dir=$2
shift 2

# The local name of each image, as the Makefile names it.
local_name() {
  local name
  name=$(make -s --no-print-directory ci-images | awk -v n="$1" '$1 == n { print $2 }')
  [ -n "$name" ] || { echo "ci-images: no image named '$1'; the Makefile's are: $(make -s --no-print-directory ci-images | cut -d' ' -f1 | tr '\n' ' ')" >&2; exit 2; }
  echo "$name"
}

case "$mode" in
  save)
    mkdir -p "$dir"
    for name in "$@"; do
      image=$(local_name "$name")
      start=$(date +%s)
      docker image inspect --format '{{.Id}}' "$image" >"$dir/$name.id"
      docker save "$image" | zstd -q -T0 -3 -o "$dir/$name.tar.zst"
      echo "ci-images: saved $image ($(docker image inspect --format '{{.Size}}' "$image" | awk '{printf "%.0f", $1/1048576}') MB) as $name.tar.zst ($(du -m "$dir/$name.tar.zst" | cut -f1) MB) in $(( $(date +%s) - start ))s"
    done
    ;;
  load)
    for name in "$@"; do
      image=$(local_name "$name")
      [ -s "$dir/$name.tar.zst" ] || { echo "ci-images: the run saved no image named '$name' in $dir" >&2; exit 1; }
      start=$(date +%s)
      zstd -q -d -c "$dir/$name.tar.zst" | docker load -q >/dev/null
      want=$(cat "$dir/$name.id")
      got=$(docker image inspect --format '{{.Id}}' "$image")
      [ "$got" = "$want" ] || { echo "ci-images: $image loaded as $got, and the run built $want" >&2; exit 1; }
      rm -f "$dir/$name.tar.zst"
      echo "ci-images: $image is $want, as built for this run, loaded in $(( $(date +%s) - start ))s"
    done
    ;;
  *) usage ;;
esac
