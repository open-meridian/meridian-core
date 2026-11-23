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
# The JDK image of the release the brief pinned, not its JRE: the JRE is
# linked without jdk.random (49 modules against the JDK's 69), and Jepsen's
# random numbers are L64X128MixRandom, which only jdk.random provides (#99's
# first ha run: "No implementation of the random number generator
# algorithm"; its second: "Module jdk.random not found").
JVM="eclipse-temurin:21.0.8_9-jdk-alpine@sha256:89517925fa675c6c4b770bee7c44d38a7763212741b0d6fca5a5103caab21a97"
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

# elle-cli prints each file, then true, false or unknown (#99's third ha
# run: "/w/history.edn \t true", spaces about the tab). jdk.random
# named, as a jar on the class path resolves a module exporting nothing
# only by service binding.
verdict() {
	docker run --rm --network none -v "$work":/w:ro "$JVM" \
		java --add-modules jdk.random -jar /w/elle-cli.jar --model list-append --consistency-models strict-serializable \
		"/w/$1" 2>&1 | tee "$work/$1.said" | tail -n 40 >&2 || true
	grep -F "/w/$1" "$work/$1.said" | awk '{ print $NF }' | tail -n 1
}
# What elle-cli found, in full: its analysis with each anomaly's operations
# (--output json), where the verdict alone said only that one exists
# (merge-queue run 45104369464: the read and the appends it missed were
# found by hand in the history). Not --verbose: 0.1.10 deprecated it for
# --output json, and 0.1.11's main rewrites it to the one argument
# "--output json", which is no option it knows. Kept whole beside the
# history, in a .e2e-*.log the job keeps. Of the driver's history, said,
# never decided by: the verdict above it is the check. Of the planted one,
# checked below, with the status it ended with, kept in $ended.
kept="$(dirname "$history")"
anomalies() {
	ended=0
	docker run --rm --network none -v "$work":/w:ro "$JVM" \
		java --add-modules jdk.random -jar /w/elle-cli.jar --model list-append --consistency-models strict-serializable \
		--output json "/w/$1" >"$2" 2>&1 || ended=$?
	echo "elle-check: what elle-cli found in $1 is kept whole in $2 ($(wc -c <"$2" | tr -d ' ') bytes)" >&2
}
said="$(verdict history.edn)"
[ "$said" = true ] || { anomalies history.edn "$kept/.e2e-elle-history.log"; echo "elle-check: its first 200 lines:" >&2; head -n 200 "$kept/.e2e-elle-history.log" | cut -c1-4000 >&2; echo "elle-check FAILED: the driver's history is not strict-serializable per key ($said)" >&2; exit 1; }
echo "elle-check: the driver's history is valid: list-append, strict-serializable, per key"
planted="$(verdict planted.edn)"
[ "$planted" = false ] || { echo "elle-check FAILED: the planted anomaly was not refused ($planted): the check cannot fail" >&2; exit 1; }
echo "elle-check: the planted anomaly is refused, as it must be"
# The same question asked of the planted history on every run (the review of
# 2026-11-23, item 10): --output json is otherwise first run on the day a
# real history is refused, when it is too late to find it says nothing. What
# it says must name the anomaly: elle-cli prints its analysis as JSON, whose
# "anomaly-types" lists each kind found (elle-cli 0.1.11, cli.clj:
# json/pprint of the analysis). A usage error, a stack trace or an analysis
# naming none has no such key, and fails the job here, on a planted history,
# never first on a real one (the review of that change, finding 5: a file
# that was not empty passed, whatever it held). And it must end as a history
# refused does, with status 1 (cli.clj: 1 where any history is not valid, 0
# where each is, 255 where it threw): an analysis printed and then an
# exception, or any other end, fails the job too (the review of 2026-11-23
# of that check, N4: its status was dropped, so a stack trace after the key
# passed).
anomalies planted.edn "$kept/.e2e-elle-planted.log"
[ "$ended" = 1 ] || {
	echo "elle-check: what --output json said of the planted history, its first 40 lines:" >&2
	head -n 40 "$kept/.e2e-elle-planted.log" | cut -c1-4000 >&2
	echo "elle-check FAILED: --output json of the planted history ended with status $ended, not the 1 of a history refused: what it would say of a real one is not known" >&2
	exit 1
}
grep -q '"anomaly-types"' "$kept/.e2e-elle-planted.log" || {
	echo "elle-check: what --output json said of the planted history, its first 40 lines:" >&2
	head -n 40 "$kept/.e2e-elle-planted.log" | cut -c1-4000 >&2
	echo "elle-check FAILED: --output json named no anomaly in the planted history (no \"anomaly-types\" in .e2e-elle-planted.log): what it would say of a real one is not known" >&2
	exit 1
}
echo "elle-check: --output json names the planted anomaly"
