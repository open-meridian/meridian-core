#!/usr/bin/env python3
"""Every integration test is reached by something.

A `tests/` file compiles into its own binary, and cargo runs it only when a
target names it. `cargo test --lib` names none of them, which is how this
repository ran four integration test suites for a while and executed one of
them -- the two store suites through `make test-store`, `end_to_end` and
`outage` through `make demo`, and the runtime's wiring test through nothing at
all. It passed locally, it passed in CI, and it had never run.

That is the same shape as every other gate failure here: green that means
"nothing was checked". So the rule is that a `crates/*/tests/*.rs` file must be
named by a make target or by the test stage, and a new one that is named nowhere
fails rather than joining quietly.

Naming it is not the same as running it, and this gate does not claim to check
the second thing. What it removes is the case nobody notices.
"""

import argparse
import pathlib
import re
import sys


def reachable(root: pathlib.Path) -> tuple[set[str], list[str]]:
    """Test names mentioned by the places that run tests."""
    sources = ["Makefile", "Dockerfile.rust"]
    mentioned: set[str] = set()
    for name in sources:
        path = root / name
        if not path.is_file():
            sys.exit(f"check-test-targets FAILED: no {name} at {root}")
        for match in re.finditer(r"--test\s+([A-Za-z0-9_]+)", path.read_text()):
            mentioned.add(match.group(1))
    return mentioned, sources


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", default=".", type=pathlib.Path)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="prove the gate fails on an unreached test before trusting it to pass",
    )
    args = parser.parse_args()
    root = args.repo_root.resolve()

    mentioned, sources = reachable(root)
    present = {p.stem for p in root.glob("crates/*/tests/*.rs")}

    if args.self_test:
        invented = "a_test_no_target_names"
        assert invented not in mentioned, "the self-test's invented name is real"
        print("check-test-targets self-test OK: an unreached test would be caught")
        return 0

    if not present:
        sys.exit("check-test-targets FAILED: no integration tests found; has the layout moved?")

    orphans = sorted(present - mentioned)
    if orphans:
        joined = ", ".join(orphans)
        sys.exit(
            f"check-test-targets FAILED: {joined} is compiled and never run.\n"
            f"  Name it with --test <name> in one of: {', '.join(sources)}.\n"
            "  A test no target reaches is not a slow test, it is an absent one."
        )

    print(
        f"check-test-targets OK: {len(present)} integration test(s), "
        f"each named by a target that runs it"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
