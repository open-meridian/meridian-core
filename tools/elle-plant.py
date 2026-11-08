#!/usr/bin/env python3
"""A planted anomaly for the Elle check (step 3b part 5): a copy of the write
driver's history (e2e/failover/driver.py) with one read reordered, which
Elle must refuse, so a green check is known to be able to fail.

The read chosen is the latest successful read holding two values whose
appends did not overlap in time: the first acknowledged before the second
was asked. Swapped in that read, the second value stands before the first
in the list, which no serial order of the appends allows: under
strict-serializable the appends' real-time order and the read's order form
a cycle (Adya's G0, real time), and the other reads holding both disagree
with it (Elle's incompatible order).

Usage: elle-plant.py HISTORY PLANTED
Exit: 0 written, 2 no read could be reordered (the history is too thin to
prove anything with).
"""
from __future__ import annotations

import re
import sys

OP = re.compile(r"^\{:type :(?P<type>\w+), :f :txn, :value \[\[(?P<micro>.*)\]\], :process (?P<process>-?\d+), "
                r":time (?P<time>\d+), :index (?P<index>\d+)\}$")
APPEND = re.compile(r'^:append "(?P<key>[^"]*)" (?P<value>\d+)$')
READ = re.compile(r'^:r "(?P<key>[^"]*)" \[(?P<values>[\d ]*)\]$')


def plant(lines: list[str]) -> tuple[list[str], str] | None:
    invoked: dict[int, int] = {}  # value -> index of its invoke
    acked: dict[int, int] = {}  # value -> index of its ok
    reads: list[tuple[int, str, list[int]]] = []  # (line number, key, values)
    for number, line in enumerate(lines):
        said = OP.match(line.strip())
        if not said:
            continue
        index = int(said["index"])
        append = APPEND.match(said["micro"])
        if append:
            value = int(append["value"])
            if said["type"] == "invoke":
                invoked[value] = index
            elif said["type"] == "ok":
                acked[value] = index
            continue
        read = READ.match(said["micro"])
        if read and said["type"] == "ok":
            values = [int(v) for v in read["values"].split()]
            if len(values) >= 2:
                reads.append((number, read["key"], values))
    for number, key, values in reversed(reads):
        for at in range(len(values) - 1):
            first, second = values[at], values[at + 1]
            if first in acked and second in invoked and acked[first] < invoked[second]:
                swapped = values[:at] + [second, first] + values[at + 2:]
                listed = " ".join(str(v) for v in swapped)
                planted = list(lines)
                planted[number] = re.sub(r'\[\[:r "([^"]*)" \[[\d ]*\]\]\]', lambda m: f'[[:r "{m.group(1)}" [{listed}]]]',
                                         lines[number], count=1)
                return planted, f"the read at line {number + 1} of {key}: {first} and {second} swapped"
    return None


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    with open(sys.argv[1], encoding="utf-8") as held:
        lines = held.read().splitlines()
    done = plant(lines)
    if done is None:
        print("elle-plant: no read holds two values whose appends did not overlap; nothing planted", file=sys.stderr)
        return 2
    planted, said = done
    with open(sys.argv[2], "w", encoding="utf-8") as kept:
        kept.write("\n".join(planted) + "\n")
    print(f"elle-plant: {said}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
