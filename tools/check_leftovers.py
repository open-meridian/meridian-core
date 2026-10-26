#!/usr/bin/env python3
"""Fail the gate on containers a core run left behind (H1, ruling 586).

Core's e2e stacks meet on one shared Docker network, `meridian`, by their
compose service names: `fake-idp`, `postgres`, `dashboard`. A container one
checkout's run left there answers to the same name as the next run's, from any
other checkout, and that run then talks to a stranger: on 2026-10-26 a
`core-c1-fake-idp-1` left by one worktree's gate broke another's
`e2e-dashboard-oidc` half an hour in.

So the gate looks before it starts and again as it ends, and fails naming
each container, its compose project and the command that takes it down:

  * every container on the `meridian` network whose compose working
    directory is a meridian-core checkout, or no longer exists (an orphan
    nobody can take down by its file any more);
  * at the end, also every container of this checkout's own compose project,
    on any network: core's e2e removes what it made.

A container on the network from another repository's stack -- the platform's
own development site, say -- is not core's and is left alone. Nothing here
removes anything: taking a stack down is its owner's act.

Usage:  python3 tools/check_leftovers.py [--when start|end] [--repo-root .]
Exit:   0 nothing left, 1 leftovers found, 2 docker could not be asked
"""

from __future__ import annotations

import argparse
import pathlib
import subprocess
import sys

NETWORK = "meridian"
FIELDS = (
    '{{.Names}}\t{{.Status}}\t{{.Label "com.docker.compose.project"}}'
    '\t{{.Label "com.docker.compose.project.working_dir"}}'
)


def containers(*filters: str) -> list[tuple[str, str, str, str]]:
    """Each container the filters match: name, status, project, working dir."""
    argv = ["docker", "ps", "-a", "--format", FIELDS]
    for one in filters:
        argv += ["--filter", one]
    done = subprocess.run(argv, capture_output=True, text=True, check=False)
    if done.returncode != 0:
        raise RuntimeError(done.stderr.strip() or "docker ps failed")
    rows = []
    for line in done.stdout.splitlines():
        parts = (line.split("\t") + ["", "", "", ""])[:4]
        if parts[0]:
            rows.append((parts[0], parts[1], parts[2], parts[3]))
    return rows


def is_core_checkout(working_dir: str) -> bool:
    """A meridian-core checkout, or a directory that is gone."""
    if not working_dir:
        return False
    here = pathlib.Path(working_dir)
    if not here.exists():
        return True
    return (here / "crates" / "sidecar" / "Cargo.toml").is_file()


def leftovers(when: str, project: str) -> list[tuple[str, str, str, str]]:
    found = {
        row[0]: row
        for row in containers(f"network={NETWORK}")
        if is_core_checkout(row[3])
    }
    if when == "end":
        for row in containers(f"label=com.docker.compose.project={project}"):
            found.setdefault(row[0], row)
    return sorted(found.values())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--when", choices=("start", "end"), default="start")
    parser.add_argument("--repo-root", default=".")
    args = parser.parse_args()
    root = pathlib.Path(args.repo_root).resolve()
    # Compose names a project after its file's directory, lower-cased.
    project = root.name.lower()
    try:
        left = leftovers(args.when, project)
    except (OSError, RuntimeError) as failed:
        print(f"check-leftovers FAILED: docker could not be asked: {failed}", file=sys.stderr)
        return 2
    if not left:
        said = "before the gate" if args.when == "start" else "after the gate"
        print(f"check-leftovers OK: no core stack is left on the {NETWORK} network {said}")
        return 0
    print(
        f"check-leftovers FAILED: {len(left)} container(s) a core run left behind, "
        f"which a run's service names on the {NETWORK} network would reach instead of its own:",
        file=sys.stderr,
    )
    for name, status, owner, working_dir in left:
        where = working_dir or "no compose file"
        print(f"  {name} ({status}; project {owner or '-'}; {where})", file=sys.stderr)
    projects = sorted({owner for _, _, owner, _ in left if owner})
    for owner in projects:
        print(
            f"  take it down: docker compose -p {owner} down --remove-orphans",
            file=sys.stderr,
        )
    print("  (its owner's act; this check removes nothing)", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
