#!/usr/bin/env python3
"""The desktop footprint (ruling 1058; spec/a-deployment-scales-out-and-fails-over-
by-configuration section 13): the default profile's pods, memory and CPU,
against v29's baseline in deploy/footprint-baseline.yaml, failing past its
limits -- no new pod, memory requested and at peak each at most 15% over,
CPU requested at most 15% over, disk at most 10% over.

What a chart asks for is read from what it renders (`render`), the same on
every machine: the pods the default profile runs (each workload's replicas,
no plugin, no Job) and the memory and CPU each requests and is limited to.
What a deployment holds at its peak -- memory, and the disk its record takes
-- is measured where it runs (`measure`, in a cluster run), and held to the
baseline's figures where the baseline has them.

Usage:
  footprint.py render < rendered.yaml          the default profile's figures, as YAML
  footprint.py check --baseline B < rendered.yaml   the rendered chart held to B's limits
  footprint.py measured --baseline B --peak-mib N --disk-mib N   a run's figures held to B's
  footprint.py --self-test
Exit: 0 within the limits, 1 past one, 2 unreadable input.
"""
from __future__ import annotations

import argparse
import re
import sys

# The workloads the default profile runs: every Deployment and StatefulSet
# the chart renders, at its replicas. A Job runs to its end and is no pod of
# the running deployment; a plugin is launched later and is not the core's.
WORKLOADS = ("Deployment", "StatefulSet", "DaemonSet")

LIMITS = {
    "pods": 0.0,
    "memory_requested_mib": 0.15,
    "memory_peak_mib": 0.15,
    "cpu_requested_millicores": 0.15,
    "disk_mib": 0.10,
}


def documents(text: str) -> list[str]:
    return [d for d in re.split(r"^---\s*$", text, flags=re.M) if d.strip()]


def quantity_mib(value: str) -> float:
    value = value.strip().strip('"')
    units = {"Ki": 1 / 1024, "Mi": 1, "Gi": 1024, "Ti": 1024 * 1024,
             "K": 1000 / 1048576, "M": 1e6 / 1048576, "G": 1e9 / 1048576}
    for unit, factor in units.items():
        if value.endswith(unit):
            return float(value[: -len(unit)]) * factor
    return float(value) / 1048576


def quantity_millicores(value: str) -> float:
    value = value.strip().strip('"')
    if value.endswith("m"):
        return float(value[:-1])
    return float(value) * 1000


def workload(doc: str) -> tuple[str, str, int, list[dict]] | None:
    kind = re.search(r"^kind:\s*(\S+)", doc, flags=re.M)
    if not kind or kind.group(1) not in WORKLOADS:
        return None
    name = re.search(r"^metadata:\s*\n(?:\s+.*\n)*?\s+name:\s*(\S+)", doc, flags=re.M)
    replicas = re.search(r"^  replicas:\s*(\d+)", doc, flags=re.M)
    count = int(replicas.group(1)) if replicas else 1
    # Each container's resources, read by indentation: a block under
    # `resources:` with `requests:` and `limits:` maps.
    containers = []
    for block in re.finditer(r"^(\s+)resources:\s*\n((?:\1\s+.*\n)+)", doc, flags=re.M):
        body = block.group(2)
        figures = {}
        for part in ("requests", "limits"):
            found = re.search(rf"^\s+{part}:\s*\n((?:\s+(?:memory|cpu):.*\n)+)", body, flags=re.M)
            if not found:
                continue
            for line in found.group(1).splitlines():
                key, _, value = line.strip().partition(":")
                figures[f"{part}.{key.strip()}"] = value.strip()
        containers.append(figures)
    return kind.group(1), name.group(1) if name else "?", count, containers


