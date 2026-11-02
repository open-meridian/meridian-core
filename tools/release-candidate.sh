#!/usr/bin/env bash
# The release candidate (ruling 1071): the runtime and analytics images built
# once, by the CI run that tests the tree main will hold, and published by
# promotion, never by a second build.
#
#   release-candidate.sh version
#       the release the run builds, <series>.<ci run number>+<commit>, and
#       the tree it builds, as `version=` and `tree=` lines for
#       $GITHUB_OUTPUT. The commit is the one the branch carried: the second
#       parent of the merge CI checks out, as publish names it.
#   release-candidate.sh pushed <local image> <repository>
#       the digest the build that loaded the local image pushed it at, by
#       digest and no tag (the Makefile's CANDIDATE_REGISTRY): refused unless
#       the image there has the local image's layers, the image this run's
#       jobs test.
#   release-candidate.sh tag <tree> <repository> <digest>...
#       the digests, one per architecture, as one manifest list tagged
#       tested-<tree>: only once every required check of the run is green.
#   release-candidate.sh promote <tree> <commit> <repository>
#       publish's: the list tested-<tree> tagged <commit> and latest, with no
#       rebuild; refused when no green run tested that tree. Prints the
#       version the images say.
#
# Only in CI, logged in to the registry.

set -euo pipefail

label='org.opencontainers.image.version'

case "${1:-}" in
  version)
    series=$(awk '/^version:/ {print $2}' deploy/chart/Chart.yaml)
    if git rev-parse -q --verify 'HEAD^2' >/dev/null; then carried=HEAD^2; else carried=HEAD; fi
    echo "version=${series%.*}.${GITHUB_RUN_NUMBER:?}+$(git rev-parse --short "$carried")"
    echo "tree=$(git rev-parse 'HEAD^{tree}')"
    ;;
  pushed)
    [ $# -eq 3 ] || { echo "usage: $0 pushed <local image> <repository>" >&2; exit 2; }
    local=$2 repo=$3
    meta=".candidate-${repo##*/}.json"
    [ -s "$meta" ] || { echo "release-candidate: no $meta; was the image built with CANDIDATE_REGISTRY?" >&2; exit 1; }
    digest=$(jq -r '."containerimage.digest"' "$meta")
    [ -n "$digest" ] && [ "$digest" != null ] || { echo "release-candidate: $meta names no digest" >&2; exit 1; }
    # The layers pushed, by their uncompressed digests, against the image the
    # run's jobs test: the same, or the candidate is not what was tested.
    pushed=$(docker buildx imagetools inspect --format '{{json .Image}}' "$repo@$digest" \
      | jq -c '(if has("rootfs") then . else (to_entries | map(select(.key | test("^linux/"))) | .[0].value) end) | .rootfs.diff_ids')
    tested=$(docker image inspect --format '{{json .RootFS.Layers}}' "$local" | jq -c .)
    [ "$pushed" = "$tested" ] || { echo "release-candidate: $repo@$digest is not $local, the image tested: its layers differ" >&2; exit 1; }
    echo "release-candidate: $local is $repo@$digest, pushed untagged" >&2
    echo "$digest"
    ;;
  tag)
    [ $# -ge 4 ] || { echo "usage: $0 tag <tree> <repository> <digest>..." >&2; exit 2; }
    tree=$2 repo=$3
    shift 3
    sources=()
    for digest in "$@"; do sources+=("$repo@$digest"); done
    docker buildx imagetools create -t "$repo:tested-$tree" "${sources[@]}"
    echo "release-candidate: $repo:tested-$tree is $(printf '%s ' "$@")"
    ;;
  promote)
    [ $# -eq 4 ] || { echo "usage: $0 promote <tree> <commit> <repository>" >&2; exit 2; }
    tree=$2 commit=$3 repo=$4
    docker buildx imagetools inspect "$repo:tested-$tree" >/dev/null 2>&1 \
      || { echo "release-candidate: no run of ci tested tree $tree green, so $repo has no candidate for it: nothing is published" >&2; exit 1; }
    version=$(docker buildx imagetools inspect --format '{{json .Image}}' "$repo:tested-$tree" \
      | jq -r --arg l "$label" '(if has("rootfs") then . else (to_entries | map(select(.key == "linux/amd64")) | .[0].value) end) | .config.Labels[$l]')
    case "$version" in
      *+"$commit") ;;
      *) echo "release-candidate: $repo:tested-$tree says version '$version', which does not name $commit" >&2; exit 1 ;;
    esac
    docker buildx imagetools create -t "$repo:$commit" -t "$repo:latest" "$repo:tested-$tree"
    echo "release-candidate: $repo:$commit and latest are tested-$tree, $version" >&2
    echo "$version"
    ;;
  *)
    echo "usage: $0 version | push | tag | promote" >&2
    exit 2
    ;;
esac
