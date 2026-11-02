#!/usr/bin/env python3
"""Every CI job must be reproducible locally, or carry a written justification.

The keystone gate. It is the one mechanism that keeps `make ci-local` from
drifting away from what CI actually enforces. Without it, "green locally" stops
meaning anything and every push becomes a remote debugging session.

The rule: for each job in .github/workflows/*.y[a]ml, either

  * a `make` target of the same name exists and is reachable from `ci-local`, or
  * the job runs stages of `ci-local`, named by `stage:` in its matrix, each a
    `stage-<name>` that `ci-local` runs (ruling 1069, 443), or
  * the job name appears in tools/ci-mirror-allowlist.txt with a reason.

And the other way round: every stage `ci-local` runs is run by exactly one CI
job, so splitting the gate into jobs never leaves a stage behind. The harness
stage is the exception, its suites being the e2e-harness jobs' (ruling 556),
which the Makefile's own ci-mirror-check holds to HARNESS_SUITES.

Run as a make target AND as a CI job, so neither side can drift unobserved.

Usage:  python3 tools/ci_mirror_check.py [--repo-root .]
Exit:   0 all mirrored, 1 unmirrored jobs found, 2 malformed input
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

WORKFLOW_GLOBS = ("*.yaml", "*.yml")
ALLOWLIST = "tools/ci-mirror-allowlist.txt"
# Run by the e2e-harness jobs, suite by suite, rather than by a stage job.
STAGES_ELSEWHERE = {"harness"}


def workflow_jobs(workflows_dir: pathlib.Path) -> dict[str, list[str]]:
    """Map job id -> [workflow files it appears in].

    Deliberately a line scanner rather than a YAML parse: the check must run
    with no third-party dependency, in any repo, including ones with no Python
    toolchain configured yet.
    """
    jobs: dict[str, list[str]] = {}
    for pattern in WORKFLOW_GLOBS:
        for wf in sorted(workflows_dir.glob(pattern)):
            in_jobs = False
            for line in wf.read_text(encoding="utf-8").splitlines():
                if re.match(r"^jobs:\s*$", line):
                    in_jobs = True
                    continue
                if in_jobs and re.match(r"^\S", line):
                    in_jobs = False  # dedented back to a top-level key
                if in_jobs:
                    m = re.match(r"^  ([A-Za-z0-9_.-]+):\s*$", line)
                    if m:
                        jobs.setdefault(m.group(1), []).append(wf.name)
    return jobs


RUNS_A_STAGE = "make -k ci-stage STAGE="


def workflow_stages(workflows_dir: pathlib.Path) -> dict[str, list[str]]:
    """Map job id -> the ci-local stages its matrix names, as `stage: x`,
    `- stage: x y` (one leg, several stages) or `stage: [x, y]`. A job naming stages that never runs
    `make -k ci-stage STAGE=...` runs none of them, and names none here."""
    stages: dict[str, list[str]] = {}
    runs: set[str] = set()
    for pattern in WORKFLOW_GLOBS:
        for wf in sorted(workflows_dir.glob(pattern)):
            in_jobs, job = False, None
            for line in wf.read_text(encoding="utf-8").splitlines():
                if re.match(r"^jobs:\s*$", line):
                    in_jobs = True
                    continue
                if in_jobs and re.match(r"^\S", line):
                    in_jobs, job = False, None
                if not in_jobs:
                    continue
                m = re.match(r"^  ([A-Za-z0-9_.-]+):\s*$", line)
                if m:
                    job = m.group(1)
                    continue
                if job and RUNS_A_STAGE in line:
                    runs.add(job)
                m = re.match(r"^\s+(?:-\s+)?stage:\s*(.+?)\s*$", line)
                if m and job:
                    value = m.group(1)
                    # A list is a leg each; one leg's value may name several
                    # stages, which it runs in order.
                    names = (value.strip("[]").split(",") if value.startswith("[") else value.split())
                    stages.setdefault(job, []).extend(n.strip() for n in names if n.strip())
    return {job: names for job, names in stages.items() if job in runs}


def ci_local_stages(makefile: pathlib.Path) -> list[str]:
    """The stages ci-local runs, by name, from its own prerequisites."""
    for line in makefile.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^ci-local\s*:(?!=)\s*(.*)$", line)
        if m:
            return [t.removeprefix("stage-") for t in m.group(1).split() if t.startswith("stage-")]
    return []


def make_targets(makefile: pathlib.Path) -> set[str]:
    if not makefile.exists():
        return set()
    targets = set()
    for line in makefile.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^([A-Za-z0-9_][A-Za-z0-9_.-]*)\s*:(?!=)", line)
        if m:
            targets.add(m.group(1))
        m = re.match(r"^\.PHONY:\s*(.+)$", line)
        if m:
            targets.update(m.group(1).split())
    return targets


def ci_local_closure(makefile: pathlib.Path) -> set[str]:
    """Targets reachable from ci-local, following prerequisite lists."""
    if not makefile.exists():
        return set()
    prereqs: dict[str, list[str]] = {}
    for line in makefile.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^([A-Za-z0-9_][A-Za-z0-9_.-]*)\s*:(?!=)\s*(.*)$", line)
        if m:
            prereqs.setdefault(m.group(1), []).extend(m.group(2).split())

    seen: set[str] = set()
    stack = ["ci-local"]
    while stack:
        target = stack.pop()
        if target in seen:
            continue
        seen.add(target)
        stack.extend(prereqs.get(target, []))
    return seen


def read_allowlist(path: pathlib.Path) -> dict[str, str]:
    """job-id<TAB>reason, one per line. A bare job id is not enough."""
    allowed: dict[str, str] = {}
    if not path.exists():
        return allowed
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        job, _, reason = line.partition("\t")
        allowed[job.strip()] = reason.strip()
    return allowed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", default=".")
    args = parser.parse_args()

    root = pathlib.Path(args.repo_root).resolve()
    workflows_dir = root / ".github" / "workflows"
    makefile = root / "Makefile"

    if not workflows_dir.is_dir():
        print(f"ci-mirror-check: no {workflows_dir.relative_to(root)}, nothing to mirror")
        return 0

    jobs = workflow_jobs(workflows_dir)
    if not jobs:
        print("ci-mirror-check: no jobs found in workflows")
        return 0

    targets = make_targets(makefile)
    reachable = ci_local_closure(makefile)
    allowed = read_allowlist(root / ALLOWLIST)
    staged = workflow_stages(workflows_dir)
    local_stages = ci_local_stages(makefile)

    unmirrored: list[tuple[str, str, str]] = []
    unreachable: list[tuple[str, str]] = []
    for job, files in sorted(jobs.items()):
        where = ", ".join(sorted(set(files)))
        if job in staged:
            for stage in staged[job]:
                if stage not in local_stages:
                    unmirrored.append((job, where, f"runs stage {stage!r}, which ci-local does not"))
            continue
        if job in allowed:
            if not allowed[job]:
                unmirrored.append((job, where, "allowlisted with no reason given"))
            continue
        if job not in targets:
            unmirrored.append((job, where, "no matching make target"))
        elif job not in reachable:
            unreachable.append((job, where))

    for job, where, why in unmirrored:
        print(f"UNMIRRORED  {job}  ({where}): {why}", file=sys.stderr)
    for job, where in unreachable:
        print(f"UNREACHABLE {job}  ({where}): target exists but ci-local does not reach it",
              file=sys.stderr)

    # Every stage of ci-local in exactly one CI job.
    stage_faults = []
    for stage in local_stages:
        jobs_running = [job for job, names in staged.items() for n in names if n == stage]
        if stage in STAGES_ELSEWHERE:
            if jobs_running:
                stage_faults.append(f"stage {stage!r} is run by its own CI jobs, not a stage job: {jobs_running}")
        elif len(jobs_running) != 1:
            stage_faults.append(f"stage {stage!r} is run by {len(jobs_running)} CI jobs {jobs_running}; "
                                f"by exactly one")
    for fault in stage_faults:
        print(f"UNRUN       {fault}", file=sys.stderr)

    if unmirrored or unreachable or stage_faults:
        print(
            f"\nci-mirror-check FAILED: {len(unmirrored) + len(unreachable)} of {len(jobs)} "
            f"CI jobs are not reproducible by `make ci-local`, and {len(stage_faults)} of its "
            f"{len(local_stages)} stages are not run once in CI.\n"
            f"Add the target, wire it into ci-local, or record a reason in {ALLOWLIST}.",
            file=sys.stderr,
        )
        return 1

    print(f"ci-mirror-check OK: {len(jobs)} CI jobs, all reachable from ci-local; "
          f"its {len(local_stages)} stages each run by one CI job"
          f"{', the harness by the e2e-harness jobs' if STAGES_ELSEWHERE & set(local_stages) else ''}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