def render(text: str) -> dict:
    pods = 0
    memory_requested = 0.0
    memory_limited = 0.0
    cpu_requested = 0.0
    named = []
    for doc in documents(text):
        found = workload(doc)
        if not found:
            continue
        kind, name, count, containers = found
        if count == 0:
            continue
        pods += count
        named.append(f"{kind}/{name}")
        for figures in containers:
            if "requests.memory" in figures:
                memory_requested += count * quantity_mib(figures["requests.memory"])
            if "limits.memory" in figures:
                memory_limited += count * quantity_mib(figures["limits.memory"])
            if "requests.cpu" in figures:
                cpu_requested += count * quantity_millicores(figures["requests.cpu"])
    return {
        "pods": pods,
        "memory_requested_mib": round(memory_requested, 1),
        "memory_limited_mib": round(memory_limited, 1),
        "cpu_requested_millicores": round(cpu_requested, 1),
        "workloads": sorted(named),
    }


def read_baseline(path: str) -> dict:
    held = {}
    for line in open(path, encoding="utf-8"):
        line = line.split("#", 1)[0].rstrip()
        match = re.match(r"^\s{2}(\w+):\s*(\S+)\s*$", line)
        if match and match.group(2) not in ("null", "~"):
            try:
                held[match.group(1)] = float(match.group(2))
            except ValueError:
                pass
    return held


def held_to(baseline: dict, figures: dict) -> list[str]:
    problems = []
    for key, allowed in LIMITS.items():
        if key not in baseline or key not in figures:
            continue
        base, now = baseline[key], figures[key]
        most = base * (1 + allowed)
        if now > most + 1e-9:
            problems.append(
                f"{key} is {now:g}, past {most:g} ({allowed:.0%} over v29's {base:g})"
            )
    return problems


def self_test() -> None:
    chart = """
apiVersion: apps/v1
kind: Deployment
metadata:
  name: x-lake
spec:
  replicas: 1
  template:
    spec:
      containers:
        - name: lake
          resources:
            requests:
              memory: 128Mi
              cpu: 50m
            limits:
              memory: 1Gi
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: x-db
spec:
  replicas: 0
---
apiVersion: batch/v1
kind: Job
metadata:
  name: x-migrate
"""
    figures = render(chart)
    assert figures["pods"] == 1, figures
    assert figures["memory_requested_mib"] == 128, figures
    assert figures["memory_limited_mib"] == 1024, figures
    assert figures["cpu_requested_millicores"] == 50, figures
    base = {"pods": 1, "memory_requested_mib": 100, "cpu_requested_millicores": 50, "disk_mib": 10}
    assert held_to(base, {"pods": 1, "memory_requested_mib": 115, "cpu_requested_millicores": 57.5}) == []
    assert len(held_to(base, {"pods": 2, "memory_requested_mib": 116})) == 2
    assert held_to(base, {"disk_mib": 11.0}) == []
    assert held_to(base, {"disk_mib": 11.1})
    assert quantity_mib("1Gi") == 1024 and quantity_millicores("0.5") == 500


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", nargs="?")
    parser.add_argument("--baseline")
    parser.add_argument("--peak-mib", type=float)
    parser.add_argument("--disk-mib", type=float)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print("footprint: self-test OK")
        if not args.command:
            return 0
    if args.command == "render":
        figures = render(sys.stdin.read())
        for key, value in figures.items():
            if key == "workloads":
                print(f"  {key}: [{', '.join(value)}]")
            else:
                print(f"  {key}: {value:g}")
        return 0
    if args.command in ("check", "measured"):
        if not args.baseline:
            print("footprint: --baseline is needed", file=sys.stderr)
            return 2
        baseline = read_baseline(args.baseline)
        if args.command == "check":
            figures = render(sys.stdin.read())
        else:
            figures = {}
            if args.peak_mib is not None:
                figures["memory_peak_mib"] = args.peak_mib
            if args.disk_mib is not None:
                figures["disk_mib"] = args.disk_mib
        problems = held_to(baseline, figures)
        said = ", ".join(
            f"{k} {figures[k]:g} (v29 {baseline[k]:g})" for k in LIMITS if k in figures and k in baseline
        )
        if problems:
            print("footprint FAILED: the default profile is past v29's baseline (ruling 1058):", file=sys.stderr)
            for problem in problems:
                print(f"  {problem}", file=sys.stderr)
            print("  Raising the baseline is a reviewed change with its reason, never a fix.", file=sys.stderr)
            return 1
        print(f"footprint OK: {said}")
        return 0
    parser.print_usage(sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
