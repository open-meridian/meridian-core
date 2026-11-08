#!/usr/bin/env python3
"""Flaky tests, named in the checks (ruling 1104; step 3b part 4).

CI's test runs are nextest's `ci` profile (.config/nextest.toml), which runs a
failed test once more: one that fails and then passes is kept in its JUnit as
a testcase with a `flakyFailure`, and the run goes on green. This reads every
JUnit file in a directory and names each such test as a warning annotation on
the run and in its summary, so a test that passed and failed on one commit is
seen, never hidden by its retry. A test that failed both times failed the run
already. Standard library only.

Usage: ci-flaky.py <junit directory>       exit 0 always (2 on unreadable XML)
"""
import os
import sys
import xml.etree.ElementTree as ET


def flaky(directory):
    found = []
    for name in sorted(os.listdir(directory)) if os.path.isdir(directory) else []:
        if not name.endswith(".xml"):
            continue
        try:
            root = ET.parse(os.path.join(directory, name)).getroot()
        except ET.ParseError as failed:
            print(f"ci-flaky: {name} is not JUnit: {failed}", file=sys.stderr)
            sys.exit(2)
        for case in root.iter("testcase"):
            failures = case.findall("flakyFailure") + case.findall("flakyError")
            if failures:
                found.append((case.get("classname", ""), case.get("name", ""), len(failures)))
    return found


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    found = flaky(sys.argv[1])
    lines = [f"- `{group} {name}`: failed {times} time(s), then passed on its retry"
             for group, name, times in found]
    for group, name, times in found:
        print(f"::warning title=Flaky test::{group} {name} failed {times} time(s) and then "
              "passed on its retry, on this commit")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as said:
            said.write("### Flaky tests\n\n" + ("\n".join(lines) if lines else "None on this run.") + "\n")
    print(f"ci-flaky: {len(found)} flaky test(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
