#!/usr/bin/env python3
"""Contract-tier changes must declare themselves, at the commit boundary.

The PreToolUse guard asks a session's intent before it writes. That is the right
place to catch an honest mistake and the wrong place to rely on, for two reasons
this project found the hard way:

  * it is registered per session root, so a session started one directory up
    loads no hooks at all (design/contract-guard-session-root-coverage), and
  * its matcher covers the file-editing tools, so a heredoc, `sed -i` or a
    `python3 -` script writes anywhere unobserved
    (design/contract-guard-bash-bypass).

Both are the same shape: a control everyone believes is holding. This gate is
placed where neither applies. It reads what actually changed on disk, in commits
that already exist, so the tool that made the change and the directory the
session started in are both irrelevant.

Two tiers, because two different things are being protected.

DERIVATION -- workflows, the matrix, the topic registry, fixtures, decisions and
the proto schema. These are the chain the whole project exists to keep pointing
one way. A commit touching them must name a queued task in a trailer:

    Contract-Revision: sdk-contract/sidecar-protocol

The task must exist in meridian-design/tasks/. That is the escalation route the
guard's own refusal text describes, made checkable.

GUARDRAIL -- CI workflows, the gates themselves, the pre-push hook and the agent
configuration. Weakening these is how every other gate stops meaning anything. A
commit touching them must say why, in a trailer:

    Guardrail-Change: adds the review job's allowlist entry

Free text, deliberately: the reason is for the human reading the review, and the
review policy already says these are read line by line, every time. What the
trailer removes is the possibility of it happening silently.

Usage:  python3 tools/check_contract_diff.py [--base REF] [--repo-root .] [--self-test]
Exit:   0 clean, 1 an undeclared contract-tier change, 2 could not determine a range
"""

from __future__ import annotations

import argparse
import os
import pathlib
import re
import subprocess
import sys

# Repo-relative, applied when the repository's directory name matches.
DERIVATION_BY_REPO = {
    "meridian-design": [r"^workflows/", r"^matrix/", r"^topics/",
                        r"^fixtures/", r"^decisions/"],
    "meridian-schema": [r"^proto/"],
}

# Applied in every repository.
GUARDRAIL_ANY = [
    r"^\.github/workflows/",
    r"^hooks/pre-push$",
    r"^\.claude/settings\.json$",
    r"^\.claude/hooks/",
    r"^tools/",
]

REVISION_TRAILER = re.compile(r"^Contract-Revision:\s*(\S+)\s*$", re.M)
GUARDRAIL_TRAILER = re.compile(r"^Guardrail-Change:\s*(\S.*?)\s*$", re.M)


def classify(path: str, repo_name: str) -> str | None:
    for pattern in DERIVATION_BY_REPO.get(repo_name, []):
        if re.search(pattern, path):
            return "derivation"
    for pattern in GUARDRAIL_ANY:
        if re.search(pattern, path):
            return "guardrail"
    return None


def git(root: pathlib.Path, *args: str) -> str:
    proc = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True)
    if proc.returncode != 0:
        return ""
    return proc.stdout.strip()


def determine_base(root: pathlib.Path, explicit: str | None) -> str | None:
    """The ref to compare against, or None when there is no honest answer.

    CI checkouts are shallow and detached by default: no upstream, no
    origin/main, nothing to compare. That is why CONTRACT_DIFF_BASE exists and
    why None is a failure rather than a pass -- see main().
    """
    for candidate in (explicit, os.environ.get("CONTRACT_DIFF_BASE")):
        if candidate:
            if not git(root, "rev-parse", "--verify", "--quiet", f"{candidate}^{{commit}}"):
                return None
            return candidate
    upstream = git(root, "rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}")
    if upstream:
        return upstream
    for candidate in ("origin/main", "origin/master"):
        if git(root, "rev-parse", "--verify", "--quiet", candidate):
            return candidate
    return None


def commits_in_range(root: pathlib.Path, base: str) -> list[str]:
    out = git(root, "rev-list", f"{base}..HEAD")
    return out.splitlines() if out else []


def check_commit(root: pathlib.Path, sha: str, repo_name: str) -> list[str]:
    files = git(root, "show", "--name-only", "--pretty=format:", sha).splitlines()
    tiers = {}
    for path in (f.strip() for f in files if f.strip()):
        tier = classify(path, repo_name)
        if tier:
            tiers.setdefault(tier, []).append(path)
    if not tiers:
        return []

    message = git(root, "show", "-s", "--format=%B", sha)
    subject = git(root, "show", "-s", "--format=%s", sha)
    problems = []

    if "derivation" in tiers:
        match = REVISION_TRAILER.search(message)
        if not match:
            problems.append(
                f"{sha[:9]} {subject}\n"
                f"    touches the derivation chain:\n"
                + "".join(f"      {p}\n" for p in tiers["derivation"])
                + "    and carries no 'Contract-Revision: <task-id>' trailer.\n"
                "    Queue the task, then name it. Editing the contract so an\n"
                "    implementation passes is the failure this project restarted over."
            )
        else:
            task = match.group(1)
            task_file = root / "tasks" / f"{task}.md"
            if repo_name != "meridian-design":
                task_file = root.parent / "meridian-design" / "tasks" / f"{task}.md"
            if not task_file.exists():
                problems.append(
                    f"{sha[:9]} {subject}\n"
                    f"    names Contract-Revision: {task}\n"
                    f"    but no such task exists at {task_file}.\n"
                    "    A trailer pointing at nothing is worse than none: it reads\n"
                    "    as authorisation to anyone skimming."
                )

    if "guardrail" in tiers and not GUARDRAIL_TRAILER.search(message):
        problems.append(
            f"{sha[:9]} {subject}\n"
            f"    changes a guardrail:\n"
            + "".join(f"      {p}\n" for p in tiers["guardrail"])
            + "    and carries no 'Guardrail-Change: <why>' trailer.\n"
            "    Say why in one line. Weakening a gate quietly is how every other\n"
            "    gate stops meaning anything."
        )
    return problems


