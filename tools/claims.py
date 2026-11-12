#!/usr/bin/env python3
"""The claims ledger (366; ruling 1113; spec/a-deployment-scales-out-and-fails-
over-by-configuration section 15): each failure class's measured recovery, per
profile and component, beside its bound, its RPO and the runs that measured
it, kept in deploy/claims-ledger.json. Nothing about recovery is stated
anywhere (the docs' service levels, the security-review pack) that is not an
entry here, and an entry is written only from green runs.

The runs write their measurements as claims lines (`CLAIM {json}`), kept in
.e2e-failover-claims.jsonl (e2e-cluster-failover, both profiles) and
.e2e-backup-claims.jsonl (e2e-cluster-backup); CI keeps each as an artifact.

Usage:
  claims.py ledger --out L F...      the ledger written from runs' claims files:
                                     per class, profile and component, the
                                     measured min, median and max, the bound,
                                     the RPO and the runs
  claims.py check --ledger L [--every] [F...]
                                     the ledger held to its bounds; with a run's
                                     files, each of their measurements held to
                                     its bound, and every class the ledger
                                     holds for a profile they measured (with
                                     --every, every class it holds) measured
                                     again: none dropped silently
  claims.py --self-test

Exit: 0 held, 1 a bound missed or a class dropped, 2 unreadable input.
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import sys

ABOUT = (
    "The claims ledger (366; ruling 1113): each failure class's recovery as "
    "measured by CI's green runs, per profile and component, beside the bound "
    "it is held to (rulings 1048, 1056) and its RPO. Written by `make "
    "claims-ledger` from the runs' claims files; checked by `make claims-check` "
    "in the gate and against every run in CI. A figure is stated elsewhere only "
    "from here. Times in seconds."
)


def read_claims(paths: list[str]) -> list[dict]:
    lines = []
    for path in paths:
        with open(path, encoding="utf-8") as kept:
            for number, line in enumerate(kept, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    said = json.loads(line)
                except json.JSONDecodeError as failed:
                    raise SystemExit(f"claims: {path}:{number} is not JSON: {failed}") from None
                for key in ("profile", "failure", "component", "recovered_s"):
                    if key not in said:
                        raise SystemExit(f"claims: {path}:{number} has no {key}")
                lines.append(said)
    return lines


# The lake's rows lost when the database's primary fails over, at most, in
# seconds of rows, by profile: none in ha since contract v30's stage E. Its
# commits are local, and its write-ahead stream holds each write until the
# lake has recorded it again (ruled 2026-11-08, superseding the ~5 s first
# ruled that day). A row lost in ha is refused whatever its seconds.
LAKE_RPO_BOUND_S = {"ha": 0}

# What was ruled for a class, written beside its entry: the bound's source,
# and what the bound leaves to later work.
RULED = {
    "order-plugin": (
        "Ruled 2026-11-08: an order plugin lost is detected within 10 s and "
        "recovered within 30 s in ha and 120 s in tolerant."
    ),
}
LAKE_RULED = (
    "Ruled 2026-11-08, built in step 3c's stage E: in ha, lake writes go "
    "through a JetStream stream kept on three servers, and a data plugin's "
    "write is answered only once the stream holds it. The lake's writer "
    "commits each batch with its position in the stream in the same "
    "transaction, and after a database failover resumes from the position "
    "the promoted standby holds, so no acknowledged row is lost "
    "(lake_seconds_bound 0). Each dataset declares how it recovers: history "
    "fetched again through the plugin's backfill, a buffer only, or not "
    "fetchable again; a gap is recorded and read. Tolerant is unchanged."
)


def key_of(said: dict) -> tuple[str, str, str]:
    return (said["profile"], said["failure"], said["component"])


def rpo_of(said: list[dict], profile: str = "") -> dict:
    """What the measurements say of the data lost: the commits acknowledged
    under load and the most lost, where the class wrote under load; the
    archive's bound, where it is one."""
    rpo: dict = {}
    acked = [c["acknowledged_commits"] for c in said if "acknowledged_commits" in c]
    lost = [c["rpo_commits_lost"] for c in said if "rpo_commits_lost" in c]
    if acked:
        rpo["acknowledged_commits_min"] = min(acked)
    if lost:
        rpo["commits_lost_max"] = max(lost)
    # Through the stores (step 3b part 5): the lake's rows lost, which its
    # local commits allow, measured and said, and in ha held to ~5 s of rows
    # (ruled 2026-11-08); anything kept twice, which nothing allows.
    lake = [c["lake_rows_lost"] for c in said if "lake_rows_lost" in c]
    if lake:
        rpo["lake_rows_lost_max"] = max(lake)
        rpo["lake_rpo_s_max"] = max(c.get("lake_rpo_s", 0) for c in said if "lake_rows_lost" in c)
        if profile in LAKE_RPO_BOUND_S:
            rpo["lake_seconds_bound"] = LAKE_RPO_BOUND_S[profile]
    twice = [c["duplicated"] for c in said if "duplicated" in c]
    if twice:
        rpo["duplicated_max"] = max(twice)
    seconds = [c["rpo_s"] for c in said if "rpo_s" in c]
    if seconds:
        rpo["seconds_max"] = max(seconds)
        bounds = {c.get("rpo_bound_s") for c in said if "rpo_s" in c}
        if len(bounds) == 1 and None not in bounds:
            rpo["seconds_bound"] = bounds.pop()
    return rpo


