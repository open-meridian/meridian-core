#!/usr/bin/env python3
"""What the fast pre-push gate runs, chosen from what the branch changed.

The pre-push hook runs `make pre-push` (accepted 2026-11-07: CI and the merge
queue are the authority for main; local is fast and targeted). Some of its
stages run on every push; the rest only when a path the branch changed is
mapped to them by PRE_PUSH_MAP, kept in the Makefile beside the stages:

    <glob>=<stage>[,<stage>...]

A glob is matched against each changed path whole (fnmatch, where * crosses
directories). Two stage names choose the Rust stage's tests as well:

    crate      the crate the path is in (crates/<dir>/...), or else every
               crate whose sources name the path (deploy/topics.tsv, which
               the sidecar and the dashboard compile in)
    workspace  every crate (Cargo.lock, Dockerfile.rust)

and the Rust stage then runs the unit tests of those crates and of every
crate that depends on them, directly or not, from the workspace's own
Cargo.toml files. Any other name is a stage of its own (pre-push-<name>).

What changed is `git diff --name-only <base>...HEAD`, the base origin/main by
default (PRE_PUSH_BASE). With no base to compare to, every stage is chosen,
and the run says so.

    pre_push.py plan --map "<glob>=<stage> ..." --always "<stage> ..."
                     [--base REF] [--order "<stage> ..."] --out FILE
    pre_push.py --self-test

Prints which stages were chosen and why, and writes FILE as make arguments:
PRE_PUSH_STAGES="..." PRE_PUSH_TESTS="<nextest filterset>".
"""

from __future__ import annotations

import argparse
import fnmatch
import os
import pathlib
import re
import subprocess
import sys
import tempfile

try:
    import tomllib
except ModuleNotFoundError:  # Python before 3.11
    tomllib = None

ROOT = pathlib.Path(__file__).resolve().parent.parent
RUST = "rust"
CRATE, WORKSPACE = "crate", "workspace"


def git(*args: str) -> subprocess.CompletedProcess:
    # A pre-push hook runs with GIT_DIR set; this reads this repository only,
    # but says so rather than trust what the hook inherited.
    env = {k: v for k, v in os.environ.items() if k not in ("GIT_DIR", "GIT_WORK_TREE")}
    return subprocess.run(["git", "-C", str(ROOT), *args], capture_output=True, text=True, env=env)


def changed_paths(base: str) -> list[str] | None:
    """The paths the branch changed against base, or None when base is not
    there to compare with."""
    if git("rev-parse", "--verify", "--quiet", f"{base}^{{commit}}").returncode != 0:
        return None
    done = git("diff", "--name-only", f"{base}...HEAD")
    if done.returncode != 0:
        return None
    return [p for p in done.stdout.splitlines() if p]


def parse_toml(path: pathlib.Path) -> dict:
    text = path.read_text(encoding="utf-8")
    if tomllib is not None:
        return tomllib.loads(text)
    # Enough of TOML for a Cargo.toml's [package] name and its dependency
    # tables' keys, where tomllib is not there.
    data: dict = {}
    table = None
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        m = re.match(r"^\[([^\]]+)\]$", line)
        if m:
            table = data.setdefault(m.group(1), {})
            continue
        m = re.match(r"^([A-Za-z0-9_.-]+)\s*=\s*(.*)$", line)
        if m and table is not None:
            key, value = m.group(1), m.group(2)
            table[key.split(".")[0]] = value.strip('"')
    if "package" in data:
        return {"package": data["package"], **{k: v for k, v in data.items() if k != "package"}}
    return data


def workspace(root: pathlib.Path) -> tuple[dict[str, str], dict[str, set[str]]]:
    """The workspace's crates, by directory under crates/ -> package name, and
    each package's workspace dependencies (normal, dev and build)."""
    members = parse_toml(root / "Cargo.toml").get("workspace", {}).get("members", [])
    if isinstance(members, str):  # the fallback parser's flat value
        members = re.findall(r'"([^"]+)"', members) or [m.strip() for m in members.strip("[]").split(",")]
    by_dir: dict[str, str] = {}
    manifests: dict[str, dict] = {}
    for member in members:
        manifest = parse_toml(root / member / "Cargo.toml")
        name = manifest["package"]["name"]
        by_dir[pathlib.PurePosixPath(member).name] = name
        manifests[name] = manifest
    names = set(manifests)
    deps: dict[str, set[str]] = {}
    for name, manifest in manifests.items():
        found = set()
        for table in ("dependencies", "dev-dependencies", "build-dependencies"):
            found |= {d for d in manifest.get(table, {}) if d in names}
        deps[name] = found - {name}
    return by_dir, deps


