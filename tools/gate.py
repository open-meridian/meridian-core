#!/usr/bin/env python3
"""The pre-push gate, in stages, inside a disk budget.

`make ci-local` runs its targets in stages (the Makefile's GATE_STAGES), and
this is what runs around each: a floor checked before anything starts, the
disk watched while every stage runs, and what a stage built for itself freed
before the next begins. The gates share one disk with the product owner's
development deployment, whose kubelet evicted every pod at 14 GB free on
2026-09-30, and a single `make ci-local` had stopped fitting on it; adding a
feature must not mean adding disk. So the most the gate holds at once is the
compile every stage shares and the largest single stage, not their sum, and a
stage that grows past its budget fails the gate rather than the machine.

Disk is read where Docker keeps it: `df` of the root of a container, which is
Docker's own filesystem, on a Docker Desktop VM and a CI runner alike.

    gate.py start --make-pid PID --floor-start GB --floor-stop GB
                  --budgets "stage=GB ..." --images "..."
    gate.py begin STAGE
    gate.py drop IMAGE ...
    gate.py mark STAGE SERVICE ...
    gate.py release STAGE
    gate.py end STAGE [--drop IMAGE ...]
    gate.py finish

What `end` frees: the build cache no image and no stage still holds, the cargo
caches excepted (they are the compile every stage shares, named
meridian-core-* in the Dockerfiles), the images named after --drop, and any
image of the gate's own that a rebuild left untagged. Never a volume, and never
an image the gate did not make. The cargo caches outlive a run, so a change
recompiles only what it touched; `start` empties them once they are older than
GATE_CACHE_DAYS, since cargo keeps what it built for every dependency version
it has ever seen.

What `release` frees: the containers of the compose services `mark` named
that the stage's targets created -- those there at `release` and not at
`mark` -- with their anonymous volumes (a database's data, a directory's).
Never a container that was there before the stage, so a developer's own
stack keeps running; never a named volume; and never the services' images,
which later stages start again.
"""

from __future__ import annotations

import argparse
import calendar
import json
import os
import pathlib
import signal
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
STATE = ROOT / ".gate"
PROBE = "meridian-gate-disk"
PROBE_IMAGE = "python:3.12-alpine"  # in every gate already; busybox df
SAMPLE_SECONDS = 2
GB = 1024 * 1024  # in the KB df reports

# The stage whose build every later stage shares: what it leaves on the disk
# once freed -- the cargo caches, the images later stages start -- is charged
# to it alone, and a later stage is charged what it holds above that. On a
# warm machine it leaves next to nothing; from cold caches, after a prune, it
# leaves the caches it filled (5.5 to 5.7 GB on 2026-10-27), which charged to
# every later stage again had settings-page fail its 7 GB at 8.9 to 11.4 GB
# while holding 3.4 to 3.5 GB of its own.
SHARED_STAGE = "compile"


def docker(*args: str, check: bool = False) -> subprocess.CompletedProcess:
    return subprocess.run(["docker", *args], capture_output=True, text=True, check=check)


def free_kb() -> int:
    """Free KB on Docker's filesystem, from the probe if it runs."""
    for cmd in (["exec", PROBE], ["run", "--rm", "--network", "none", PROBE_IMAGE]):
        done = docker(*cmd, "df", "-Pk", "/")
        lines = done.stdout.strip().splitlines()
        if done.returncode == 0 and len(lines) >= 2:
            return int(lines[-1].split()[3])
    sys.exit("gate: cannot read the free space on Docker's filesystem (docker df failed)")


def compose_images() -> dict[str, str]:
    """The images this repository's compose file builds, by service: the gate's
    own, named as compose names them (<project>-<service>)."""
    done = subprocess.run(["docker", "compose", "--profile", "*", "config", "--format", "json"],
                          cwd=ROOT, capture_output=True, text=True)
    if done.returncode != 0:
        return {}
    config = json.loads(done.stdout)
    return {name: service.get("image") or f"{config['name']}-{name}"
            for name, service in config["services"].items() if service.get("build")}