def ledger(claims: list[dict]) -> dict:
    grouped: dict[tuple[str, str, str], list[dict]] = {}
    for said in claims:
        grouped.setdefault(key_of(said), []).append(said)
    entries = []
    for (profile, failure, component), said in sorted(grouped.items()):
        times = sorted(float(c["recovered_s"]) for c in said)
        bounds = {c.get("bound_s") for c in said}
        if len(bounds) != 1:
            raise SystemExit(f"claims: {profile} {failure} {component} was held to more than one bound: {sorted(map(str, bounds))}")
        detected = [float(c["detected_s"]) for c in said if "detected_s" in c]
        entry = {
            "class": failure,
            "profile": profile,
            "component": component,
            "bound_s": bounds.pop(),
            "measured_s": {
                "runs": len(times),
                "min": round(times[0], 2),
                "median": round(statistics.median(times), 2),
                "max": round(times[-1], 2),
            },
            "rpo": rpo_of(said, profile),
            **({"detected_s": {"max": round(max(detected), 2),
                               "bound": next(c.get("detect_bound_s") for c in said if "detected_s" in c)}}
               if detected else {}),
            "runs": sorted({str(c.get("run", "local")) for c in said}),
            "command": said[-1].get("command", ""),
        }
        ruled = [RULED[failure]] if failure in RULED else []
        if "lake_seconds_bound" in entry["rpo"]:
            ruled.append(LAKE_RULED)
        if ruled:
            entry["ruled"] = " ".join(ruled)
        entries.append(entry)
    return {"about": ABOUT, "entries": entries}