def dependents(deps: dict[str, set[str]], chosen: set[str]) -> set[str]:
    """chosen, and every crate depending on one of them, directly or not."""
    out = set(chosen)
    grew = True
    while grew:
        grew = False
        for name, needs in deps.items():
            if name not in out and needs & out:
                out.add(name)
                grew = True
    return out


def naming(root: pathlib.Path, by_dir: dict[str, str], path: str) -> set[str]:
    """The crates whose Rust sources name path (include_str!, a test's read)."""
    found = set()
    for directory, name in by_dir.items():
        for src in (root / "crates" / directory).rglob("*.rs"):
            try:
                if path in src.read_text(encoding="utf-8", errors="ignore"):
                    found.add(name)
                    break
            except OSError:
                continue
    return found


def parse_map(text: str) -> list[tuple[str, list[str]]]:
    rules = []
    for pair in text.split():
        glob, sep, stages = pair.partition("=")
        if not sep or not glob or not stages:
            sys.exit(f"pre-push: PRE_PUSH_MAP entry '{pair}' is not <glob>=<stage>[,<stage>]")
        rules.append((glob, [s for s in stages.split(",") if s]))
    return rules


def plan(root: pathlib.Path, rules: list[tuple[str, list[str]]], always: list[str],
         order: list[str], paths: list[str] | None) -> tuple[list[str], list[str], list[str]]:
    """(stages in order, crates whose tests run, lines saying why)."""
    # A Cargo workspace's crates only where the map names them: the same
    # tool serves the repositories with no Rust, or one crate.
    uses_crates = any(s in (CRATE, WORKSPACE) for _, ss in rules for s in ss)
    by_dir, deps = workspace(root) if uses_crates else ({}, {})
    every_stage = list(dict.fromkeys(always + [s for _, ss in rules for s in ss
                                               if s not in (CRATE, WORKSPACE)]
                                     + ([RUST] if uses_crates else [])))
    why: dict[str, list[str]] = {s: ["every push"] for s in always}
    crates: set[str] = set()
    because: dict[str, str] = {}
    lines = []
    if paths is None:
        for stage in every_stage:
            why.setdefault(stage, []).append("no base to compare with")
        crates = set(deps)
        for name in crates:
            because[name] = "no base"
    else:
        for path in paths:
            for glob, stages in rules:
                if not fnmatch.fnmatchcase(path, glob):
                    continue
                for stage in stages:
                    if stage == WORKSPACE:
                        why.setdefault(RUST, []).append(f"{path} (every crate)")
                        for name in deps:
                            because.setdefault(name, path)
                        crates |= set(deps)
                    elif stage == CRATE:
                        parts = path.split("/")
                        if parts[0] == "crates" and len(parts) > 2 and parts[1] in by_dir:
                            named = {by_dir[parts[1]]}
                        else:
                            named = naming(root, by_dir, path)
                        if named:
                            why.setdefault(RUST, []).append(f"{path} ({', '.join(sorted(named))})")
                            for name in named:
                                because.setdefault(name, path)
                            crates |= named
                    else:
                        why.setdefault(stage, []).append(path)
    tested = dependents(deps, crates)
    known = order + [s for s in every_stage if s not in order]
    stages = [s for s in known if s in why]
    for stage in known:
        reasons = why.get(stage)
        if reasons:
            shown = reasons if len(reasons) <= 4 else reasons[:4] + [f"and {len(reasons) - 4} more"]
            lines.append(f"pre-push: {stage:<9} RUN   {'; '.join(shown)}")
        else:
            lines.append(f"pre-push: {stage:<9} skip  no changed path maps to it")
    if tested:
        rdeps = sorted(tested - crates)
        lines.append(f"pre-push: unit tests of {len(tested)} crate(s): {' '.join(sorted(crates))}"
                     + (f"; and, depending on them, {' '.join(rdeps)}" if rdeps else ""))
    return stages, sorted(tested), lines


def filterset(crates: list[str]) -> str:
    return " | ".join(f"package({c})" for c in crates)