def cargo_caches() -> list[tuple[str, float]]:
    """The gate's own cargo caches -- the cache mounts named meridian-core-* in
    the Dockerfiles -- by build cache record, with when each was made (epoch
    seconds). Their sizes as Docker reports them are not kept current, so they
    are bounded by age instead."""
    out = docker("buildx", "du", "--verbose", "--filter", "type=exec.cachemount").stdout
    found, cache, made = [], None, time.time()
    for line in out.splitlines():
        key, _, value = line.partition(":")
        value = value.strip()
        if key == "ID":
            cache, made = value, time.time()
        elif key == "Created at":
            # 2026-10-13 02:23:52.378110192 +0000 UTC
            stamp = value.split(".")[0]
            made = calendar.timegm(time.strptime(stamp, "%Y-%m-%d %H:%M:%S"))
        elif key == "Description" and 'with id "/meridian-core-' in value and cache:
            found.append((cache, made))
    return found


def gb(kb: float) -> str:
    return f"{kb / GB:.1f}"


def load() -> dict:
    return json.loads((STATE / "run.json").read_text())


def samples(since: float, until: float) -> list[int]:
    out = []
    path = STATE / "samples"
    if path.exists():
        for line in path.read_text().splitlines():
            when, kb = line.split()
            if since <= float(when) <= until:
                out.append(int(kb))
    return out


def record(kb: int) -> None:
    with open(STATE / "samples", "a") as f:
        f.write(f"{time.time():.1f} {kb}\n")


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def descendants(pid: int) -> list[int]:
    """pid and everything under it, from ps, which macOS and Linux both have."""
    rows = subprocess.run(["ps", "-A", "-o", "pid=,ppid="], capture_output=True, text=True).stdout
    children: dict[int, list[int]] = {}
    for row in rows.splitlines():
        child, parent = (int(x) for x in row.split())
        children.setdefault(parent, []).append(child)
    found, stack = [], [pid]
    while stack:
        p = stack.pop()
        found.append(p)
        stack.extend(children.get(p, []))
    return found


def watch(make_pid: int, floor_stop_kb: int) -> None:
    """The daemon: a sample every few seconds until make exits; under the stop
    floor, make and everything it started are stopped, with the reason said."""
    try:
        while alive(make_pid) and not (STATE / "finished").exists():
            kb = free_kb()
            record(kb)
            if kb < floor_stop_kb:
                msg = (f"\ngate: STOPPED -- {gb(kb)} GB free on Docker's filesystem, under the "
                       f"{gb(floor_stop_kb)} GB floor (GATE_DISK_STOP). Stopping make so the disk is "
                       f"not filled for whatever else runs on it.\n")
                (STATE / "breach").write_text(msg)
                sys.stderr.write(msg)
                sys.stderr.flush()
                for pid in reversed(descendants(make_pid)):
                    try:
                        os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                break
            time.sleep(SAMPLE_SECONDS)
    finally:
        docker("rm", "-f", PROBE)
        if not alive(make_pid):
            # make stopped short of `finish`: nothing is left to measure for.
            (STATE / "run.json").unlink(missing_ok=True)


