#!/usr/bin/env python3
"""CI's two tiers (the PO's ruling of 2026-11-08): the pull request runs a fast
tier, and the cluster jobs the paths it touches select; the merge queue runs
everything, every job that ran before the tiers, and is the only authority
for main.

This file holds what ci.yaml's tiered jobs run, as data -- the stage legs, the
harness groups, the cluster runs -- and decides, for a run, which of them run:

  ci_tier.py plan --event E [--base B --head H]
      key=value lines for $GITHUB_OUTPUT: each job's matrix, as JSON, and
      what the pull request leaves to the queue. Any event but pull_request,
      and a pull request whose changes cannot be read, runs everything.
  ci_tier.py table
      every job, today / pull request (fast tier) / merge queue, as Markdown.
  ci_tier.py check [--workflow .github/workflows/ci.yaml]
      the merge queue's plan runs every stage of ci-local, every harness
      group and every cluster run; every stage of ci-local runs in a tier;
      ci.yaml takes each tiered job's matrix from this file's plan; and the
      aggregate ci-local fails a non-pull-request run where a job did not
      succeed (a skipped job passes a required check, so none may pass the
      queue by being skipped).
  ci_tier.py --self-test

Standard library only, as the gate's other tools.
"""
from __future__ import annotations

import argparse
import fnmatch
import json
import pathlib
import re
import subprocess
import sys

# ── What runs today, every job of it (the merge queue's tier) ──────────────

# ci-local's stage legs after the images and compile jobs (ci.yaml `stages`).
STAGE_LEGS = [
    {"stage": "settings-page", "artifacts": "images settings-binary", "images": "runtime settings-binary"},
    {"stage": "tests dashboards", "artifacts": "images test-images", "images": "runtime tests interop"},
    {"stage": "sdk first-run image", "artifacts": "images", "images": "runtime interop version-check"},
]
# The stages the static, images and compile jobs run, every run.
STAGES_ALWAYS = ["checks", "lint", "images", "compile"]
# The harness's suites, a few a group (ruling 556).
HARNESS_GROUPS = [
    {"group": "lake", "suites": "e2e-lake e2e-prepared e2e-activity"},
    {"group": "orders", "suites": "e2e-orders e2e-access-per-role e2e-plugin-area harness-check"},
    {"group": "tickets", "suites": "e2e-tickets e2e-one-id e2e-service-account e2e-archive"},
]
# The cluster runs (ruling 1066; the k3s window, ruling 935; step 3b).
CLUSTERS = [
    {"target": "e2e-cluster"},
    {"target": "e2e-cluster-external"},
    {"target": "e2e-cluster-ldap"},
    {"target": "e2e-cluster-oidc"},
    {"target": "e2e-cluster-upgrade"},
    {"target": "e2e-cluster-backup"},
    {"target": "e2e-cluster", "k3s": "v1.33.13-k3s2"},
    {"target": "e2e-cluster", "k3s": "v1.34.12-k3s1"},
    {"target": "e2e-cluster", "k3s": "v1.35.9-k3s1"},
    {"target": "e2e-cluster", "k3s": "v1.37.1-k3s1"},
    {"target": "e2e-cluster-scaled", "agents": 3},
    {"target": "e2e-cluster-reconfigure", "agents": 3},
    {"target": "e2e-cluster-failover", "profile": "tolerant"},
    {"target": "e2e-cluster-failover", "profile": "ha", "agents": 3, "timeout": 70},
]


def cluster_name(run: dict) -> str:
    """The job's name, as ci.yaml names it: a required check's name."""
    extra = (f", {run['profile']}" if run.get("profile") else "") + (f", k3s {run['k3s']}" if run.get("k3s") else "")
    return f"e2e-cluster-k3d ({run['target']}{extra})"


# ── The pull request's tier ────────────────────────────────────────────────

# Every pull request: the checks and lint, the images and the compile, the
# unit and integration tests and the dashboards' suites, and one base install.
FAST = {
    "stages": ["tests dashboards"],
    "harness": [],
    "clusters": ["e2e-cluster-k3d (e2e-cluster)"],
}
FAILOVER = ["e2e-cluster-k3d (e2e-cluster-failover, tolerant)", "e2e-cluster-k3d (e2e-cluster-failover, ha)",
            "e2e-cluster-k3d (e2e-cluster-scaled)", "e2e-cluster-k3d (e2e-cluster-reconfigure)"]