def run_plan(args: argparse.Namespace) -> int:
    rules = parse_map(args.map)
    paths = changed_paths(args.base)
    if paths is None:
        print(f"pre-push: {args.base} is not here to compare with, so every stage runs "
              f"(fetch it, or set PRE_PUSH_BASE)")
    else:
        print(f"pre-push: {len(paths)} path(s) changed against {args.base} "
              f"(git diff --name-only {args.base}...HEAD)")
    stages, crates, lines = plan(ROOT, rules, args.always.split(), args.order.split(), paths)
    print("\n".join(lines))
    pathlib.Path(args.out).write_text(
        f'PRE_PUSH_STAGES="{" ".join(stages)}" PRE_PUSH_TESTS="{filterset(crates)}"\n')
    return 0


def self_test() -> int:
    problems = []
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        (root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/bus", "crates/sidecar", "crates/runtime", "crates/lone"]\n')
        crates = {
            "bus": ("meridian-bus", ""),
            "sidecar": ("meridian-sidecar", 'meridian-bus = { path = "../bus" }\n'),
            "runtime": ("meridian-runtime", "meridian-sidecar.workspace = true\n"),
            "lone": ("meridian-lone", "[dev-dependencies]\nmeridian-bus = { path = \"../bus\" }\n"),
        }
        for d, (name, dep) in crates.items():
            (root / "crates" / d / "src").mkdir(parents=True)
            body = f'[package]\nname = "{name}"\n'
            if dep and not dep.startswith("["):
                body += "\n[dependencies]\n"
            body += dep
            (root / "crates" / d / "Cargo.toml").write_text(body)
            (root / "crates" / d / "src" / "lib.rs").write_text("")
        (root / "crates" / "sidecar" / "src" / "lib.rs").write_text(
            'const T: &str = include_str!("../../../deploy/topics.tsv");\n')
        rules = parse_map("crates/*=crate deploy/*=crate Cargo.lock=workspace "
                          "proto/*=codegen,contract deploy/chart/*=chart")
        order = ["checks", "rust", "codegen", "chart", "contract"]
        cases = [
            (["README.md"], ["checks"], []),
            (["crates/sidecar/src/lib.rs"], ["checks", "rust"], ["meridian-runtime", "meridian-sidecar"]),
            (["crates/bus/src/lib.rs"], ["checks", "rust"],
             ["meridian-bus", "meridian-lone", "meridian-runtime", "meridian-sidecar"]),
            (["deploy/topics.tsv"], ["checks", "rust"], ["meridian-runtime", "meridian-sidecar"]),
            (["deploy/chart/values.yaml"], ["checks", "chart"], []),
            (["proto/a.proto"], ["checks", "codegen", "contract"], []),
            (["Cargo.lock"], ["checks", "rust"],
             ["meridian-bus", "meridian-lone", "meridian-runtime", "meridian-sidecar"]),
            (None, order, ["meridian-bus", "meridian-lone", "meridian-runtime", "meridian-sidecar"]),
        ]
        for paths, want_stages, want_crates in cases:
            stages, tested, _ = plan(root, rules, ["checks"], order, paths)
            if stages != want_stages or tested != want_crates:
                problems.append(f"{paths}: got {stages} {tested}, want {want_stages} {want_crates}")
        stages, tested, _ = plan(root, parse_map("src/*=test docs/*=docs"), ["lint"],
                                 ["lint", "test", "docs"], ["src/a.py", "README.md"])
        if stages != ["lint", "test"] or tested != []:
            problems.append(f"no crates: got {stages} {tested}")
        stages, _, _ = plan(root, parse_map("src/*=test docs/*=docs"), ["lint"],
                            ["lint", "test", "docs"], None)
        if stages != ["lint", "test", "docs"]:
            problems.append(f"no crates, no base: got {stages}")
    if filterset(["a", "b"]) != "package(a) | package(b)":
        problems.append("filterset")
    for p in problems:
        print(f"pre_push.py self-test FAILED: {p}", file=sys.stderr)
    if not problems:
        print("pre_push.py self-test OK")
    return 1 if problems else 0


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("plan")
    p.add_argument("--map", required=True)
    p.add_argument("--always", default="")
    p.add_argument("--order", default="")
    p.add_argument("--base", default="origin/main")
    p.add_argument("--out", required=True)
    args = parser.parse_args()
    return run_plan(args)


if __name__ == "__main__":
    sys.exit(main())