def start(args: argparse.Namespace) -> int:
    old = STATE / "daemon.pid"
    if old.exists():
        try:
            os.kill(int(old.read_text()), signal.SIGTERM)
        except (ProcessLookupError, ValueError):
            pass
    docker("rm", "-f", PROBE)
    for f in ("samples", "stages", "current", "breach", "finished", "daemon.pid", "marked.json"):
        (STATE / f).unlink(missing_ok=True)
    STATE.mkdir(exist_ok=True)

    budgets = {}
    for pair in args.budgets.split():
        name, _, value = pair.partition("=")
        budgets[name] = float(value)
    floor_start_kb = int(float(args.floor_start) * GB)
    floor_stop_kb = int(float(args.floor_stop) * GB)

    # The cargo caches outlive a run, so a change compiles only what changed;
    # but cargo never deletes what it built for a dependency version or a
    # toolchain no longer used, so past their age they start again empty
    # rather than grow with every upgrade for as long as the machine lives.
    caches = cargo_caches()
    oldest = min((made for _, made in caches), default=time.time())
    if time.time() - oldest > float(args.cache_days) * 86400:
        print(f"gate: the cargo caches are {(time.time() - oldest) / 86400:.0f} days old, past "
              f"GATE_CACHE_DAYS ({args.cache_days}); emptied, so this run compiles from nothing")
        for cache, _ in caches:
            docker("builder", "prune", "-f", "--filter", f"id={cache}")

    docker("run", "-d", "--rm", "--name", PROBE, "--network", "none", PROBE_IMAGE, "sleep", "86400")
    kb = free_kb()
    if kb < floor_start_kb:
        docker("rm", "-f", PROBE)
        print(f"gate: REFUSED -- {gb(kb)} GB free on Docker's filesystem, under the "
              f"{gb(floor_start_kb)} GB the gate starts from (GATE_DISK_START).\n"
              f"  Free some first: `docker system df` says where it is. The gate needs up to "
              f"{max(budgets.values(), default=0):.0f} GB more at its peak (GATE_BUDGETS) and stops "
              f"itself under {gb(floor_stop_kb)} GB (GATE_DISK_STOP).", file=sys.stderr)
        return 1
    peak = max(budgets.values(), default=0)
    print(f"gate: {gb(kb)} GB free on Docker's filesystem; floors {gb(floor_start_kb)} GB to start, "
          f"{gb(floor_stop_kb)} GB to stop; the largest stage's budget is {peak:.0f} GB")
    sys.stdout.flush()
    if kb - peak * GB < floor_stop_kb:
        print(f"gate: warning -- a stage at its full budget would take this machine under the "
              f"{gb(floor_stop_kb)} GB floor, where the gate stops itself", file=sys.stderr)

    images = {}
    for ref in args.images.split() + list(compose_images().values()):
        images[ref] = docker("image", "inspect", "--format", "{{.Id}}", ref).stdout.strip()
    (STATE / "run.json").write_text(json.dumps({
        "make_pid": args.make_pid, "floor_start_kb": floor_start_kb,
        "floor_stop_kb": floor_stop_kb, "budgets": budgets,
        "start_free_kb": kb, "started": time.time(), "images": images,
    }))
    record(kb)

    sys.stdout.flush()
    if os.fork() == 0:
        os.setsid()
        if os.fork() == 0:
            devnull = os.open(os.devnull, os.O_RDWR)
            os.dup2(devnull, 0)
            os.dup2(devnull, 1)  # stderr stays: a breach is said where make says things
            (STATE / "daemon.pid").write_text(str(os.getpid()))
            watch(args.make_pid, floor_stop_kb)
        os._exit(0)
    return 0


def started() -> bool:
    if (STATE / "run.json").exists():
        return True
    print("gate: no run started (make ci-local starts one); this stage runs unmeasured")
    return False


def begin(args: argparse.Namespace) -> int:
    if not started():
        return 0
    kb = free_kb()
    record(kb)
    (STATE / "current").write_text(f"{args.stage} {time.time():.1f}")
    print(f"\n== gate: {args.stage} ({gb(kb)} GB free)")
    return 0


def free_stage(run: dict, drop: list[str]) -> None:
    built = compose_images()
    for ref in drop:
        # compose:<service> is the image compose builds for that service.
        ref = built.get(ref.removeprefix("compose:"), ref) if ref.startswith("compose:") else ref
        docker("image", "rm", ref)
    # Images of the gate's own that a rebuild left untagged: removed by ID, and
    # only when nothing holds them.
    for ref, was in run["images"].items():
        now = docker("image", "inspect", "--format", "{{.Id}}", ref).stdout.strip()
        if was and was != now:
            docker("image", "rm", was)
            run["images"][ref] = now
        elif not was and now:
            run["images"][ref] = now
    (STATE / "run.json").write_text(json.dumps(run))
    # Build cache nothing holds, the cache mounts kept: the cargo caches are
    # the compile every stage shares (bounded by GATE_CACHE_DAYS at `start`),
    # and another project's are not the gate's to drop.
    docker("builder", "prune", "-f", "--filter", "type!=exec.cachemount")