ALL_CLUSTERS = [cluster_name(run) for run in CLUSTERS]
# What else a path the pull request touches selects, after the pre-push
# gate's map where it fits (the Makefile's PRE_PUSH_MAP). "full" is every job.
RULES: list[tuple[list[str], dict]] = [
    # CI itself, the build and the dependencies: everything, on the pull request.
    ([".github/*", "tools/ci_tier.py", "tools/ci-*", "tools/gate.py", "tools/free-runner-disk.sh",
      "Makefile", "Dockerfile", "Dockerfile.rust", "Cargo.toml", "Cargo.lock", "deny.toml"], {"full": True}),
    # The chart and the cluster harness: every cluster run.
    (["deploy/chart/*", "e2e/cluster/*", "deploy/database/*", "deploy/backup/*"], {"clusters": ALL_CLUSTERS}),
    # Failover, the stores' takeover and fencing, their migrations.
    (["e2e/failover/*", "crates/changes/*", "crates/runtime/src/member*", "crates/*/src/postgres.rs",
      "crates/*/migrations/*", "deploy/claims-ledger.json", "tools/claims.py"],
     {"clusters": FAILOVER + ["e2e-cluster-k3d (e2e-cluster-upgrade)", "e2e-cluster-k3d (e2e-cluster-backup)"]}),
    (["e2e/backup/*", "crates/runtime/src/backup*", "crates/runtime/src/restore*"],
     {"clusters": ["e2e-cluster-k3d (e2e-cluster-backup)"]}),
    (["crates/first-run/*", "e2e/first-run/*"],
     {"stages": ["sdk first-run image"], "clusters": ALL_CLUSTERS[:6]}),
    # The dashboard: its settings pages, its ways in, the harness's suites.
    (["crates/dashboard/*", "e2e/settings-page/*", "e2e/dashboard/*", "e2e/plugin-page/*"],
     {"stages": ["settings-page"], "harness": ["lake", "orders", "tickets"],
      "clusters": ["e2e-cluster-k3d (e2e-cluster-oidc)", "e2e-cluster-k3d (e2e-cluster-ldap)",
                   "e2e-cluster-k3d (e2e-cluster-external)"]}),
    # The contract and what speaks it: the SDK stage and every harness suite.
    (["proto/*", "codegen/*", "Dockerfile.codegen", "crates/sidecar/*", "crates/domain/*", "crates/bus/*",
      "crates/access/*", "e2e/interop/*", "e2e/book/*", "deploy/harness/*", "deploy/nats/*", "deploy/topics.tsv",
      "deploy/roles.tsv", "crates/analytics-engine/reference_cases/*"],
     {"stages": ["sdk first-run image"], "harness": ["lake", "orders", "tickets"]}),
    # Each store and its suites.
    (["crates/lake/*", "crates/analytics/*", "crates/analytics-engine/*", "e2e/lake/*", "e2e/prepared/*",
      "e2e/activity/*"], {"harness": ["lake"]}),
    (["crates/bor/*", "e2e/orders/*", "e2e/access-per-role/*", "e2e/plugin-area/*"], {"harness": ["orders"]}),
    (["crates/config/*", "crates/conductor/*", "crates/instrument/*", "crates/street/*", "e2e/tickets/*",
      "e2e/one-id/*", "e2e/service-account/*", "e2e/archive/*"], {"harness": ["tickets"]}),
    (["crates/runtime/*", "crates/launcher/*"], {"stages": ["sdk first-run image"],
                                                 "clusters": ALL_CLUSTERS[:6] + FAILOVER}),
]


def changed_files(base: str, head: str) -> list[str] | None:
    done = subprocess.run(["git", "diff", "--name-only", f"{base}...{head}"], capture_output=True, text=True)
    if done.returncode != 0:
        return None
    return [line for line in done.stdout.splitlines() if line.strip()]


def select(paths: list[str] | None) -> dict:
    """What a pull request touching `paths` runs; None reads as everything."""
    if paths is None:
        return {"full": True, "why": ["the pull request's changes could not be read: everything runs"]}
    chosen = {key: set(value) for key, value in FAST.items()}
    why = []
    for globs, adds in RULES:
        hit = sorted({p for p in paths for g in globs if fnmatch.fnmatch(p, g)})
        if not hit:
            continue
        why.append(f"{hit[0]}{' and others' if len(hit) > 1 else ''}: "
                   + ("everything" if adds.get("full") else
                      ", ".join(sorted(set(adds.get("stages", [])) | set(adds.get("harness", []))
                                       | set(adds.get("clusters", []))))))
        if adds.get("full"):
            return {"full": True, "why": why}
        for key in chosen:
            chosen[key] |= set(adds.get(key, []))
    return {"full": False, "why": why, **{k: sorted(v) for k, v in chosen.items()}}


