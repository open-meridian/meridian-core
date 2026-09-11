#!/usr/bin/env python3
"""Nothing reaches another component's store. The bus is the way across.

The kernel holds the ledger and the reference crate holds the replica, each over
its own database. Both are reachable over the bus: commands, queries, events. A
consumer knows those and does not know there is a database, let alone which one
or what shape it is in.

That is not a preference. Storage separation without API separation is
decorative: two stores fuse into one the moment a second component opens a
connection to either, because from then on the schema is the interface and
changing it is everybody's problem. Choosing different technology for one side,
which is most of why they are separate, stops being possible the day something
reads its tables directly.

Nobody argues against this. What happens instead is that somebody adds a
dependency for convenience, it works, and nothing objects. This objects.

The rule: a crate may depend on `meridian-kernel` or `meridian-reference` only
if it is listed in tools/crate-boundary-allowlist.txt with a reason. The
legitimate reason is being a composition root -- a binary that wires a process
together and therefore has to construct the store it hands over.

Usage:  python3 tools/check_crate_boundaries.py [--repo-root .]
Exit:   0 no crossing, 1 an undeclared dependency, 2 malformed input
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

# The crates that own a store. Depending on one means holding its types, which
# means holding its schema.
OWNS_A_STORE = ("meridian-kernel", "meridian-reference")

ALLOWLIST = "tools/crate-boundary-allowlist.txt"

DEPENDENCY = re.compile(r"^\s*(meridian-[a-z-]+)\s*(=|\.)", re.MULTILINE)
SECTION = re.compile(r"^\[([^\]]+)\]\s*$", re.MULTILINE)


def dependencies(manifest: pathlib.Path) -> set[str]:
    """Every meridian crate this one depends on, in any dependency section."""
    text = manifest.read_text(encoding="utf-8")
    found: set[str] = set()

    for match in SECTION.finditer(text):
        if "dependencies" not in match.group(1):
            continue
        start = match.end()
        following = SECTION.search(text, start)
        body = text[start : following.start() if following else len(text)]
        found.update(DEPENDENCY.findall(body) and {m[0] for m in DEPENDENCY.findall(body)})

    return found


def allowed(root: pathlib.Path) -> dict[tuple[str, str], str]:
    path = root / ALLOWLIST
    if not path.exists():
        return {}

    entries: dict[tuple[str, str], str] = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) < 2 or not parts[1].strip():
            print(f"{ALLOWLIST}:{number}: an entry with no reason is not an entry", file=sys.stderr)
            raise SystemExit(2)
        pair = parts[0].split("->")
        if len(pair) != 2:
            print(f"{ALLOWLIST}:{number}: expected 'crate->dependency', got {parts[0]!r}", file=sys.stderr)
            raise SystemExit(2)
        entries[(pair[0].strip(), pair[1].strip())] = parts[1].strip()
    return entries


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", default=".")
    args = parser.parse_args()

    root = pathlib.Path(args.repo_root)
    permitted = allowed(root)

    crossings: list[str] = []
    checked = 0

    for manifest in sorted(root.glob("crates/*/Cargo.toml")):
        crate = manifest.parent.name
        name = f"meridian-{crate}"
        checked += 1

        for dependency in sorted(dependencies(manifest)):
            if dependency not in OWNS_A_STORE or dependency == name:
                continue
            if (name, dependency) in permitted:
                continue
            crossings.append(f"{name} depends on {dependency}")

    if crossings:
        print(f"check-crate-boundaries FAILED: {len(crossings)} undeclared dependency(ies)")
        for crossing in crossings:
            print(f"  {crossing}")
        print(
            "\nA store is reached over the bus, not by linking against it. If this crate"
            "\nis a composition root and has to construct one, say so in"
            f"\n{ALLOWLIST}."
        )
        return 1

    print(
        f"check-crate-boundaries OK: {checked} crate(s), "
        f"{len(permitted)} declared crossing(s), no undeclared ones"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
