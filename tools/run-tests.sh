#!/usr/bin/env bash
# The workspace's test binaries, compiled once and run where they are needed.
#
# Dockerfile.rust's `compile` stage builds every test binary the gates run in
# one cargo invocation and keeps them, and nothing else, in its image. The
# targets that run tests -- `test` in a build stage, test-store, test-broker
# and test-directory beside the services they need -- run those binaries here
# rather than each compiling the workspace again: three of them used to, from
# nothing, in a container's own layer, which is where the gate ran out of disk
# (2026-10-12).
#
#   run-tests.sh collect < cargo's JSON messages
#       In the compile stage: copy each executable cargo reported -- the test
#       binaries, and those of the workspace's own binaries a test starts by
#       the path cargo compiled into it (CARGO_BIN_EXE_<name>) -- to
#       $MERIDIAN_TESTS_DIR/<package>/<target> and list it in manifest.tsv.
#       A binary no test names is left out: the runtime's eleven, every one
#       copied, were 1.1 GB of the image, and its tests start three (ruling
#       354).
#
#   run-tests.sh link
#       Once the target cache is no longer mounted: put each workspace binary
#       back at the path cargo built it at, as a link to its copy.
#
#   run-tests.sh [--workspace | -p <package>...] [--lib] [--test <name>...] [-- <args>]
#       Run the selected binaries as `cargo test` selects and runs them: every
#       named target of every named package that has it, a test named that no
#       package has refused, each binary from its package's directory, and the
#       first that fails stopping the run with its status.

set -euo pipefail

DIR=${MERIDIAN_TESTS_DIR:-/tests}
MANIFEST=$DIR/manifest.tsv

if [ "${1:-}" = collect ]; then
  mkdir -p "$DIR"
  rows=$(mktemp)
  # One compiler-artifact message per line; the test executables are the ones
  # whose "executable" is a path rather than null.
  sed -n 's/.*"manifest_path":"\([^"]*\)".*"target":{"kind":\["\([a-z-]*\)".*"name":"\([^"]*\)".*"executable":"\([^"]*\)".*/\1\t\2\t\3\t\4/p' \
    | while IFS=$'\t' read -r manifest kind name exe; do
        pkg=$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$manifest" | head -1)
        # Cargo gives a package's binaries' paths to that package's own
        # integration tests and to nothing else, so a binary is kept when its
        # package's sources name it, and one they do not is one no test here
        # can start.
        if [ "$kind" = bin ] \
          && ! grep -rqF "CARGO_BIN_EXE_$name\"" "$(dirname "$manifest")"; then
          continue
        fi
        mkdir -p "$DIR/$pkg"
        cp "$exe" "$DIR/$pkg/$name"
        printf '%s\t%s\t%s\t%s\t%s\n' "$pkg" "$kind" "$name" "$(dirname "$manifest")" "$exe"
      done >"$rows"
  [ -s "$rows" ] || { echo "run-tests: cargo reported no test executables" >&2; exit 1; }
  LC_ALL=C sort -t$'\t' -k1,1 -k2,2 -k3,3 "$rows" >"$MANIFEST"
  rm -f "$rows"
  echo "run-tests: $(wc -l <"$MANIFEST") test binaries in $DIR"
  exit 0
fi

if [ "${1:-}" = link ]; then
  while IFS=$'\t' read -r pkg kind name _ built; do
    [ "$kind" = bin ] || continue
    mkdir -p "$(dirname "$built")"
    ln -sf "$DIR/$pkg/$name" "$built"
  done <"$MANIFEST"
  exit 0
fi

[ -f "$MANIFEST" ] || { echo "run-tests: no $MANIFEST; this runs in the image Dockerfile.rust's compile stage made" >&2; exit 2; }

packages=() tests=() lib=0 every=0
while [ $# -gt 0 ]; do
  case "$1" in
    --workspace) every=1 ;;
    -p|--package) packages+=("$2"); shift ;;
    --lib) lib=1 ;;
    --test) tests+=("$2"); shift ;;
    --locked) ;;  # as cargo is given it: nothing is resolved here
    --) shift; break ;;
    *) echo "run-tests: '$1' is not something this runner selects by" >&2; exit 2 ;;
  esac
  shift
done
# A virtual workspace with no package named runs every member, as cargo does.
[ ${#packages[@]} -gt 0 ] || every=1
[ $lib = 1 ] || [ ${#tests[@]} -gt 0 ] \
  || { echo "run-tests: name --lib or a --test; running everything is not a selection" >&2; exit 2; }

among() { local want=$1; shift; local x; for x in "$@"; do [ "$x" = "$want" ] && return 0; done; return 1; }

for p in "${packages[@]+"${packages[@]}"}"; do
  cut -f1 "$MANIFEST" | grep -qxF "$p" || { echo "run-tests: no package named $p" >&2; exit 101; }
done
for t in "${tests[@]+"${tests[@]}"}"; do
  awk -F'\t' -v t="$t" '$2 == "test" && $3 == t {found=1} END {exit !found}' "$MANIFEST" \
    || { echo "run-tests: no test target named $t" >&2; exit 101; }
done

ran=0
while IFS=$'\t' read -r -u 3 pkg kind name dir _; do
  [ $every = 1 ] || among "$pkg" "${packages[@]}" || continue
  case "$kind" in
    lib) [ $lib = 1 ] || continue ;;
    test) among "$name" "${tests[@]+"${tests[@]}"}" || continue ;;
    *) continue ;;
  esac
  echo "     Running $pkg ($kind $name)" >&2
  ( cd "$dir" && CARGO_MANIFEST_DIR="$dir" CARGO_PKG_NAME="$pkg" "$DIR/$pkg/$name" "$@" ) || exit $?
  ran=$((ran + 1))
done 3<"$MANIFEST"

[ $ran -gt 0 ] || { echo "run-tests: the selection named no binary" >&2; exit 101; }