def plan(event: str, paths: list[str] | None) -> dict[str, str]:
    """The outputs a run's tier job writes: each tiered job's matrix and
    what is left to the queue, as JSON."""
    full = event != "pull_request"
    chosen = {"full": True, "why": [f"{event}: everything runs"]} if full else select(paths)
    every = chosen["full"]
    stages = [leg for leg in STAGE_LEGS if every or leg["stage"] in chosen["stages"]]
    harness = [g for g in HARNESS_GROUPS if every or g["group"] in chosen["harness"]]
    clusters = [run for run in CLUSTERS if every or cluster_name(run) in chosen["clusters"]]
    left_harness = [g for g in HARNESS_GROUPS if g not in harness]
    left_clusters = [run for run in CLUSTERS if run not in clusters]
    out = {
        "full": "true" if every else "false",
        "stages": json.dumps({"include": stages}),
        "stage_names": json.dumps([leg["stage"] for leg in stages]),
        "harness": json.dumps({"include": harness}),
        "harness_any": "true" if harness else "false",
        "harness_left": json.dumps({"include": left_harness}),
        "harness_left_any": "true" if left_harness else "false",
        "clusters": json.dumps({"include": clusters}),
        "clusters_left": json.dumps({"include": left_clusters}),
        "clusters_left_any": "true" if left_clusters else "false",
        "why": json.dumps(chosen["why"]),
    }
    return out


# ── The table and the check ────────────────────────────────────────────────

JOBS_UNTIERED = ["ci-local (checks)", "ci-local (lint)", "ci-local (images)", "ci-local (compile)", "ci-local"]


def table() -> str:
    rows = ["| job | today | pull request (fast tier) | merge queue |", "|---|---|---|---|"]
    for name in JOBS_UNTIERED:
        rows.append(f"| {name} | runs | runs | runs |")
    for leg in STAGE_LEGS:
        fast = "runs" if leg["stage"] in FAST["stages"] else "by path"
        rows.append(f"| ci-local ({leg['stage']}) | runs | {fast} | runs |")
    for g in HARNESS_GROUPS:
        rows.append(f"| e2e-harness ({g['group']}) | runs | by path | runs |")
    for name in ALL_CLUSTERS:
        rows.append(f"| {name} | runs | {'runs' if name in FAST['clusters'] else 'by path'} | runs |")
    rows.append("| e2e-cluster-k3d (e2e-cluster, arm64) | runs | queue only | runs |")
    for name in ("release-candidate", "footprint-judge", "claims-check"):
        rows.append(f"| {name} | runs | {'runs' if name == 'footprint-judge' else 'queue only'} | runs |")
    return "\n".join(rows)


def makefile_stages(makefile: pathlib.Path) -> list[str]:
    text = makefile.read_text(encoding="utf-8")
    line = re.search(r"^ci-local:([^\n]*)$", text, re.M)
    return [s.removeprefix("stage-") for s in (line.group(1).split() if line else []) if s.startswith("stage-")]


