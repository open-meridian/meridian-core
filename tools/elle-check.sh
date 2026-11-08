#!/bin/sh
# Elle on the write driver's history (step 3b part 5; the PO's ruling of
# 2026-11-07, item 2): in the ha failover CI job only, never in a product
# image and never on a laptop. elle-cli (a pinned release, its zip checked
# against its digest by tools/ci-tools.sh) on a pinned JVM image.
#
# The model checked: list-append under strict-serializable, on single-key
# transactions, which is per-partition linearizability -- the book's
# account partitions each one fenced log with one writer at a time (step
# 3a; spec/a-deployment-scales-out-and-fails-over-by-configuration section
# 2, items 2 and 4, and section 5; ruling 1048): an order acknowledged is
# kept, and every later read of its account sees it, in the one order the
# partition's log took it. The lake is left out: its commits are local
# (the spec's section 4, durability), so an acknowledged row may go with a
# lost primary within its lag bound, which a linearizability check would
# call an anomaly the spec allows; its loss is measured and said by the run
# itself instead.
#
# Then a copy of the history with one read reordered (tools/elle-plant.py)
# must be refused: a check that cannot fail proves nothing.
#
# Usage: elle-check.sh HISTORY TOOLS_DIR
set -eu
history="$1"
tools="$2"
[ -n "${GITHUB_ACTIONS:-}" ] || { echo "elle-check: runs in CI's ha failover job only (ruling of 2026-11-07)" >&2; exit 2; }
ELLE_ZIP="elle-cli-bin-0.1.11.zip"
JVM="eclipse-temurin:21.0.8_9-jre-alpine@sha256:990397e0495ac088ab6ee3d949a2e97b715a134d8b96c561c5d130b3786a489d"
[ -s "$history" ] || { echo "elle-check FAILED: no history at $history" >&2; exit 1; }
[ -f "$tools/$ELLE_ZIP" ] || { echo "elle-check FAILED: $ELLE_ZIP is not in $tools (tools/ci-tools.sh fetch)" >&2; exit 1; }
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
unzip -q "$tools/$ELLE_ZIP" -d "$work/elle"
jar="$(find "$work/elle" -name '*standalone*.jar' | head -1)"
[ -n "$jar" ] || jar="$(find "$work/elle" -name '*.jar' | head -1)"
[ -n "$jar" ] || { echo "elle-check FAILED: no jar in $ELLE_ZIP" >&2; exit 1; }
cp "$jar" "$work/elle-cli.jar"
cp "$history" "$work/history.edn"
python3 "$(dirname "$0")/elle-plant.py" "$work/history.edn" "$work/planted.edn"
printf 'elle-check: %s operations, %s acknowledged appends, %s reads\n' \
	"$(wc -l <"$work/history.edn" | tr -d ' ')" \
	"$(grep -c ':type :ok, :f :txn, :value \[\[:append' "$work/history.edn" || true)" \
	"$(grep -c ':type :ok, :f :txn, :value \[\[:r' "$work/history.edn" || true)"

# elle-cli prints each file, a tab, and true, false or unknown. Jepsen's
# random numbers are L64X128MixRandom, which only jdk.random provides; the
# JRE image holds it, and a jar on the class path resolves it only when
# named (#99's first run: "No implementation of the random number
# generator algorithm").
verdict() {
	docker run --rm --network none -v "$work":/w:ro "$JVM" \
		java --add-modules jdk.random -jar /w/elle-cli.jar --model list-append --consistency-models strict-serializable \
		"/w/$1" 2>&1 | tee "$work/$1.said" | tail -n 40 >&2 || true
	awk -F '\t' -v f="/w/$1" '$1 == f { print $2 }' "$work/$1.said" | tail -n 1
}
said="$(verdict history.edn)"
[ "$said" = true ] || { echo "elle-check FAILED: the driver's history is not strict-serializable per key ($said)" >&2; exit 1; }
echo "elle-check: the driver's history is valid: list-append, strict-serializable, per key"
planted="$(verdict planted.edn)"
[ "$planted" = false ] || { echo "elle-check FAILED: the planted anomaly was not refused ($planted): the check cannot fail" >&2; exit 1; }
echo "elle-check: the planted anomaly is refused, as it must be"