def check_ledger(book: dict) -> list[str]:
    missed = []
    for entry in book.get("entries", []):
        name = f"{entry['profile']} {entry['class']} ({entry['component']})"
        bound = entry.get("bound_s")
        worst = entry["measured_s"]["max"]
        if bound is not None and worst > bound:
            missed.append(f"{name}: the ledger's max {worst}s is past its bound {bound}s")
        if "local" in entry.get("runs", []):
            missed.append(f"{name}: the ledger holds a local run's figure; only CI's green runs are written")
        lost = entry.get("rpo", {}).get("commits_lost_max")
        if lost:
            missed.append(f"{name}: the ledger holds {lost} acknowledged commits lost; the bound is none")
        if entry.get("rpo", {}).get("duplicated_max"):
            missed.append(f"{name}: the ledger holds a record kept twice; the bound is none")
        found = entry.get("detected_s") or {}
        if found.get("bound") is not None and found.get("max", 0) > found["bound"]:
            missed.append(f"{name}: detected in {found['max']}s, past its bound {found['bound']}s")
        rpo = entry.get("rpo", {})
        if "seconds_bound" in rpo and rpo.get("seconds_max", 0) > rpo["seconds_bound"]:
            missed.append(f"{name}: an RPO of {rpo['seconds_max']}s past its bound {rpo['seconds_bound']}s")
        if "lake_seconds_bound" in rpo and rpo.get("lake_rpo_s_max", 0) > rpo["lake_seconds_bound"]:
            missed.append(f"{name}: the lake lost {rpo['lake_rpo_s_max']}s of rows, past its bound "
                          f"{rpo['lake_seconds_bound']}s")
        if entry["profile"] in LAKE_RPO_BOUND_S and LAKE_RPO_BOUND_S[entry["profile"]] == 0 \
                and rpo.get("lake_rows_lost_max"):
            missed.append(f"{name}: the lake lost {rpo['lake_rows_lost_max']} acknowledged rows; the bound is none")
    return missed


def check_run(book: dict, claims: list[dict], every: bool = False) -> list[str]:
    missed = []
    measured = {key_of(c) for c in claims}
    profiles = {c["profile"] for c in claims}
    runs = {c.get("command", "").split("E2E_CLUSTER_TARGET=")[-1].split()[0] for c in claims if c.get("command")}
    for said in claims:
        name = f"{said['profile']} {said['failure']} ({said['component']})"
        bound = said.get("bound_s")
        if bound is not None and float(said["recovered_s"]) > bound:
            missed.append(f"{name}: {said['recovered_s']}s, past its bound {bound}s")
        if said.get("rpo_commits_lost"):
            missed.append(f"{name}: {said['rpo_commits_lost']} acknowledged commits lost")
        if said.get("duplicated"):
            missed.append(f"{name}: {said['duplicated']} kept twice")
        if said.get("detect_bound_s") is not None and float(said.get("detected_s", 0)) > said["detect_bound_s"]:
            missed.append(f"{name}: detected in {said['detected_s']}s, past its bound {said['detect_bound_s']}s")
        lake_bound = LAKE_RPO_BOUND_S.get(said["profile"])
        if "lake_rows_lost" in said and lake_bound is not None and float(said.get("lake_rpo_s", 0)) > lake_bound:
            missed.append(f"{name}: the lake lost {said['lake_rpo_s']}s of rows, past its bound {lake_bound}s")
        elif lake_bound == 0 and said.get("lake_rows_lost"):
            missed.append(f"{name}: the lake lost {said['lake_rows_lost']} acknowledged rows; the bound is none")
    for entry in book.get("entries", []):
        key = (entry["profile"], entry["class"], entry["component"])
        target = entry.get("command", "").split("E2E_CLUSTER_TARGET=")[-1].split()[0] if entry.get("command") else ""
        considered = every or (entry["profile"] in profiles and (not runs or target in runs))
        if considered and key not in measured:
            missed.append(f"{entry['profile']} {entry['class']} ({entry['component']}): in the ledger, and this run did not measure it")
    return missed