def drop(args: argparse.Namespace) -> int:
    """Within a stage, what its earlier targets built and its later ones do not
    need, freed before they run -- `end`'s --drop, staged -- so the stage holds
    the larger of its parts at once, not their sum."""
    if not started():
        return 0
    run = load()
    before = free_kb()
    record(before)
    free_stage(run, args.images)
    after = free_kb()
    record(after)
    print(f"-- gate: dropped {' '.join(args.images)} within the stage: {gb(before)} GB free, "
          f"{gb(after)} GB once freed", flush=True)
    return 0


def service_containers(services: list[str]) -> set[str]:
    """The containers of these services in this repository's compose project,
    running or not, by ID."""
    done = subprocess.run(["docker", "compose", "--profile", "*", "ps", "-aq", *services],
                          cwd=ROOT, capture_output=True, text=True)
    return set(done.stdout.split()) if done.returncode == 0 else set()


def mark(args: argparse.Namespace) -> int:
    """The services whose containers the stage's targets will create, with the
    containers they have already, so `release` frees only the stage's own."""
    if not started():
        return 0
    path = STATE / "marked.json"
    marked = json.loads(path.read_text()) if path.exists() else {}
    marked[args.stage] = {"services": args.services,
                          "before": sorted(service_containers(args.services))}
    path.write_text(json.dumps(marked))
    return 0


def release(args: argparse.Namespace) -> int:
    """The containers the stage's targets created for the services `mark`
    named, and their anonymous volumes, removed before the next stage."""
    if not started():
        return 0
    path = STATE / "marked.json"
    marked = json.loads(path.read_text()) if path.exists() else {}
    if args.stage not in marked:
        sys.exit(f"gate: releasing {args.stage}, which marked no services")
    entry = marked.pop(args.stage)
    path.write_text(json.dumps(marked))
    before = free_kb()
    record(before)
    created = sorted(service_containers(entry["services"]) - set(entry["before"]))
    for container in created:
        docker("rm", "-f", "-v", container)
    after = free_kb()
    record(after)
    print(f"-- gate: released {len(created)} container(s) of {' '.join(entry['services'])} "
          f"the stage made, with their volumes: {gb(before)} GB free, {gb(after)} GB once freed",
          flush=True)
    return 0


