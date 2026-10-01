#!/usr/bin/env python3
"""One clock for the deployment, injected. decisions/024: time is the deployment's.

meridian-core once had five `Clock` traits, one per component, and the bus and
the sidecar read the wall clock directly. Times in one journal came from as
many sources as there were components, and a component that reads the wall
clock cannot be replayed. Nobody argues for a sixth; somebody writes
`SystemTime::now()` because it is the shortest way to a number, and nothing
objects. This objects.

The rules, for code that ships (tests are free to read the wall clock, and
usually have to, to name a scratch database):

  * `SystemTime::now` appears only in crates/clock, inside `SystemClock`.
  * `trait Clock` is defined only in crates/clock.
  * `SystemClock` is named only in crates/clock and in the runtime crate, which
    is the composition root that chooses the clock and hands it to every
    component. A component that constructs one has stopped taking the clock it
    is given.

Test code is a file named tests.rs, anything under a tests/, examples/ or
benches/ directory, and a `#[cfg(test)] mod name { ... }` block.

Usage:  python3 tools/check_one_clock.py [--repo-root .] [--self-test]
Exit:   0 one clock, 1 a second source of time
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys

CLOCK_CRATE = "crates/clock"
COMPOSITION_ROOT = "crates/runtime"

WALL_CLOCK = re.compile(r"\bSystemTime::now\b")
TRAIT = re.compile(r"\btrait\s+Clock\b")
SYSTEM_CLOCK = re.compile(r"\bSystemClock\b")
TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")
LINE_COMMENT = re.compile(r"//[^\n]*")


def is_test_file(relative: pathlib.PurePosixPath) -> bool:
    parts = relative.parts
    return relative.name == "tests.rs" or any(p in ("tests", "examples", "benches") for p in parts)


def shipped(text: str) -> str:
    """The text with every inline test module blanked, line numbers kept."""
    text = LINE_COMMENT.sub(lambda m: " " * len(m.group(0)), text)
    out = list(text)
    for match in TEST_MODULE.finditer(text):
        depth = 1
        at = match.end()
        while at < len(text) and depth:
            if text[at] == "{":
                depth += 1
            elif text[at] == "}":
                depth -= 1
            at += 1
        for i in range(match.start(), at):
            if out[i] != "\n":
                out[i] = " "
    return "".join(out)


def findings(relative: pathlib.PurePosixPath, text: str) -> list[str]:
    path = relative.as_posix()
    if is_test_file(relative) or path.startswith(CLOCK_CRATE + "/"):
        return []
    found = []
    code = shipped(text)
    for number, line in enumerate(code.splitlines(), 1):
        where = f"{path}:{number}"
        if WALL_CLOCK.search(line):
            found.append(f"{where}: reads the wall clock; take the deployment's `Clock` instead")
        if TRAIT.search(line):
            found.append(f"{where}: defines a `Clock` of its own; use meridian_clock::Clock")
        if SYSTEM_CLOCK.search(line) and not path.startswith(COMPOSITION_ROOT + "/"):
            found.append(
                f"{where}: constructs the wall clock; a component takes the clock it is given, "
                "and only the runtime chooses one"
            )
    return found


def scan(root: pathlib.Path) -> tuple[int, list[str]]:
    files = sorted(root.glob("crates/*/**/*.rs"))
    found: list[str] = []
    for file in files:
        relative = pathlib.PurePosixPath(file.relative_to(root).as_posix())
        found.extend(findings(relative, file.read_text(encoding="utf-8")))
    return len(files), found


def self_test() -> None:
    p = pathlib.PurePosixPath
    reads = "fn now() -> i64 { std::time::SystemTime::now(); 0 }\n"
    assert findings(p("crates/street/src/service.rs"), reads), "a component's wall-clock read passed"
    assert findings(p("crates/runtime/src/lib.rs"), reads), "the runtime's own wall-clock read passed"
    assert not findings(p("crates/clock/src/lib.rs"), reads), "the clock itself was refused"
    assert not findings(p("crates/street/tests/postgres.rs"), reads), "a test file was refused"
    assert not findings(p("crates/street/src/x/tests.rs"), reads), "a tests.rs was refused"

    inline = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() { let _ = SystemClock; SystemTime::now(); }\n}\n"
    assert not findings(p("crates/street/src/service.rs"), inline), "an inline test module was refused"
    after = inline + "fn c() { SystemTime::now(); }\n"
    assert findings(p("crates/street/src/service.rs"), after), "code after a test module passed"

    constructs = "fn wire() -> Arc<dyn Clock> { Arc::new(SystemClock) }\n"
    assert findings(p("crates/sidecar/src/service.rs"), constructs), "a component chose its clock"
    assert not findings(p("crates/runtime/src/bin/street.rs"), constructs), "the composition root was refused"

    second = "pub trait Clock: Send + Sync { fn now_ns(&self) -> i64; }\n"
    assert findings(p("crates/street/src/service.rs"), second), "a second Clock trait passed"
    assert not findings(p("crates/street/src/service.rs"), "// SystemTime::now is not read here\n"), (
        "a comment was refused"
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", default=".", type=pathlib.Path)
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="prove the gate fails on a second clock before trusting it to pass",
    )
    args = parser.parse_args()

    if args.self_test:
        self_test()
        print("check-one-clock self-test OK: a second source of time would be caught")
        return 0

    root = args.repo_root.resolve()
    if not (root / CLOCK_CRATE / "src" / "lib.rs").exists():
        sys.exit(f"check-one-clock FAILED: no {CLOCK_CRATE}; has the layout moved?")

    scanned, found = scan(root)
    if found:
        print("check-one-clock FAILED: time read from somewhere other than the deployment's clock", file=sys.stderr)
        for line in found:
            print(f"  {line}", file=sys.stderr)
        return 1

    print(f"check-one-clock OK: {scanned} source files, one clock, chosen by the runtime")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