def check(workflow: pathlib.Path, makefile: pathlib.Path) -> list[str]:
    missed = []
    queue = plan("merge_group", [])
    if queue["full"] != "true":
        missed.append("the merge queue's plan is not every job")
    if json.loads(queue["stages"])["include"] != STAGE_LEGS:
        missed.append("the merge queue's plan leaves a stage leg out")
    if json.loads(queue["harness"])["include"] != HARNESS_GROUPS:
        missed.append("the merge queue's plan leaves a harness group out")
    if json.loads(queue["clusters"])["include"] != CLUSTERS:
        missed.append("the merge queue's plan leaves a cluster run out")
    in_legs = {s for leg in STAGE_LEGS for s in leg["stage"].split()}
    for stage in makefile_stages(makefile):
        if stage != "harness" and stage not in in_legs and stage not in STAGES_ALWAYS:
            missed.append(f"ci-local's stage {stage} runs in neither tier")
    text = workflow.read_text(encoding="utf-8")
    # The stage legs stay a static matrix (ci_mirror_check reads them): the
    # same legs as here, each run or left by the plan.
    legs = re.search(r"^  stages:\n(.*?)\n    steps:", text, re.S | re.M)
    named = re.findall(r"^\s+- stage: (.+?)\s*$", legs.group(1), re.M) if legs else []
    if named != [leg["stage"] for leg in STAGE_LEGS]:
        missed.append(f"ci.yaml's stage legs {named} are not this file's {[leg['stage'] for leg in STAGE_LEGS]}")
    for needle, what in (
        ("contains(fromJSON(needs.tier.outputs.stage_names), matrix.stage)", "each stage leg run or left by the plan"),
        ("matrix: ${{ fromJSON(needs.tier.outputs.harness) }}", "the harness groups' matrix from the plan"),
        ("matrix: ${{ fromJSON(needs.tier.outputs.clusters) }}", "the cluster runs' matrix from the plan"),
        ("tools/ci_tier.py plan", "the tier job running the plan"),
        ("needs.tier.outputs.harness_any != 'false'", "the harness job run unless the plan says none"),
        ("needs.tier.outputs.full != 'false'", "the arm64 run run unless the plan says the pull request's tier"),
        ("every job of the merge queue's run succeeded", "ci-local failing a queue run where a job did not succeed"),
    ):
        if needle not in text:
            missed.append(f"ci.yaml does not hold {what} ({needle})")
    return missed


def self_test() -> int:
    queue = plan("merge_group", None)
    assert queue["full"] == "true" and len(json.loads(queue["clusters"])["include"]) == len(CLUSTERS)
    assert queue["clusters_left_any"] == "false" and queue["harness_left_any"] == "false"
    # A docs-only pull request: the fast tier alone.
    docs = plan("pull_request", ["INSTALL.md"])
    assert docs["full"] == "false"
    assert [leg["stage"] for leg in json.loads(docs["stages"])["include"]] == ["tests dashboards"]
    assert [cluster_name(r) for r in json.loads(docs["clusters"])["include"]] == ["e2e-cluster-k3d (e2e-cluster)"]
    assert docs["harness_any"] == "false" and docs["clusters_left_any"] == "true"
    assert len(json.loads(docs["clusters_left"])["include"]) == len(CLUSTERS) - 1
    # Failover's paths select its runs; CI's own, everything; unreadable, everything.
    fo = plan("pull_request", ["e2e/failover/run.py"])
    names = {cluster_name(r) for r in json.loads(fo["clusters"])["include"]}
    assert set(FAILOVER) <= names and "e2e-cluster-k3d (e2e-cluster-oidc)" not in names
    assert plan("pull_request", [".github/workflows/ci.yaml"])["full"] == "true"
    assert plan("pull_request", None)["full"] == "true"
    assert plan("workflow_dispatch", ["INSTALL.md"])["full"] == "true"
    # Every cluster name a rule names is a cluster run.
    for _, adds in RULES:
        for name in adds.get("clusters", []):
            assert name in ALL_CLUSTERS, name
        for leg in adds.get("stages", []):
            assert any(leg == s["stage"] for s in STAGE_LEGS), leg
    print("ci_tier: self-test OK")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    sub = parser.add_subparsers(dest="command")
    p = sub.add_parser("plan")
    p.add_argument("--event", required=True)
    p.add_argument("--base", default="")
    p.add_argument("--head", default="HEAD")
    sub.add_parser("table")
    c = sub.add_parser("check")
    c.add_argument("--workflow", default=".github/workflows/ci.yaml")
    c.add_argument("--makefile", default="Makefile")
    asked = parser.parse_args()
    if asked.self_test:
        return self_test()
    if asked.command == "plan":
        paths = changed_files(asked.base, asked.head) if asked.event == "pull_request" and asked.base else None
        for key, value in plan(asked.event, paths).items():
            print(f"{key}={value}")
        return 0
    if asked.command == "table":
        print(table())
        return 0
    if asked.command == "check":
        missed = check(pathlib.Path(asked.workflow), pathlib.Path(asked.makefile))
        if missed:
            print("ci-tier-check FAILED:", file=sys.stderr)
            for line in missed:
                print(f"  - {line}", file=sys.stderr)
            return 1
        print(f"ci-tier-check OK: the merge queue runs every stage, harness group and cluster run "
              f"({len(STAGE_LEGS)} legs, {len(HARNESS_GROUPS)} groups, {len(CLUSTERS)} runs); "
              "every ci-local stage runs in a tier")
        return 0
    parser.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
