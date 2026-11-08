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
#       Run the selected binaries as `cargo test` selects them: every named
#       target of every named package that has it, a test named that no
#       package has refused, and the first that fails stopping the run with
#       its status. Each binary under cargo-nextest (ruling 1104; step 3b part
#       4), from the binaries' list the compile stage kept: each test its own
#       process, from its package's directory; its results in JUnit, copied
#       to $MERIDIAN_JUNIT_DIR where that is set; and under NEXTEST_PROFILE=ci
#       (CI's) a failed test run once more, a test that fails and then passes
#       named flaky there. <args> are libtest's: filters, --ignored,
#       --include-ignored, --exact, --nocapture.

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
        # Where cargo built it, under this directory's target/, so the
        # binaries' list nextest reads finds it there (--target-dir-remap).
        kept="$DIR/target/${exe#/w/target/}"
        mkdir -p "$(dirname "$kept")"
        cp "$exe" "$kept"
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
    ln -sf "$DIR/target/${built#/w/target/}" "$built"
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

# libtest's arguments as nextest takes them: filters stay filters.
filters=() options=()
while [ $# -gt 0 ]; do
  case "$1" in
    --ignored) options+=(--run-ignored only) ;;
    --include-ignored) options+=(--run-ignored all) ;;
    --nocapture) options+=(--no-capture) ;;
    --exact) filters+=(--exact) ;;
    -*) echo "run-tests: libtest's '$1' is not one this runner passes on" >&2; exit 2 ;;
    *) filters+=("$1") ;;
  esac
  shift
done

profile=${NEXTEST_PROFILE:-default}
ran=0
while IFS=$'\t' read -r -u 3 pkg kind name dir _; do
  [ $every = 1 ] || among "$pkg" "${packages[@]}" || continue
  case "$kind" in
    lib) [ $lib = 1 ] || continue; id="$pkg" ;;
    test) among "$name" "${tests[@]+"${tests[@]}"}" || continue; id="$pkg::$name" ;;
    *) continue ;;
  esac
  echo "     Running $pkg ($kind $name)" >&2
  status=0
  marker=$(mktemp)
  cargo-nextest nextest run --binaries-metadata "$DIR/binaries.json" \
    --cargo-metadata "$DIR/cargo-metadata.json" --target-dir-remap "$DIR/target" \
    --workspace-remap /w --profile "$profile" --no-tests=pass -E "binary_id($id)" \
    "${options[@]+"${options[@]}"}" ${filters[@]+-- "${filters[@]}"} || status=$?
  # nextest keeps its JUnit in its store, under the target directory: the
  # one cargo's metadata names or the one it was remapped to. The file
  # written by this run, whichever.
  if [ -n "${MERIDIAN_JUNIT_DIR:-}" ]; then
    junit=$(find "$DIR/target/nextest" /w/target/nextest -path "*/$profile/junit.xml" \
      -newer "$marker" 2>/dev/null | head -1 || true)
    [ -z "$junit" ] || cp "$junit" "$MERIDIAN_JUNIT_DIR/$pkg.$kind.$name.xml" || true
  fi
  rm -f "$marker"
  [ $status = 0 ] || exit $status
  ran=$((ran + 1))
done 3<"$MANIFEST"

[ $ran -gt 0 ] || { echo "run-tests: the selection named no binary" >&2; exit 101; }