def self_test() -> int:
    problems = []
    cases = [
        ("workflows/W1-x.workflow.yaml", "meridian-design", "derivation"),
        ("matrix/matrix.tsv", "meridian-design", "derivation"),
        ("topics/topics.md", "meridian-design", "derivation"),
        ("fixtures/holdings/x.yaml", "meridian-design", "derivation"),
        ("decisions/001-x.md", "meridian-design", "derivation"),
        ("proto/meridian/v1/holdings.proto", "meridian-schema", "derivation"),
        # The same path is not derivation-tier in a repo that does not own it.
        ("proto/meridian/v1/holdings.proto", "meridian-core", None),
        ("workflows/W1-x.workflow.yaml", "meridian-core", None),
        (".github/workflows/ci.yaml", "meridian-core", "guardrail"),
        (".github/workflows/ci.yaml", "meridian-design", "guardrail"),
        ("hooks/pre-push", "meridian-python", "guardrail"),
        (".claude/settings.json", "meridian-design", "guardrail"),
        ("tools/check_derivation.py", "meridian-design", "guardrail"),
        ("crates/bus/src/lib.rs", "meridian-core", None),
        ("README.md", "meridian-design", None),
        ("tasks/design/x.md", "meridian-design", None),
        ("intent/x.md", "meridian-design", None),
    ]
    for path, repo, want in cases:
        got = classify(path, repo)
        if got != want:
            problems.append(f"classify({path!r}, {repo!r}) = {got!r}, expected {want!r}")

    trailer_cases = [
        ("subject\n\nbody\n\nContract-Revision: design/x\n", REVISION_TRAILER, "design/x"),
        # No space after the colon is what git's own trailer parsing accepts,
        # so it is accepted here. Rejecting it would be friction with no gain.
        ("subject\n\nContract-Revision:design/x\n", REVISION_TRAILER, "design/x"),
        ("subject\n\nno trailer here\n", REVISION_TRAILER, None),
        ("subject\n\nGuardrail-Change: because the job needed it\n",
         GUARDRAIL_TRAILER, "because the job needed it"),
        ("subject\n\nGuardrail-Change:\n", GUARDRAIL_TRAILER, None),
        # Prose mentioning the word must not count as a declaration.
        ("subject\n\nI considered a Contract-Revision: but did not\n",
         REVISION_TRAILER, None),
    ]
    for message, pattern, want in trailer_cases:
        match = pattern.search(message)
        got = match.group(1) if match else None
        if got != want:
            problems.append(f"trailer in {message!r} = {got!r}, expected {want!r}")

    for p in problems:
        print(f"  {p}", file=sys.stderr)
    if problems:
        print(f"check-contract-diff self-test FAILED: {len(problems)} problem(s)",
              file=sys.stderr)
        return 1
    print(f"check-contract-diff self-test OK: "
          f"{len(cases)} classifications, {len(trailer_cases)} trailer cases")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", default=".")
    parser.add_argument("--base")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    root = pathlib.Path(args.repo_root).resolve()
    repo_name = root.name

    base = determine_base(root, args.base)
    if base is None:
        print(
            "check-contract-diff FAILED: no base to compare against.\n\n"
            "Tried, in order: --base, CONTRACT_DIFF_BASE, the branch's upstream,\n"
            "origin/main, origin/master. None resolved to a commit.\n\n"
            "This is a failure and not a pass. An earlier version returned 0 here,\n"
            "and in CI -- a shallow detached checkout with no origin/main -- that\n"
            "meant the gate reported success having inspected nothing. A gate that\n"
            "cannot see the range it is meant to police must say so.\n\n"
            "In CI, set CONTRACT_DIFF_BASE and check out enough history to resolve\n"
            "it. Locally, set an upstream or fetch origin.",
            file=sys.stderr)
        return 2

    commits = commits_in_range(root, base)
    if not commits:
        print(f"check-contract-diff OK: no commits ahead of {base}")
        return 0

    problems = []
    for sha in commits:
        problems.extend(check_commit(root, sha, repo_name))

    for problem in problems:
        print(f"\n{problem}", file=sys.stderr)

    if problems:
        print(f"\ncheck-contract-diff FAILED: {len(problems)} undeclared "
              f"contract-tier change(s) in {len(commits)} commit(s) ahead of {base}.\n"
              f"Amend the commit message with the trailer it needs. This gate reads\n"
              f"what changed on disk, so no tool or session root avoids it.",
              file=sys.stderr)
        return 1

    print(f"check-contract-diff OK: {len(commits)} commit(s) ahead of {base}, "
          f"contract-tier changes declared")
    return 0


if __name__ == "__main__":
    sys.exit(main())