def end(args: argparse.Namespace) -> int:
    if not started():
        return 0
    run = load()
    stage, began = (STATE / "current").read_text().split()
    if stage != args.stage:
        sys.exit(f"gate: ending {args.stage}, but {stage} is the stage running")
    now_kb = free_kb()
    record(now_kb)
    ended = time.time()
    lowest = min(samples(float(began), ended) + [now_kb])
    above_kb = max(0, run["start_free_kb"] - lowest)
    shared_kb = 0 if stage == SHARED_STAGE else run.get("shared_kb", 0)
    peak_kb = max(0, above_kb - shared_kb)
    budget = run["budgets"].get(stage)
    over = budget is not None and peak_kb > budget * GB
    free_stage(run, args.drop)
    after = free_kb()
    record(after)
    if stage == SHARED_STAGE:
        run["shared_kb"] = max(0, run["start_free_kb"] - after)
        (STATE / "run.json").write_text(json.dumps(run))
    verdict = "OVER" if over else "ok"
    with open(STATE / "stages", "a") as f:
        f.write(f"{stage}\t{gb(peak_kb)}\t{budget if budget is not None else '-'}\t"
                f"{gb(lowest)}\t{gb(after)}\t{ended - float(began):.0f}\t{verdict}\n")
    (STATE / "current").unlink()
    shared_said = (f", {gb(shared_kb)} of it the {SHARED_STAGE}'s, {gb(peak_kb)} its own"
                   if shared_kb else "")
    print(f"-- gate: {stage} peaked {gb(above_kb)} GB above the start{shared_said} (budget "
          f"{budget if budget is not None else 'none'}), {gb(lowest)} GB free at its lowest, "
          f"{gb(after)} GB once freed", flush=True)
    if stage == SHARED_STAGE and run["shared_kb"]:
        print(f"-- gate: the {SHARED_STAGE} leaves {gb(run['shared_kb'])} GB every later stage "
              f"shares; each is charged what it holds above that", flush=True)
    if budget is None:
        print(f"gate: FAILED -- stage {stage} has no budget in GATE_BUDGETS", file=sys.stderr)
        return 1
    if over:
        print(f"gate: FAILED -- stage {stage} held {gb(peak_kb)} GB above where the gate started"
              f"{f' and the {SHARED_STAGE} left' if shared_kb else ''}, "
              f"over its {budget:g} GB budget (GATE_BUDGETS_LOCAL or GATE_BUDGETS_CI in the "
              f"Makefile).\n"
              f"  A stage that needs more disk than it did is a finding: free what it builds "
              f"before the next stage, split it, or raise its budget in the same commit that "
              f"says why.\n"
              f"  measured so far: {measured()}", file=sys.stderr)
        return 1
    return 0


def measured() -> str:
    """Each stage's peak so far as a budget row, ready to paste into the Makefile."""
    rows = [line.split("\t") for line in (STATE / "stages").read_text().splitlines()]
    return " ".join(f"{row[0]}={row[1]}" for row in rows)


def finish(_: argparse.Namespace) -> int:
    run = load()
    (STATE / "finished").write_text("")
    rows = [line.split("\t") for line in (STATE / "stages").read_text().splitlines()]
    shared = run.get("shared_kb", 0)
    print("\ngate: per-stage peak, GB above the start "
          f"({gb(run['start_free_kb'])} GB free then)"
          + (f", after the {SHARED_STAGE} above the {gb(shared)} GB it left" if shared else "")
          + ":")
    for stage, peak, budget, lowest, after, secs, verdict in rows:
        print(f"  {stage:<14} peak {peak:>5} of {budget:>4}  lowest free {lowest:>5}  "
              f"freed to {after:>5}  {int(secs) // 60:>3}m{int(secs) % 60:02d}s  {verdict}")
    worst = max(rows, key=lambda r: float(r[1]))
    print(f"gate: the most a stage held was {worst[1]} GB, in {worst[0]}"
          + (f", above the {gb(shared)} GB the {SHARED_STAGE} left" if shared and worst[0] != SHARED_STAGE else ""))
    print(f"gate: measured peaks, a budget row before headroom: {measured()}")
    docker("rm", "-f", PROBE)
    (STATE / "run.json").unlink()
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("start")
    s.add_argument("--make-pid", type=int, required=True)
    s.add_argument("--floor-start", required=True)
    s.add_argument("--floor-stop", required=True)
    s.add_argument("--budgets", required=True)
    s.add_argument("--images", default="")
    s.add_argument("--cache-days", default="14")
    b = sub.add_parser("begin")
    b.add_argument("stage")
    e = sub.add_parser("end")
    e.add_argument("stage")
    e.add_argument("--drop", nargs="*", default=[])
    d = sub.add_parser("drop")
    d.add_argument("images", nargs="+")
    m = sub.add_parser("mark")
    m.add_argument("stage")
    m.add_argument("services", nargs="+")
    r = sub.add_parser("release")
    r.add_argument("stage")
    sub.add_parser("finish")
    args = parser.parse_args()
    return {"start": start, "begin": begin, "drop": drop, "mark": mark, "release": release,
            "end": end, "finish": finish}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