def table(book: dict, claims: list[dict]) -> str:
    now = {}
    for said in claims:
        now.setdefault(key_of(said), []).append(float(said["recovered_s"]))
    rows = ["| profile | class | component | bound | ledger min / median / max | this run | RPO |",
            "|---|---|---|---|---|---|---|"]
    for entry in book.get("entries", []):
        key = (entry["profile"], entry["class"], entry["component"])
        m = entry["measured_s"]
        this = ", ".join(f"{t:.2f}" for t in now.get(key, [])) or "--"
        rpo = entry.get("rpo") or {}
        said_rpo = ("0 lost of " + str(rpo["acknowledged_commits_min"]) + "+") if "acknowledged_commits_min" in rpo \
            else (f"<= {rpo['seconds_max']}s" if "seconds_max" in rpo else "--")
        bound = "--" if entry.get("bound_s") is None else f"{entry['bound_s']}s"
        rows.append(f"| {entry['profile']} | {entry['class']} | {entry['component']} | {bound} | "
                    f"{m['min']} / {m['median']} / {m['max']} ({m['runs']} runs) | {this} | {said_rpo} |")
    return "\n".join(rows)


def self_test() -> int:
    ha = {"profile": "ha", "failure": "bor-writer", "component": "bor", "bound_s": 10,
          "command": "make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-failover PROFILE=ha FAILURE=bor-writer"}
    db = {"profile": "ha", "failure": "db-primary", "component": "bor", "bound_s": 30,
          "command": "make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-failover PROFILE=ha FAILURE=db-primary"}
    runs = [dict(ha, recovered_s=t, run=r) for t, r in ((7.0, "1"), (4.85, "2"), (7.13, "3"))]
    runs += [dict(db, recovered_s=22.2, run="1", acknowledged_commits=311, rpo_commits_lost=0),
             dict(db, recovered_s=22.8, run="2", acknowledged_commits=122, rpo_commits_lost=0)]
    book = ledger(runs)
    bor = next(e for e in book["entries"] if e["class"] == "bor-writer")
    assert bor["measured_s"] == {"runs": 3, "min": 4.85, "median": 7.0, "max": 7.13}, bor
    assert bor["runs"] == ["1", "2", "3"]
    dbe = next(e for e in book["entries"] if e["class"] == "db-primary")
    assert dbe["rpo"] == {"acknowledged_commits_min": 122, "commits_lost_max": 0}, dbe
    assert check_ledger(book) == []
    # A run measuring everything, within bounds: held.
    assert check_run(book, [dict(ha, recovered_s=6.0), dict(db, recovered_s=23.0, rpo_commits_lost=0)]) == []
    # A class past its bound, a commit lost, a class dropped: each said.
    said = check_run(book, [dict(ha, recovered_s=10.5)])
    assert any("past its bound" in s for s in said) and any("did not measure" in s for s in said), said
    assert any("lost" in s for s in check_run(book, [dict(ha, recovered_s=5), dict(db, recovered_s=20, rpo_commits_lost=2)]))
    # Another profile's run says nothing of this one's classes -- unless the
    # run is CI's whole one, which measures every class the ledger holds.
    assert check_run(book, [dict(ha, profile="tolerant", recovered_s=1.0)]) == []
    assert len(check_run(book, [dict(ha, profile="tolerant", recovered_s=1.0)], every=True)) == 2
    # A ledger holding a figure past its bound, or a local run's, is refused.
    bad = json.loads(json.dumps(book))
    bad["entries"][0]["measured_s"]["max"] = 99
    bad["entries"][1]["runs"] = ["local"]
    assert len(check_ledger(bad)) == 2
    # Two bounds for one class cannot be one entry.
    try:
        ledger([dict(ha, recovered_s=1, run="1"), dict(ha, recovered_s=1, run="2", bound_s=15)])
        raise AssertionError("two bounds made one entry")
    except SystemExit:
        pass
    assert "bor-writer" in table(book, runs)
    # Through the stores: the lake's loss said, a record kept twice or a
    # detection past its bound refused.
    plug = {"profile": "ha", "failure": "order-plugin", "component": "ems-plugin", "bound_s": 30,
            "detect_bound_s": 10, "command": "make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-failover"}
    made = ledger([dict(plug, recovered_s=5.0, detected_s=1.2, duplicated=0, run="1"),
                   dict(db, recovered_s=22.0, run="1", lake_rows_lost=0, lake_rpo_s=0)])
    entry = next(e for e in made["entries"] if e["class"] == "order-plugin")
    assert entry["detected_s"] == {"max": 1.2, "bound": 10} and entry["rpo"]["duplicated_max"] == 0, entry
    assert next(e for e in made["entries"] if e["class"] == "db-primary")["rpo"]["lake_rows_lost_max"] == 0
    assert check_ledger(made) == []
    assert any("twice" in m for m in check_run(made, [dict(plug, recovered_s=5.0, detected_s=1.0, duplicated=1)]))
    assert any("detected" in m for m in check_run(made, [dict(plug, recovered_s=5.0, detected_s=11.0)]))
    # What was ruled written beside the entry; in ha the lake loses no
    # acknowledged row, one row lost refused as surely as many seconds.
    assert "10 s" in entry["ruled"] and "120 s" in entry["ruled"], entry
    lake_entry = next(e for e in made["entries"] if e["class"] == "db-primary")
    assert lake_entry["rpo"]["lake_seconds_bound"] == 0 and "stage E" in lake_entry["ruled"], lake_entry
    assert any("lake lost" in m for m in check_run(made, [dict(db, recovered_s=22.0, lake_rows_lost=40,
                                                                lake_rpo_s=6.2)]))
    assert any("acknowledged rows" in m for m in check_run(made, [dict(db, recovered_s=22.0, lake_rows_lost=1,
                                                                       lake_rpo_s=0)]))
    past = ledger([dict(db, recovered_s=22.0, run="1", lake_rows_lost=40, lake_rpo_s=6.2)])
    assert any("lake lost" in m for m in check_ledger(past)), past
    one = ledger([dict(db, recovered_s=22.0, run="1", lake_rows_lost=1, lake_rpo_s=0)])
    assert any("acknowledged rows" in m for m in check_ledger(one)), one
    print("claims: self-test OK")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    sub = parser.add_subparsers(dest="command")
    made = sub.add_parser("ledger")
    made.add_argument("--out", required=True)
    made.add_argument("files", nargs="+")
    held = sub.add_parser("check")
    held.add_argument("--ledger", required=True)
    held.add_argument("--every", action="store_true",
                      help="every class the ledger holds must be among the files' measurements (CI's whole run)")
    held.add_argument("files", nargs="*")
    asked = parser.parse_args()
    if asked.self_test:
        return self_test()
    if asked.command == "ledger":
        book = ledger(read_claims(asked.files))
        missed = check_ledger(book)
        if missed:
            print("claims: the runs given miss their bounds; nothing written:", file=sys.stderr)
            for line in missed:
                print(f"  - {line}", file=sys.stderr)
            return 1
        with open(asked.out, "w", encoding="utf-8") as kept:
            json.dump(book, kept, indent=2)
            kept.write("\n")
        print(f"claims: {len(book['entries'])} entries written to {asked.out}")
        return 0
    if asked.command == "check":
        try:
            with open(asked.ledger, encoding="utf-8") as kept:
                book = json.load(kept)
        except (OSError, json.JSONDecodeError) as failed:
            print(f"claims: {asked.ledger} could not be read: {failed}", file=sys.stderr)
            return 2
        claims = read_claims(asked.files) if asked.files else []
        missed = check_ledger(book) + (check_run(book, claims, asked.every) if claims or asked.every else [])
        said = table(book, claims)
        if claims:
            print(said)
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with open(summary, "a", encoding="utf-8") as kept:
                kept.write("## The claims ledger, against this run\n\n" + said + "\n")
        if missed:
            print("claims-check FAILED:", file=sys.stderr)
            for line in missed:
                print(f"  - {line}", file=sys.stderr)
            return 1
        print(f"claims-check OK: {len(book.get('entries', []))} entries within their bounds"
              + (f", and this run's {len(claims)} measurements within theirs, none dropped" if claims else ""))
        return 0
    parser.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
