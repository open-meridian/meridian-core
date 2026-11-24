"""Failure classes injected and timed on a scratch cluster (step 3b).

spec/a-deployment-scales-out-and-fails-over-by-configuration sections 10 and
15, plan Track 7; rulings 1048, 1051, 1052, 1056. One run installs the chart
in a profile and injects each failure class named, measuring how long the
component took to answer again and holding it to its bound:

- `PROFILE=tolerant`, the default chart, one member each: a component is
  back by restart within the tolerant bound (1056: a pod or the database
  within 2 min).
- `PROFILE=ha`, a warm standby per writer (`<component>.standby: 1`) on three
  agents labelled into three zones (`zones`): the standby takes the lease,
  raises the epoch and answers within the component tier (1048: the book,
  street and order path within 10 s; lake writes within 15 s), the takeover
  recorded (`WriterTookOver`), the old writer back as the standby.
- `PROFILE=scaled`, the HA install held to its shape (e2e-cluster-scaled):
  every member set spread over nodes and zones, each lease held, a budget
  per set, every stateless pod tolerating a lost node for 30 s; and a
  plugin instance at two shards with a standby pod serving a reader's
  standing want, each subject served once (step 3d, checkpoint 5, item 33).

A component "answers" when its change log answers on the bus from inside
one of its pods (`meridian-<component> probe`), asked every half second: so
each time measured includes the probe's own round trip through kubectl.

Failure classes (`FAILURE=`, comma-separated; all the profile's by default):
`lake-writer`, `bor-writer`, `street-writer`, `instrument-writer` (the
writer's process killed outright, SIGKILL from its node, so its lease lapses
unreleased); `node-freeze` (the writer's node paused past its
lease, then resumed: nothing it does after the lease is kept, and it steps
down); `split-brain` (the writer cut from the broker and the API server but
not the database, by packets dropped on its node, established connections
included: it steps down before its lease ends, the standby takes over, and
nothing it writes is kept); `node` (the writer's node stopped);
`network-policy` (a component cut from its database by a NetworkPolicy: the
probe names the path and its address, and it heals with no restart);
`zone` (HA only: every node of the zone holding the most writers stopped at
once; every store answers again from another zone within the zone tier);
`order-plugin` (the ems stand-in's pod's processes killed outright mid-flow:
found not ready, registered again, and the book's orders and placements
held to what it was told, none lost and none twice); `memory-pressure` and
`noisy-plugin` (stage M, the resilience spec's section 10a: the flood
instance reads past the lake's budget, or past its own share at its
sidecar, and is refused for load alone, with a retry-after, every other
instance within its band, nothing restarted and nothing OOMKilled);
`standing-coordinator` (HA only; contract v30, W12.4: the analytics
coordinator holding the standing driver's job killed outright, the job
registered again with a live coordinator within 30 s); `apiserver`
(contract v30, stage C: the API server cut from every pod of the release
for 30 s under the driver's load: no takeover, no write outage, no
readiness flap); `write-ahead-bound` (HA only, stage C: the lake's writer
cut from the broker past a short `lake.writeAhead.maxAge`, so the stream
drops what it held: the gap recorded with the next batch, asked again
where its dataset declares backfill, and read where the dataset is).

The buy-side trust targets (contract v30, stage C; the resilience spec's
section 15), after every class: the data driver reads its own price back
every second as a live page reads it (a default read of the latest), and
from those reads and the lake's own rows the run holds that a page drawn
from them (1) flagged the price stale within its data type's stale
threshold plus 2 s of the lake's newest row turning stale, never showing it
live past that, and (2) was never blank: no read answered without the last
good row. A failed read leaves the page as it was drawn, its value ageing.

The write driver (driver.py, step 3b part 5) writes through the stores for
the whole run, from test-only plugin instances in the chart's sidecars[]
set here at install: prices to the lake, orders to the book, and the ems
stand-in's orders and placements. After every class each acknowledged one
is looked for in the stores' own rows -- RPO through the stores, said in
the claims of db-primary, torn-write, zone and reconfigure -- and the oms
instance's history (invoke, ok, fail, info) is kept for Elle, which CI's
ha failover job runs on it (tools/elle-check.sh).

After the install and after every class, the record's own properties are
held (ruling 1105): in each store's change log no sequence missing or
taken twice, no epoch going back along the sequence (a fenced writer's
commit never kept after its successor's), and every epoch above the first
raised by a takeover the store recorded, one per epoch, none skipped.

Each measurement is printed as a claims line (`CLAIM {json}`) and kept in
.e2e-failover-claims.jsonl beside the checkout, with the command and the
cluster's Kubernetes: the class, its bound and whether it was held, and
nothing measured. Limits are public and measured figures are private (PO
2026-11-15): the line with its figures, and each time a check measured, is
kept in .e2e-failover-figures.jsonl beside the run, which nothing prints
and no job keeps. No platform: the conductor waits for an enrolment
nobody gives. No value of a secret, a key or a password is ever printed.
"""

import base64
import datetime
import http.client
import json
import os
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

NAMESPACE = os.environ.get("E2E_NAMESPACE", "meridian-failover-e2e")
RELEASE = os.environ.get("E2E_RELEASE", "fo")
FULL = f"{RELEASE}-meridian-runtime"
IMAGE = os.environ["E2E_IMAGE"]
# The brought database's image under Patroni (deploy/database), built by the
# make target for the HA profiles.
POSTGRES_IMAGE = os.environ.get("E2E_POSTGRES_IMAGE", "meridian-postgres:local")
CHART = os.path.abspath(os.environ.get("E2E_CHART", "deploy/chart"))
PROFILE = os.environ.get("PROFILE", "tolerant")
# The shape the install is in now: the profile's, except while
# e2e-cluster-reconfigure moves a live install from one to the other.
SHAPE = {"reconfigure": "tolerant"}.get(PROFILE, PROFILE)
CLAIMS = os.environ.get("E2E_FAILOVER_CLAIMS", ".e2e-failover-claims.jsonl")
# Every figure of a run, kept beside it and never printed: core is public,
# and so are its CI logs and artifacts. Not a log and not the claims file,
# so no job keeps it: a private run reads it for the ledger with figures
# (tools/claims.py figures).
FIGURES = os.environ.get("E2E_FAILOVER_FIGURES", ".e2e-failover-figures.jsonl")

# What a claims line says in public, and its verdict: the ledger's own tool,
# so the run and CI's claims-check judge a line by one rule.
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "tools"))
import claims as claims_tool  # noqa: E402
import shard_accounts  # noqa: E402
# Whether a release's workloads rolled out, judged as the CLI judges it: the
# cluster harness's, shared. Appended: e2e/cluster holds a run.py of its own.
sys.path.append(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "cluster"))
import rollout  # noqa: E402
CLUSTER = os.environ.get("E2E_K3D_CLUSTER", "meridian-e2e")
ADMIN = "local|alice"
ZONES = ["zone-a", "zone-b", "zone-c"]

STORES = ["lake", "bor", "instrument", "street", "conductor", "launcher"]
# The stores with a reader member in the HA runs (contract v30, stage B),
# and how many each.
READ_STORES = ["lake", "bor"]
READERS = 1
# TEST ONLY: where a reader is told to hold behind, while the file is there
# (MERIDIAN_READER_HOLD_FILE, which the chart never sets): in its pod's own
# scratch, the ready volume.
HOLD_FILE = "/run/meridian-ready/hold"
BINARY = {c: f"meridian-{c}" for c in STORES}

# The bounds each profile is held to (rulings 1048 and 1056), in seconds.
TOLERANT_POD = 120
TOLERANT_BROKER = 60
HA = {"bor": 10, "street": 10, "instrument": 10, "conductor": 10, "launcher": 10, "lake": 15}
# The database tier (1048): the record's commits back within 30 s of the
# primary lost, RPO 0; the lake within 30 s.
HA_DATABASE = {"bor": 30, "lake": 30}
# The zone tier (1048): a zone's node pool lost, every member there taken
# over within 2 min.
HA_ZONE = 120
# A standby's copy gone bad (step 3b part 4; proposed against 1048/1056):
# found within a minute of the replica scrub's start, and re-cloned from
# the primary within the zone tier, 2 min. Neither stops the primary
# serving, so neither is an outage; both are how long the deployment runs
# with one copy fewer than it was set up with.
REPLICA_DETECT = 60
REPLICA_RECLONE = 120

# Internal TLS (ruling 1084; tls.internal.enabled) in the ha run: every
# class there is injected with every store, Job and member on TLS, and
# after the install a plaintext connection is shown refused (internal_tls).
# The reconfigure run turns it on by an upgrade, before its first move.
TLS = PROFILE == "ha"

# The live install moved between the profiles, in order (e2e-cluster-
# reconfigure): up to ha, back, and up again, where it is restored to a time
# from a base backup taken in tolerant before the first move (step 3b part
# 7: the order the CLI's rehearsal failed in).
RECONFIGURE = ["ha", "tolerant", "ha"]

# The store-level write driver (step 3b part 5; driver.py): test-only plugin
# instances in the chart's sidecars[], set here at install and in no values
# file, writing through the stores' own write paths for the whole run:
# prices to the lake (data), orders to the book (oms), and orders placed by
# the ems stand-in (desk). What a class lost is read in the stores' own rows
# against what each acknowledged.
DRIVEN = PROFILE in ("tolerant", "ha", "reconfigure")
DRIVERS = {"drive-data": "data", "drive-oms": "oms", "drive-desk": "desk", "drive-flood": "flood",
           "drive-standing": "standing"}
# What each instance is launched with: the ems stand-in (desk) holds oms and
# ems, as e2e/orders/stand_in.py's desk does; it is the order-plugin class's
# subject.
DRIVER_ROLES = {"data": ["data", "reporting"], "oms": ["oms"], "desk": ["oms", "ems"],
                # The memory and isolation classes' subject (stage M): it
                # records its own dataset and reads it.
                "flood": ["data", "reporting"],
                # The standing-coordinator class's subject (contract v30): a
                # standing job held by its view.
                "standing": ["reporting"]}
# The flood instance's own sidecar.limits.inFlight: its pressure readers'
# count in driver.py (FLOOD_PRESSURE), each reaching the lake at once, as
# under the chart's 32 the class was built against; its noisy readers (160)
# stay past it.
FLOOD_IN_FLIGHT = 32
DRIVER_SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "driver.py")
DRIVER_FIXTURES = ["e2e/book/instruments.sql", "e2e/failover/driver.sql"]
# The subjects a sharded instance shares, in the scaled run (item 33).
SHARD_FIXTURES = ["e2e/failover/shards.sql"]
# Enough keys that the oms instance appends to the run's end (driver.py: a
# key takes PER_KEY appends, 400): at its 5 appends a second, 36 last 48
# minutes. Twelve lasted 17 (CI 43656183991, ha: full at 1,047 s, before
# rebalance began, so no order was appended through any move and Elle saw
# none). e2e/failover/driver.sql opens the same accounts.
DRIVER_ACCOUNTS = [f"ACC-DRIVE-{i:02d}" for i in range(36)]
DESK_ACCOUNT = "ACC-DRIVE-DESK"
DRIVER_INSTRUMENT = "LCL-01J8XQ4M7K00000000E2EAAPL"
DRIVER_CASH = "LCL-01J8XQ4M7K000000000E2EUSD"
# The run's signer, which vouches for the driver's person (a recorded order
# is a user's act): its seed in a Secret only the oms driver's container
# reads, its public half beside the dashboard's keys.
DRIVER_SIGNER = "e2e-drive-signer"
DRIVER_SIGNER_KEY_ID = "e2e-drive"
INTEROP_IMAGE = os.environ.get("E2E_INTEROP_IMAGE", "meridian-python-interop")
# A plugin instance run as two shards with a standby pod (step 3d; the
# resilience spec's sections 7 and 15), in the scaled shape: a sidecars[]
# entry whose plugin is the shard driver (driver.py, DRIVER_ROLE=shard; step
# 3d, checkpoint 5, item 33), a data plugin on the SDK's meridian.run, so
# what is held is the pods, their Leases, the standby's takeover, what a
# plugin's container is given and reaches, and what each pod serves of the
# reader's want (served_once, below).
SHARDED = "shard-proof"
SHARDS = 2
# What the shard driver says, as the stand-in before it did: whether the
# members' token is in its container, and each time it changes, whether its
# sidecar answers on loopback -- where a plugin's Register is answered, and
# so whether its run() may start. A shard lost stands its sidecar by again
# while the plugin's container -- and its log -- runs on (PR CI
# 43854641653: a standby's plugin log held the answer of a shard its pod had
# held before). So it says each change, and a check reads its last word.
# The same words as driver.py's.
SHARD_NO_TOKEN = "shard: no lease token in the plugin container"
SHARD_ANSWERED = "shard: the sidecar answers"
SHARD_GONE = "shard: the sidecar no longer answers"


def plugin_answered(said):
    """Whether the shard driver's log `said` ends answered: its last word of
    the sidecar is that it answers, not that it no longer does."""
    words = [line for line in said.splitlines() if SHARD_ANSWERED in line or SHARD_GONE in line]
    return bool(words) and SHARD_ANSWERED in words[-1]


# The reader whose standing want the shards share (driver.py,
# DRIVER_ROLE=shard-reader): a view of the subjects' prices, which its
# sidecar renews at the lake as a standing want of the dataset it is
# entitled to, the shard instance's.
SHARD_READER = "shard-reader"
SHARD_DATASET = f"{SHARDED}:served"
# The subjects shared (e2e/failover/shards.sql).
SHARD_SUBJECTS = [f"LCL-01J8XQ4M7K00000000E2ESH{n:02d}" for n in range(32)]
# A standby holds a lapsed shard within this (ruling 1056, question 12).
SHARD_TAKEOVER = 10
# The write-scale step's load (step 3d, stage T; section 15), in the scaled
# run alone: two more test-only instances of driver.py, named as the
# driver's own are (e2e/failover/driver.sql puts the accounts in drive-oms's
# write scope), idle until the step tells them to write. Each has its own
# share at its sidecar raised past what it can offer, as a launch may set
# it: what is measured is the stores' writers, never a plugin's share.
SCALE_DRIVERS = {"drive-data": "scale-data", "drive-oms": "scale-oms"}
SCALE_ROLES = {"scale-data": ["data"], "scale-oms": ["oms"]}
SCALE_LIMITS = {"inFlight": 64, "requestsPerSecond": 100000, "writeMiBPerSecond": 256}
SCALE_FILE = "/tmp/scale"
# The lake's datasets written, `drive-data:<key>`: four that are each writer
# group's own of two, by rendezvous (chosen in scale_keys).
SCALE_DATASETS = 8
# The book's partitions in the scaled run, so its accounts fall to both
# writer groups: of eight, five are group 1's of two and three group 0's,
# by rendezvous, so the step writes as many accounts of each (scale_accounts).
SCALE_PARTITIONS = 8
# Two writer groups on two databases against one on one: at least this
# ("about twice", ruled 2026-11-10). Proved off CI (PO, item 39,
# 2026-11-16): on the private cloud benchmark machine, on 3d's final head,
# its figures in the private file. CI runs the three shapes and holds that
# each group commits to its own database and the run completes; the ratio
# is computed here all the same, held or not, into SCALE_FIGURES alone.
SCALE_AT_LEAST = 1.8
SCALE_WARM_S = 15
SCALE_MEASURED_S = 45
# Every figure of the step, kept beside the run and never printed: core is
# public, and so are its CI logs (PO 2026-11-13). Not a log, so no job
# keeps it as an artifact either: a private run reads it for the claims
# ledger.
SCALE_FIGURES = os.environ.get("E2E_WRITE_SCALE_FIGURES", ".e2e-write-scale.json")

# The classes that read RPO through the stores themselves, and say it in
# their claims; after every other, the driver's rows are held all the same.
RPO_IN_CLASS = {"db-primary", "torn-write", "zone"}
# The oms driver's history (Jepsen's invoke/ok/fail/info), kept for Elle.
HISTORY = os.environ.get("E2E_FAILOVER_HISTORY", ".e2e-failover-history.edn")

# The rebalance class (step 3d; the resilience spec's section 15), after
# the two writer groups in the tolerant and ha runs: the lake's and the
# book's group-0 movers' injected lag and digest mismatch, files in their
# pods' own scratch (MERIDIAN_MOVE_LAG_FILE, MERIDIAN_MOVE_DIGEST_FILE,
# which the chart never sets), and moves.holdSeconds as the chart sets it
# by default.
REBALANCED = DRIVEN and PROFILE in ("tolerant", "ha")
MOVE_LAG_FILE = "/run/meridian-ready/move-lag"
MOVE_DIGEST_FILE = "/run/meridian-ready/move-digest"
HOLD_SECONDS = 5
# The deployment admin's password, drawn by first_run for this process
# alone: never printed, never written anywhere but as its hash.
ADMIN_PASSWORD = None

DEFAULT_FAILURES = {
    "tolerant": ["lake-writer", "bor-writer", "network-policy", "db-primary", "broker",
                 "bit-rot", "torn-write", "order-plugin", "memory-pressure", "noisy-plugin",
                 "apiserver"],
    "ha": ["lake-writer", "bor-writer", "street-writer", "instrument-writer",
           "split-brain", "node-freeze", "node", "zone", "network-policy", "db-primary", "broker",
           "bit-rot", "torn-write", "volume-lost", "replica-rot", "order-plugin",
           "memory-pressure", "noisy-plugin", "standing-coordinator", "apiserver",
           "write-ahead-bound"],
    "scaled": [],
    "reconfigure": [],
}


class Scorecard:
    def __init__(self):
        self.failures = []

    def check(self, held, said):
        print(f"    {'ok' if held else 'FAILED'}: {said}", flush=True)
        if not held:
            self.failures.append(said)

    def note(self, said):
        print(f"    - {said}", flush=True)


def run(*command, input=None, check=True, quiet=False):
    done = subprocess.run(command, capture_output=True, input=input)
    if check and done.returncode != 0:
        said = "(its output is not shown: it can quote a password)" if quiet else (
            f"{done.stdout.decode(errors='replace')}\n{done.stderr.decode(errors='replace')}")
        raise SystemExit(f"{' '.join(command[:6])} failed ({done.returncode})\n{said}")
    return done


def kubectl(*args, input=None, check=True, quiet=False):
    return run("kubectl", "--namespace", NAMESPACE, *args, input=input, check=check, quiet=quiet)


def out(*args, check=True):
    return kubectl(*args, check=check).stdout.decode().strip()


def settled_read(read, settled, seconds, every=2.0):
    """`read()` asked every `every` until `settled` says its answer has
    settled, within `seconds`: that answer, the one checked, or the last
    one read where none settled. A second read after the one that settled
    can straddle a change -- a takeover, a new timeline -- that the first
    waited out (PR CI 43810170103)."""
    said = []

    def ready():
        said[:] = [read()]
        return settled(said[0])

    happens(ready, seconds, every)
    return said[0] if said else read()


def happens(ready, seconds, every=2.0):
    """Whether `ready` comes true within `seconds`, asked every `every`. A
    command it runs that fails is "not yet": `run` stops with SystemExit,
    which `except Exception` let through, and a claim deleted between two
    reads of it ended a local run's volume-lost class."""
    started = time.time()
    while time.time() - started < seconds:
        try:
            if ready():
                return True
        except (Exception, SystemExit):
            pass
        time.sleep(every)
    return False


def wait_for(what, ready, seconds=600):
    if not happens(ready, seconds):
        raise SystemExit(f"{what} never happened, in {seconds}s")


def secret(name):
    data = json.loads(out("get", "secret", name, "-o", "json")).get("data") or {}
    return {key: base64.b64decode(value) for key, value in data.items()}


def primary(avoid=()):
    """The database's primary pod: under Patroni the one it labels so (not
    one of `avoid`, an old primary cut off and still labelled), else the one
    member."""
    said = json.loads(out("get", "pods", "-l",
                          f"app.kubernetes.io/instance={RELEASE},meridian.dev/component=database,"
                          "meridian.dev/database-role=primary", "-o", "json"))["items"]
    live = [p["metadata"]["name"] for p in said
            if not p["metadata"].get("deletionTimestamp") and p["metadata"]["name"] not in avoid]
    return live[0] if live else f"{FULL}-database-0"


def psql(sql, database="meridian", quiet=False, pod=None):
    return kubectl(
        "exec", "-i", pod or primary(), "-c", "postgres", "--",
        "psql", "-X", "-q", "-At", "-v", "ON_ERROR_STOP=1", "-U", "postgres", "-d", database,
        input=sql.encode(), quiet=quiet,
    ).stdout.decode().strip()


def database_members():
    # The scaled shape adds a record-read replica (contract v30, stage C).
    return {"ha": 3, "scaled": 4}.get(SHAPE, 1)


def pods(component):
    """The component's pods, running and not going, oldest first."""
    said = json.loads(out("get", "pods", "-l",
                          f"app.kubernetes.io/instance={RELEASE},meridian.dev/component={component}",
                          "-o", "json"))["items"]
    alive = [p for p in said
             if p["status"].get("phase") == "Running" and not p["metadata"].get("deletionTimestamp")]
    alive.sort(key=lambda p: p["metadata"]["creationTimestamp"])
    return alive


def ready(pod):
    return any(c["type"] == "Ready" and c["status"] == "True"
               for c in pod["status"].get("conditions", []))


def restarts(pod_name):
    pod = json.loads(out("get", "pod", pod_name, "-o", "json"))
    return sum(c.get("restartCount", 0) for c in pod["status"].get("containerStatuses", []))


def holder(component):
    """Who holds the component's lease, where it has one."""
    done = kubectl("get", "lease", f"{FULL}-{component}", "-o", "json", check=False)
    if done.returncode != 0:
        return ""
    # The holder is a process: its pod's name, then `_` and its start.
    held = json.loads(done.stdout).get("spec", {}).get("holderIdentity") or ""
    return held.split("_", 1)[0]


def probe(pod_name, component):
    """The member's probe of every path, as JSON; None where it could not run."""
    # Never left hanging: an exec through a node going away can wait on it.
    try:
        done = subprocess.run(["kubectl", "--namespace", NAMESPACE, "exec", pod_name, "-c", component, "--",
                               BINARY[component], "probe"], capture_output=True, timeout=15)
    except subprocess.TimeoutExpired:
        return None
    for line in done.stdout.decode(errors="replace").splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                return None
    return None


def answering(component, avoid=(), above_epoch=None):
    """A probe from one of the component's live pods (not one of `avoid`)
    whose change log answered, at an epoch above `above_epoch` if given."""
    for pod in pods(component):
        name = pod["metadata"]["name"]
        if name in avoid:
            continue
        said = probe(name, component)
        answers = (said or {}).get("answers") or {}
        if component == "launcher":
            # No change log: a launcher answers by holding the lease and serving.
            if said and said.get("member", {}).get("mode") == "writer" and said["member"].get("ready"):
                return said
            continue
        if answers.get("ok") and (above_epoch is None or answers.get("epoch", 0) > above_epoch):
            return said
    return None


def epoch_now(component, seconds=30):
    """The epoch the component answers at, asked until it answers: one
    probe that did not answer read as epoch 0, and CI 42291353103 called a
    broker class with no takeover a takeover."""
    found = {}

    def answered():
        said = answering(component)
        if said is not None:
            found["epoch"] = (said.get("answers") or {}).get("epoch", 0)
        return said is not None
    happens(answered, seconds, every=0.5)
    return found.get("epoch", 0)


def claim(s, failure, component, seconds, bound, extra=None):
    line = {
        "profile": PROFILE, "failure": failure, "component": component,
        "recovered_s": round(seconds, 2), "bound_s": bound,
        "held": bound is None or seconds <= bound,
        "command": f"make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-failover PROFILE={PROFILE} FAILURE={failure}",
        "kubernetes": kubernetes_version(),
        "run": os.environ.get("GITHUB_RUN_ID", "local"),
    }
    line.update(extra or {})
    said = claims_tool.public(line)
    print(f"CLAIM {json.dumps(said)}", flush=True)
    with open(CLAIMS, "a", encoding="utf-8") as kept:
        kept.write(json.dumps(said) + "\n")
    with open(FIGURES, "a", encoding="utf-8") as kept:
        kept.write(json.dumps(line) + "\n")
    if bound is None:
        s.check(True, f"{failure}: {component} measured (no bound is set for it yet)")
    else:
        s.check(line["held"], f"{failure}: {component} answered again within {bound}s")


def figure(what, **measured):
    """A time a check measured, kept with the run's figures and never
    printed: the check's own words say its limit and whether it held."""
    with open(FIGURES, "a", encoding="utf-8") as kept:
        kept.write(json.dumps({"note": what, "profile": PROFILE, **measured}) + "\n")


def kubernetes_version():
    try:
        said = json.loads(run("kubectl", "version", "-o", "json").stdout)
        return said.get("serverVersion", {}).get("gitVersion", "")
    except Exception:
        return ""


def took_over_record(component, epoch):
    table = {"bor": "book", "conductor": "config"}.get(component, component)
    return psql(f"select member || '|' || why from {table}_writer_took_over where epoch = {int(epoch)}")


# ── Install, first run ─────────────────────────────────────────────────────

def nodes():
    return json.loads(run("kubectl", "get", "nodes", "-o", "json").stdout)["items"]


def server_node():
    for node in nodes():
        labels = node["metadata"].get("labels", {})
        if labels.get("node-role.kubernetes.io/control-plane") == "true":
            return node["metadata"]["name"]
    return nodes()[0]["metadata"]["name"]


def broker_set():
    """The broker's StatefulSet: named by what it keeps on disk, so the set
    with JetStream's volumes (the HA profiles) is a set of its own."""
    return f"{FULL}-broker-jetstream" if SHAPE in ("ha", "scaled") else f"{FULL}-broker"


def install(s):
    repository, _, tag = IMAGE.rpartition(":")
    values = [
        "--set", "deployment.id=DEP-failover-e2e",
        "--set", "platform.address=http://127.0.0.1:9",
        "--set", f"image.repository={repository}", "--set", f"image.tag={tag}",
        # The registry stays on: the launcher, a member set of its own,
        # renders only beside it.
        "--set", "pluginStorage.enabled=false",
    ]
    if PROFILE == "tolerant":
        # A reader beside the lake's one writer (contract v30, stage C): the
        # db-primary class holds it to what the record holds after the
        # database restarts.
        values += ["--set", f"lake.readers={READERS}"]
    if PROFILE in ("ha", "scaled", "reconfigure"):
        # The conductor's standby needs the key as a Secret: the generated
        # one's volume attaches to one node. A tolerant install that is to
        # be turned up to ha has it there from the start (ruling 1074).
        workdir = tempfile.mkdtemp(prefix="failover-key-")
        key = os.path.join(workdir, "key.pem")
        run("openssl", "genpkey", "-algorithm", "ed25519", "-out", key)
        run("kubectl", "create", "namespace", NAMESPACE, check=False)
        kubectl("create", "secret", "generic", "failover-key", f"--from-file=key.pem={key}", quiet=True)
        os.remove(key)
        values += ["--set", "key.generate=false", "--set", "key.existingSecret=failover-key"]
        # The HA preset in one value (step 3b): a standby per writer, the
        # database with two standbys, the broker at three with JetStream,
        # the dashboard and analytics at two.
        if PROFILE != "reconfigure":
            values += ["--set", "resilience.profile=ha"]
            # A reader beside the lake's and the book's writer and standby
            # (contract v30, stage B; the spec's section 15 shape), each
            # serving its store's reads in the store's queue group.
            values += ["--set", f"lake.readers={READERS}", "--set", f"bor.readers={READERS}"]
            if PROFILE == "scaled":
                # A record-read replica beside the primary and its two
                # standbys (contract v30, stage C; database.brought.readReplicas).
                values += ["--set", "database.brought.readReplicas=1"]
        if TLS:
            values += ["--set", "tls.internal.enabled=true"]
        values += ["--set", "zones={" + ",".join(ZONES) + "}"]
        values += ["--set", f"database.brought.patroniImage={POSTGRES_IMAGE}"]
    public = None
    if DRIVEN:
        public = driver_signer()
        values += driver_values()
    if PROFILE == "scaled":
        # One list: a values file's sidecars[] replaces another's whole.
        # Each runs driver.py in the SDK's interop image.
        def driven(role, env):
            return {"image": INTEROP_IMAGE, "pullPolicy": "IfNotPresent",
                    "args": ["python", "-c",
                             "import os; exec(compile(os.environ['MERIDIAN_DRIVER'], 'driver.py', 'exec'))"],
                    "env": {"DRIVER_ROLE": role, "DRIVER_INSTRUMENT": DRIVER_INSTRUMENT, "DRIVER_CASH": DRIVER_CASH,
                            **env}}
        # The sharded instance and the reader whose want it shares (item 33).
        sidecars = [
            {"instanceId": SHARDED, "roles": ["data"], "shards": SHARDS, "standby": True,
             "plugin": driven("shard", {})},
            {"instanceId": SHARD_READER, "roles": ["reporting"],
             "plugin": driven("shard-reader", {"DRIVER_SUBJECTS": ",".join(SHARD_SUBJECTS)})},
        ]
        # The write-scale step's load (stage T), idle until it is told.
        public = driver_signer()
        for instance, role in SCALE_DRIVERS.items():
            sidecars.append({"instanceId": instance, "roles": SCALE_ROLES[role], "limits": SCALE_LIMITS,
                             "plugin": driven(role, {"DRIVER_ACCOUNTS": ",".join(DRIVER_ACCOUNTS),
                                                     "DRIVER_SCALE_KEYS": ",".join(scale_keys())})})
            if role == "scale-oms":
                sidecars[-1]["plugin"]["existingSecret"] = DRIVER_SIGNER
        sharded = os.path.join(tempfile.mkdtemp(prefix="failover-shards-"), "shards.json")
        with open(sharded, "w", encoding="utf-8") as kept:
            json.dump({"sidecars": sidecars}, kept)
        values += ["-f", sharded, "--set", f"bor.partitions={SCALE_PARTITIONS}"]
        for at in range(len(sidecars)):
            values += ["--set-file", f"sidecars[{at}].plugin.env.MERIDIAN_DRIVER={DRIVER_SCRIPT}"]
    run("helm", "install", RELEASE, CHART, "--namespace", NAMESPACE, "--create-namespace", *values)
    s.check(True, f"helm install, profile {PROFILE}")
    if PROFILE == "ha":
        # The book's reader may be held behind on purpose (reader_held).
        kubectl("set", "env", f"deployment/{FULL}-bor-reader", f"MERIDIAN_READER_HOLD_FILE={HOLD_FILE}")
    if REBALANCED:
        # A move's injected lag and digest mismatch (rebalance): the files
        # group 0's movers look for, which the chart never sets, set here
        # before the load starts, as set env restarts the pods.
        for component in ("lake", "bor"):
            kubectl("set", "env", f"deployment/{FULL}-{component}",
                    f"MERIDIAN_MOVE_LAG_FILE={MOVE_LAG_FILE}", f"MERIDIAN_MOVE_DIGEST_FILE={MOVE_DIGEST_FILE}")
    if public:
        driver_trusted(public)
    if PROFILE == "ha":
        # The write driver's pods beside the database and the broker on the
        # server node: what a class measures is the stores' loss, and the
        # driver is how it is read, never its subject.
        for instance in DRIVERS:
            kubectl("patch", "deployment", f"{FULL}-sidecar-{instance}", "--type", "merge", "-p",
                    json.dumps({"spec": {"template": {"spec": {
                        "nodeSelector": {"kubernetes.io/hostname": server_node()}}}}}))
        # The database's and the broker's members kept on the server node,
        # so a test that stops or pauses an agent never stops them: theirs
        # are the db-primary and broker classes, injected apart. The shape
        # run (scaled) leaves them spread over the zones.
        pin = json.dumps(pinned())
        # The database's members are made by first run, after this.
        kubectl("patch", "statefulset", f"{FULL}-database", "--type", "merge", "-p", pin)
        # The broker's servers are made at install, each with JetStream's
        # volume on the node it landed on: stopped, their claims let go,
        # and made again on the server node, before anything has used them.
        servers = json.loads(out("get", "statefulset", broker_set(), "-o", "json"))["spec"]["replicas"]
        kubectl("scale", "statefulset", broker_set(), "--replicas=0")
        wait_for("the broker's servers stopped", lambda: not out(
            "get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE},meridian.dev/component=broker",
            "-o", "name"), seconds=180)
        kubectl("delete", "pvc", "--ignore-not-found", "--wait", "--timeout=120s",
                *[f"jetstream-{broker_set()}-{i}" for i in range(servers)])
        kubectl("patch", "statefulset", broker_set(), "--type", "merge", "-p", pin)
        kubectl("scale", "statefulset", broker_set(), f"--replicas={servers}")
        # Formed again before first run starts the stores, which make their
        # streams on it: CI run 43185314225's lake write-ahead stream, made
        # as the servers came back, had no leader for ten minutes, every
        # write refused "nothing serves", until the servers' next roll.
        wait_for("the broker's servers formed again, JetStream healthy on each",
                 lambda: broker_formed(servers), seconds=300)
        cluster_services_on_server()


def pinned():
    """The pin on the database's and the broker's members in ha: the server
    node by its name, the chart's spread taken off (install, above)."""
    return {"spec": {"template": {"spec": {
        "nodeSelector": {"kubernetes.io/hostname": server_node()},
        "affinity": None, "topologySpreadConstraints": None}}}}


def pinned_sets():
    """The member sets this run pinned to the server node: ha's database
    and broker; none in another profile."""
    return [f"{FULL}-database", broker_set()] if PROFILE == "ha" else []


# The cluster's own services, of which k3s runs one each, on any node.
CLUSTER_SERVICES = ["coredns", "local-path-provisioner", "metrics-server"]


def cluster_services_on_server():
    """The cluster's own services kept on the server node, beside its API
    server, so an agent stopped or paused never takes them. CI runs
    42288351455 and 42300989974 stopped the agent holding k3s's one CoreDNS
    (42298349697 paused it): every name in the cluster went unanswered until
    the node was marked lost, so the probe, resolving the broker's name,
    could not answer on the live member either. What a failure class
    measures is a member's node lost, not the cluster's DNS, which a cluster
    built for HA runs replicated across its nodes."""
    pin = json.dumps({"spec": {"template": {"spec": {
        "nodeSelector": {"kubernetes.io/hostname": server_node()}}}}})
    for name in CLUSTER_SERVICES:
        run("kubectl", "--namespace", "kube-system", "patch", "deployment", name,
            "--type", "merge", "-p", pin)
    for name in CLUSTER_SERVICES:
        run("kubectl", "--namespace", "kube-system", "rollout", "status", f"deployment/{name}",
            "--timeout=180s")


def cluster_services_where():
    """The node each of the cluster's own services runs on."""
    said = json.loads(run("kubectl", "--namespace", "kube-system", "get", "pods", "-o", "json").stdout)
    where = {}
    for pod in said["items"]:
        name = pod["metadata"]["name"]
        service = next((s for s in CLUSTER_SERVICES if name.startswith(f"{s}-")), None)
        if service and pod["status"].get("phase") == "Running" and not pod["metadata"].get("deletionTimestamp"):
            where.setdefault(service, set()).add(pod["spec"].get("nodeName", ""))
    return where


def first_run(s):
    """What the wizard's Job writes, written directly, then what it does:
    the database, its roles, the URLs; the conductor and the dashboard
    restarted; the Job ended and its rights given up (as e2e/backup)."""
    wait_for("the database's StatefulSet", lambda: kubectl(
        "get", "statefulset", f"{FULL}-database", check=False).returncode == 0)
    kubectl("scale", "statefulset", f"{FULL}-database", f"--replicas={database_members()}")
    wait_for("the database's primary", lambda: kubectl(
        "exec", primary(), "-c", "postgres", "--", "pg_isready", "-h", "127.0.0.1", "-U", "postgres",
        check=False).returncode == 0
        and psql("select not pg_is_in_recovery()", database="postgres") == "t")
    passwords = secret(f"{FULL}-database-passwords")
    serving, migrating = passwords["serving"].decode(), passwords["migrating"].decode()
    psql(f"create role meridian_app login password '{serving}';\n"
         f"create role meridian_migrate login password '{migrating}';\n"
         "create database meridian;\n", database="postgres", quiet=True)
    psql("grant connect on database meridian to meridian_app, meridian_migrate;\n"
         "grant usage on schema public to meridian_app, meridian_migrate;\n"
         "grant create on schema public to meridian_migrate;\n"
         "revoke create on schema public from meridian_app, public;\n")
    host = f"{FULL}-database.{NAMESPACE}.svc.cluster.local"
    database = json.loads(out("get", "secret", f"{FULL}-database", "-o", "json"))
    database["data"] = {
        "url": base64.b64encode(
            f"postgres://meridian_app:{serving}@{host}:5432/meridian?sslmode=disable".encode()).decode(),
        "migrate-url": base64.b64encode(
            f"postgres://meridian_migrate:{migrating}@{host}:5432/meridian?sslmode=disable".encode()).decode(),
    }
    kubectl("replace", "-f", "-", input=json.dumps(database).encode(), quiet=True)
    addresses = json.loads(out("get", "secret", "meridian-addresses", "-o", "json"))
    addresses["data"] = {
        "dashboard-url": base64.b64encode(b"http://failover-e2e.localhost").decode(),
        "administrator-login": base64.b64encode(ADMIN.encode()).decode(),
        # The deployment holds its own accounts, as the wizard says for a
        # local administrator (local|alice): so the dashboard keeps its
        # tables -- the inbox's notices among them -- in the database every
        # copy shares, and write-ahead-bound reads a gap's notice where an
        # admin's inbox reads it (contract v30, stage D). Without it the
        # dashboard signed nobody in and kept its notices in each copy's
        # memory (failover-repeat 43071322484, PR CI 43070882389).
        "local-accounts": base64.b64encode(b"on").decode(),
    }
    if REBALANCED:
        # Her account's name and password (step 3d): the rebalance class
        # signs her in, as a deployment admin does, to move partitions by the
        # dashboard's own tools and dialog. The password is drawn here and
        # kept in this process alone, its Argon2id hash as the wizard's Job
        # writes it; neither is ever printed.
        addresses["data"]["local-account-name"] = base64.b64encode(ADMIN.split("|", 1)[1].encode()).decode()
        addresses["data"]["administrator-password-hash"] = base64.b64encode(
            admin_password_hash().encode()).decode()
    kubectl("replace", "-f", "-", input=json.dumps(addresses).encode(), quiet=True)
    del serving, migrating, passwords
    jobs = json.loads(out("get", "jobs", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]
    job = next(j for j in jobs
               if j["spec"]["template"]["metadata"]["labels"].get("meridian.dev/component") == "first-run")
    env = {e["name"]: e.get("value", "") for e in job["spec"]["template"]["spec"]["containers"][0].get("env", [])}
    restarted = [n.strip() for n in env["MERIDIAN_FIRST_RUN_RESTART"].split(",") if n.strip()]
    for name in restarted:
        kubectl("rollout", "restart", f"deployment/{name}")
    kubectl("delete", "pod", "-l", f"job-name={job['metadata']['name']}", "--wait")
    wait_for("the first-run Job to finish", lambda: json.loads(out(
        "get", "job", job["metadata"]["name"], "-o", "json"))["status"].get("succeeded"), seconds=180)
    kubectl("delete", "rolebinding", env["MERIDIAN_FIRST_RUN_BINDING"], "--ignore-not-found")
    if not happens(all_ready, 900):
        # Why, in the job's own log (CI 43955282180: the lake's write-ahead
        # stream could not be placed for fifteen minutes, and nothing said
        # what JetStream held): the deployments not available, and each
        # broker server's JetStream as it sees itself.
        deployments = json.loads(out("get", "deployments", "-l", f"app.kubernetes.io/instance={RELEASE}",
                                     "-o", "json", check=False) or "{}").get("items", [])
        unready = [f"{d['metadata']['name']} {d.get('status', {}).get('availableReplicas', 0)}/"
                   f"{d.get('spec', {}).get('replicas')}" for d in deployments
                   if d.get("status", {}).get("availableReplicas", 0) != d.get("spec", {}).get("replicas")]
        print(f"A: not available after 900s: {', '.join(unready) or 'none'}", flush=True)
        for line in broker_jetstream_said():
            print(f"A: {line}", flush=True)
        cluster_said("the install never got every component ready")
        raise SystemExit("every component ready never happened, in 900s")
    s.check(True, "first run applied; every component ready")


def patroni_api(path, member=None, method="GET", body=None):
    """A shell command, run in a database member, asking Patroni's REST API
    there for `path`: over TLS by the member's own Service's name, verified
    against the internal CA, where internal TLS is on (ruled 2026-11-08);
    by 127.0.0.1 in the clear otherwise. Unsafe methods carry Patroni's
    credential, from the member's own environment."""
    auth = '-u "patroni:$PATRONI_RESTAPI_PASSWORD" ' if method != "GET" else ""
    data = f"-H 'Content-Type: application/json' -d '{body}' " if body is not None else ""
    if TLS:
        name = f"{member or '$POD_NAME'}.{NAMESPACE}.svc.cluster.local"
        return (f"curl -fsS --cacert /run/meridian-tls/ca.crt -X {method} {auth}{data}"
                f"https://{name}:8008/{path}")
    return f"curl -fsS -X {method} {auth}{data}http://127.0.0.1:8008/{path}"


def internal_tls(s):
    """Internal TLS on (ruling 1084): every store's connection to the
    database over TLS, and a plaintext one refused, by the database and by
    the broker; a TLS one verified against the chart's CA by the name the
    stores use. And Patroni's REST API over TLS alone (ruled 2026-11-08):
    each member reached by another by its own name, verified against the
    CA, a plaintext request answered nothing, and no member's Patroni
    saying a request to another failed its certificate."""
    print("T: internal TLS: plaintext refused", flush=True)
    host = f"{FULL}-database.{NAMESPACE}.svc.cluster.local"
    member = primary()

    def connects(mode):
        script = (f'PGPASSWORD="${{PATRONI_SUPERUSER_PASSWORD:-$POSTGRES_PASSWORD}}" '
                  f'psql "host={host} dbname=meridian user=postgres {mode} connect_timeout=5" '
                  "-X -At -c 'select ssl from pg_stat_ssl where pid = pg_backend_pid()' 2>&1")
        done = kubectl("exec", member, "-c", "postgres", "--", "sh", "-c", script, check=False)
        return done.returncode, done.stdout.decode(errors="replace").strip()

    code, said = connects("sslmode=disable")
    s.check(code != 0 and "no encryption" in said,
            "the database refuses a plaintext connection from another pod's address "
            f"({said.splitlines()[-1] if said else 'nothing said'})")
    code, said = connects("sslmode=verify-full sslrootcert=/run/meridian-tls/ca.crt")
    s.check(code == 0 and said == "t",
            "the database serves TLS under the chart's CA, verified by the stores' name for it")
    # Every serving connection from a store over TLS, none in the clear.
    counted = psql("select coalesce(sum(case when s.ssl then 1 else 0 end), 0) || ' ' || "
                   "coalesce(sum(case when s.ssl then 0 else 1 end), 0) "
                   "from pg_stat_activity a join pg_stat_ssl s using (pid) "
                   "where a.usename in ('meridian_app', 'meridian_migrate') "
                   "and a.client_addr is not null")
    secured, clear = (int(n) for n in counted.split())
    s.check(secured > 0 and clear == 0,
            f"the stores' connections: {secured} over TLS, {clear} in the clear")
    # The broker: a client that does not speak TLS gets no further than the
    # server's greeting, which says it is required.
    broker = f"{FULL}-broker.{NAMESPACE}.svc.cluster.local"
    script = (f"(printf 'CONNECT {{\"verbose\":false}}\\r\\nPING\\r\\n'; sleep 2) "
              f"| nc -w 4 {broker} 4222 2>&1")
    said = kubectl("exec", member, "-c", "postgres", "--", "sh", "-c", script,
                   check=False).stdout.decode(errors="replace")
    s.check('"tls_required":true' in said and "PONG" not in said,
            "the broker requires TLS of every client and answers a plaintext one nothing "
            f"({'tls_required' if 'tls_required' in said else 'no greeting'}"
            f"{', PONG' if 'PONG' in said else ''})")
    # Patroni's REST API: every other member asked from this one by its own
    # name over TLS, and in the clear by its address answered nothing.
    members = [p["metadata"]["name"] for p in pods("database")]
    for other in members:
        if other == member:
            continue
        done = kubectl("exec", member, "-c", "postgres", "--", "sh", "-c",
                       patroni_api("patroni", member=other), check=False)
        said = done.stdout.decode(errors="replace")
        s.check(done.returncode == 0 and f'"name": "{other}"' in said.replace('":"', '": "'),
                f"Patroni's REST API on {other} answers {member} over TLS, verified by {other}'s own name"
                + ("" if done.returncode == 0 else f": {done.stderr.decode(errors='replace').strip()[-200:]}"))
        address = next(p["status"]["podIP"] for p in pods("database") if p["metadata"]["name"] == other)
        clear = kubectl("exec", member, "-c", "postgres", "--", "sh", "-c",
                        f"curl -fsS -m 5 http://{address}:8008/patroni", check=False)
        s.check(clear.returncode != 0,
                f"Patroni's REST API on {other} answers a plaintext request nothing")
    # A request whose certificate its Patroni could not verify, as the
    # client; not the server's own complaint about the plaintext request
    # above ([SSL: HTTP_REQUEST]), which is this check's doing.
    for each in members:
        logs = kubectl("logs", each, "-c", "postgres", check=False).stdout.decode(errors="replace")
        failed = [line for line in logs.splitlines()
                  if "CERTIFICATE_VERIFY_FAILED" in line or "certificate verify failed" in line]
        s.check(not failed, f"{each}'s Patroni reached the other members' REST APIs with their "
                f"certificates verified" + (f": {failed[-1][-200:]}" if failed else ""))


def all_ready():
    names = out("get", "deployments", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "name").split()
    if not names:
        return False
    return kubectl("wait", "--for=condition=Available", "--timeout=5s", *names, check=False).returncode == 0


def members_settled(component):
    """Every member running and ready: two with a standby."""
    want = 2 if SHAPE in ("ha", "scaled") else 1
    live = pods(component)
    return len(live) == want and all(ready(p) for p in live)


def modes_of(component, seconds=60):
    """The members' modes, sorted, once they are one writer and one standby
    or `seconds` have passed. Asked again rather than once: a probe through
    kubectl exec that did not run (its node's kubelet busy, a run's 15 s
    timeout) reads as no mode, and a local run failed network-policy on
    (['', 'standby']) with the writer serving."""
    found = {}

    def settled():
        found["modes"] = sorted((probe(p["metadata"]["name"], component) or {}).get("member", {}).get("mode", "")
                                for p in pods(component))
        return found["modes"] == ["standby", "writer"]
    happens(settled, seconds, every=1)
    return found.get("modes", [])


# ── The shape of the HA configuration (e2e-cluster-scaled) ─────────────────

def rendered(component):
    return kubectl("get", "deployment", f"{FULL}-{component}", check=False).returncode == 0


def shape(s):
    print("S: the HA configuration's shape", flush=True)
    for component in STORES:
        if not rendered(component):
            s.check(False, f"{component}: the chart rendered no {component}")
            continue
        wait_for(f"{component}'s two members ready", lambda c=component: members_settled(c), seconds=300)
        live = pods(component)
        on = {p["spec"]["nodeName"] for p in live}
        s.check(len(on) == 2, f"{component}: writer and standby on two nodes ({', '.join(sorted(on))})")
        zones = {next((n["metadata"]["labels"].get("topology.kubernetes.io/zone") for n in nodes()
                       if n["metadata"]["name"] == node), "") for node in on}
        s.check(len(zones) == 2 and "" not in zones, f"{component}: in two zones ({', '.join(sorted(zones))})")
        held = holder(component)
        s.check(held in {p["metadata"]["name"] for p in live}, f"{component}: its lease held by a member ({held})")
        modes = modes_of(component)
        s.check(modes == ["standby", "writer"], f"{component}: one writer, one standby ({modes})")
        budget = kubectl("get", "pdb", f"{FULL}-{component}", check=False).returncode == 0
        s.check(budget, f"{component}: a disruption budget over its members")
    # The readers (contract v30, stage B): each store's reader ready, in the
    # reader mode, a Deployment of its own beside the writer's.
    for component in READ_STORES:
        reader = f"{component}-reader"
        settled = happens(lambda r=reader: len(pods(r)) == READERS and all(ready(p) for p in pods(r)), 300)
        s.check(settled, f"{component}: {READERS} reader ready ({len(pods(reader))})")
        modes = [(probe(p["metadata"]["name"], component) or {}).get("member", {}).get("mode", "")
                 for p in pods(reader)]
        s.check(modes == ["reader"] * READERS, f"{component}: its reader says it reads ({modes})")
    tolerated = all(
        any(t.get("key") == "node.kubernetes.io/unreachable" and t.get("tolerationSeconds") == 30
            for t in p["spec"].get("tolerations", []))
        for c in STORES + ["dashboard"] for p in pods(c))
    s.check(tolerated, "every stateless pod tolerates an unreachable node for 30 s")
    # The sets the preset turns up beside the stores (step 3b part 2): the
    # database's three members under Patroni, the broker's three servers,
    # the dashboard's two copies and analytics' two of each, every set spread
    # over three zones (or two, for two), with a budget over it.
    for component, want in (("database", database_members()), ("broker", 3), ("dashboard", 2),
                            ("analytics-coordinator", 2), ("analytics-worker", 2)):
        settled = happens(lambda c=component, n=want: len(pods(c)) == n and all(ready(p) for p in pods(c)), 300)
        live = pods(component)
        s.check(settled, f"{component}: {want} members ready ({len(live)})")
        if PROFILE == "ha" and component in ("database", "broker"):
            # Kept on the server node in the failover run (install), so the
            # agents' classes are injected apart; the shape run spreads them.
            s.note(f"{component}: kept on the server node for this run")
        else:
            zones = {zone_of(p["spec"]["nodeName"]) for p in live}
            s.check(len(zones) == min(want, 3) and "" not in zones,
                    f"{component}: one member per zone ({', '.join(sorted(zones))})")
        budget = kubectl("get", "pdb", f"{FULL}-{component}", check=False).returncode == 0
        s.check(budget, f"{component}: a disruption budget over its members")
    roles = sorted(p["metadata"]["labels"].get("meridian.dev/database-role", "") for p in pods("database"))
    s.check(roles == ["primary"] + ["replica"] * (database_members() - 1),
            f"database: one primary, its standbys and record-read replicas ({roles})")
    if SHAPE == "scaled":
        # Each store's reader wired to the record-read replica by the chart
        # itself (contract v30, stage C2, approved 2026-11-11): its read pool
        # is on that member, and on no standby, which serve no reads.
        replica = f"{FULL}-database-{database_members() - 1}"
        standbys = [f"{FULL}-database-{n}" for n in (1, 2)]

        def held_on(member, address):
            said = psql(f"select count(*) from pg_stat_activity where client_addr = '{address}'",
                        database="postgres", quiet=True, pod=member)
            return int(said or 0)
        for component in READ_STORES:
            for pod in pods(f"{component}-reader"):
                address = pod["status"].get("podIP", "")
                on = happens(lambda a=address: held_on(replica, a) > 0, 120)
                s.check(on, f"{component}: its reader reads the record's past from the record-read replica "
                        f"({replica}; {held_on(replica, address)} connection(s) from {pod['metadata']['name']})")
                elsewhere = sum(held_on(member, address) for member in standbys)
                s.check(elsewhere == 0, f"{component}: its reader holds nothing on a standby ({elsewhere})")
    # The synchronous standbys a quorum (contract v30, stage C): ANY 1 of
    # the two, each standing in it, strict.
    quorum = psql("select count(*) from pg_stat_replication where sync_state = 'quorum'", database="postgres")
    names = psql("show synchronous_standby_names", database="postgres")
    s.check(quorum == "2" and names.startswith("ANY 1 ("),
            f"database: both standbys in a quorum of one ({quorum}; {names})")
    if SHAPE == "scaled":
        # The record-read replica outside the quorum (nosync): asynchronous.
        streaming = psql("select count(*) from pg_stat_replication where sync_state = 'async'",
                         database="postgres")
        s.check(streaming == "1", f"database: the record-read replica asynchronous, outside the quorum "
                                  f"({streaming})")
    checksums = psql("show data_checksums", database="postgres")
    s.check(checksums == "on", f"database: made with page checksums ({checksums})")
    # Every store's probe names the primary it reaches, and its checksums.
    for component in ("bor", "lake"):
        said = probe(pods(component)[0]["metadata"]["name"], component) or {}
        database = next((p for p in said.get("paths", []) if p.get("path") == "database"), {})
        s.check(database.get("primary") is True and database.get("data_checksums") == "on",
                f"{component}: its probe names a primary with checksums ({database.get('primary')}, "
                f"{database.get('data_checksums')})")
    if PROFILE == "ha":
        where = cluster_services_where()
        s.check(all(on == {server_node()} for on in where.values()) and len(where) == len(CLUSTER_SERVICES),
                f"the cluster's own services on the server node, apart from the classes "
                f"({', '.join(f'{k}: {sorted(v)}' for k, v in sorted(where.items()))})")
    jetstream_rolled(s)


# ── A plugin instance's shards and its standby pod (step 3d) ───────────────

def shard_pods():
    """The sharded instance's pods, running and not going."""
    return [p for p in pods("sidecar") if p["metadata"]["labels"].get("meridian.dev/instance") == SHARDED]


def shard_holder(shard):
    """The pod holding a shard's Lease, and its holder as written (a
    process: the pod's name, `_`, its start); empty where none does."""
    done = kubectl("get", "lease", f"{SHARDED}-shard-{shard}", "-o", "json", check=False)
    if done.returncode != 0:
        return "", ""
    held = json.loads(done.stdout).get("spec", {}).get("holderIdentity") or ""
    return held.split("_", 1)[0], held


def shard_log(pod_name, container="sidecar", stamped=False):
    said = kubectl("logs", pod_name, "-c", container, *(["--timestamps"] if stamped else []), check=False)
    return said.stdout.decode(errors="replace")


def said_at(pod_name, words):
    """When the pod's sidecar, as it runs now, last said `words`: seconds
    since the epoch by the cluster's own stamp on the line, or none."""
    for line in reversed(shard_log(pod_name, stamped=True).splitlines()):
        if words in line:
            stamp = line.split(" ", 1)[0]
            whole, _, rest = stamp.rstrip("Z").partition(".")
            try:
                at = datetime.datetime.strptime(whole, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.timezone.utc)
            except ValueError:
                return None
            return at.timestamp() + float("0." + (rest or "0"))
    return None


def shard_admission(s, deployment):
    """The launcher's admission policy (S2), asked of the API server
    without making anything: the launcher's account may make a Deployment
    as this instance's is, and not one whose plugin container mounts the
    members' token, nor one under another account."""
    policy = kubectl("get", "validatingadmissionpolicy", f"{FULL}-launcher", check=False)
    if policy.returncode != 0:
        s.check(False, "shards: the launcher's admission policy is not there to hold")
        return
    made = {"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": f"{FULL}-plugin-admission-proof", "namespace": NAMESPACE,
                         "labels": {"meridian.dev/launched": "true"}},
            "spec": json.loads(json.dumps(deployment["spec"]))}
    made["spec"]["selector"] = {"matchLabels": {"meridian.dev/instance": "admission-proof"}}
    made["spec"]["template"]["metadata"]["labels"] = {"meridian.dev/instance": "admission-proof",
                                                      "meridian.dev/launched": "true"}
    made["metadata"]["labels"]["meridian.dev/instance"] = "admission-proof"
    # Its own instance's account (item 31), as the launcher would run it.
    made["spec"]["template"]["spec"]["serviceAccountName"] = shard_accounts.account_of(FULL, "admission-proof")

    def asked(change):
        tried = json.loads(json.dumps(made))
        change(tried["spec"]["template"]["spec"])
        done = kubectl("create", "--dry-run=server", f"--as=system:serviceaccount:{NAMESPACE}:{FULL}-launcher",
                       "-f", "-", input=json.dumps(tried).encode(), check=False, quiet=True)
        return done.returncode == 0, (done.stderr or b"").decode(errors="replace").strip()[-300:]

    def to_plugin(pod):
        plugin = next(c for c in pod["containers"] if c["name"] == "plugin")
        plugin.setdefault("volumeMounts", []).append({"name": "member-token", "mountPath": "/tmp/token"})

    admitted, why = asked(lambda pod: None)
    s.check(admitted, f"shards: the launcher may make an instance of several pods as the chart shapes it ({why})")
    for what, change, says in (
            ("whose plugin container mounts the members' token", to_plugin, "sidecar's container alone"),
            ("that mounts the token by default",
             lambda pod: pod.__setitem__("automountServiceAccountToken", True), "sidecar's container alone"),
            ("under the conductor's account",
             lambda pod: pod.__setitem__("serviceAccountName", f"{FULL}-conductor"), "runs under no account")):
        admitted, why = asked(change)
        s.check(not admitted and says in why, f"shards: the launcher may not make one {what} ({why})")


def shard_epoch(shard):
    """The shard's epoch as the lake's database 0 holds it: 0 before any take."""
    said = psql(f"select coalesce(max(epoch), 0) from lake_shard_fence "
                f"where instance = '{SHARDED}' and shard = {shard}", quiet=True)
    return int(said or 0)


def shard_epochs(shard):
    """The shard's epoch as every lake and book database holds it (review c5a
    M5), by (store, database): 0 where one holds none."""
    held = {}
    for store, table in (("lake", "lake_shard_fence"), ("book", "book_shard_fence")):
        # store_databases (below) is the run's one: (d, its primary's pod).
        # c5a's second definition of the name, above it, was overridden by
        # it, and its label built from a pair broke the scaled run (CI
        # 43920761210).
        for d, pod in store_databases(store):
            said = psql(f"select coalesce(max(epoch), 0) from {table} "
                        f"where instance = '{SHARDED}' and shard = {shard}",
                        quiet=True, pod=pod)
            held[(store, d)] = int(said or 0)
    return held


def shard_takes():
    """How many shard takes the lake's control partition records."""
    said = psql("select count(*) from lake_control_change where kind = 'meridian.v1.ShardTookOver'",
                quiet=True)
    return int(said or 0)


def sidecar_status(pod):
    """The pod's sidecar container's status, as the kubelet reports it."""
    return next((c for c in pod["status"].get("containerStatuses", []) if c["name"] == "sidecar"), {})


def shard_restarts_said(s, when):
    """Each shard pod's sidecar log before its last restart, where it
    restarted, said in the run's own log: a rollout replaces the pods, and
    those logs with them (review c5a5 F3: every shard sidecar had ended
    once at its start, and why was lost with its pod)."""
    for pod in shard_pods():
        name, sidecar = pod["metadata"]["name"], sidecar_status(pod)
        if not sidecar.get("restartCount"):
            continue
        ended = sidecar.get("lastState", {}).get("terminated", {})
        said = kubectl("logs", name, "-c", "sidecar", "--previous", "--timestamps", "--tail=80", check=False)
        s.note(f"shards, {when}: {name}'s sidecar restarted {sidecar['restartCount']} time(s); its last run ended "
               f"{ended.get('reason')} (exit {ended.get('exitCode')}) at {ended.get('finishedAt')}, saying:")
        for line in (said.stdout or said.stderr).decode(errors="replace").splitlines():
            print(f"        {line}", flush=True)


# A shard Lease's lapse (leases.durationSeconds).
SHARD_LAPSE_S = 6
# The kubelet's longest wait before it starts a container that ended again
# (CrashLoopBackOff's cap): a sidecar that lost its shard ends, and comes
# back as the standby only after it (review c5a5 F3).
SIDECAR_BACK_OFF_S = 300


def the_standby():
    """The shard pod holding no shard's Lease; empty where none does."""
    holders = {shard_holder(n)[0] for n in range(SHARDS)}
    return next((p["metadata"]["name"] for p in shard_pods() if p["metadata"]["name"] not in holders), "")


def standing_by(pod_name):
    """Whether the pod's sidecar runs, started, and said in its current run
    that it stands by at least a Lease's lapse ago: a standby that takes a
    lapsed shard within the bound, its first read of the Lease done (review
    c5a5 F2: a standby whose sidecar was waiting out its back-off took the
    killed shard's Lease 23 s after the kill)."""
    sidecar = sidecar_status(json.loads(out("get", "pod", pod_name, "-o", "json")))
    if "running" not in sidecar.get("state", {}) or not sidecar.get("started"):
        return False
    said = said_at(pod_name, "standing by")
    return said is not None and time.time() - said >= SHARD_LAPSE_S


def shard_said_kept(when, pod_names, began, say=False):
    """Each pod's sidecar log, its run before and as it runs now, kept
    beside the run (.e2e-shards-<n>.log, which CI keeps with
    .e2e-cluster.log), in the order given: the last pod's current log ends
    the file, the tail CI prints. With `say`, its lines about Leases from
    30 s before `began` are printed to the job log as well."""
    SHARDS_KEPT.append(when)
    name = f".e2e-shards-{len(SHARDS_KEPT)}.log"
    parts = [f"== {when}, began {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(began))}"]
    for pod_name in [p for p in pod_names if p]:
        for previous in (("--previous",), ()):
            said = kubectl("logs", pod_name, "-c", "sidecar", "--timestamps", *previous, check=False)
            text = (said.stdout or said.stderr).decode(errors="replace")
            parts.append(f"== {pod_name}, sidecar{', before, if it restarted' if previous else ''}")
            parts.append(text)
            if say and not previous:
                since = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(began - 30))
                for line in text.splitlines():
                    if line[:19] >= since and ("lease" in line or "standing by" in line):
                        print(f"      {pod_name}: {line}", flush=True)
    with open(name, "w", encoding="utf-8") as kept:
        kept.write("\n".join(parts))
    print(f"    - shards: {when}: each pod's sidecar log kept in {name}", flush=True)


SHARDS_KEPT = []


def a_standby_stands_by(s, before):
    """Before a takeover's bound is measured: the standby pod's sidecar
    standing by for a lapse, checked."""
    standby = ""

    def stands():
        nonlocal standby
        standby = the_standby()
        return bool(standby) and standing_by(standby)
    stood = happens(stands, SIDECAR_BACK_OFF_S + 30, every=1.0)
    s.check(stood, f"shards: before {before}, the standby's sidecar has stood by for a lapse ({standby or 'none'})")
    return stood


# How long the shard pods are cut from the API server: well past a Lease's
# lapse (leases.durationSeconds, 6 s), so a shard stopping by the clock would
# have stopped.
SHARD_CUT_S = 15
LOST = "the shard's lease is lost"
# What a shard's sidecar says as it stands by again in its process, its
# shard lost (item 49, ruled 2026-11-16): it no longer ends.
STANDING_AGAIN = "; standing by again"
# What it says once it stands by, every shard's Lease another's.
STOOD_BY = "standing by, serving nothing"
# What it says as a store refuses a write of its as fenced (item 32).
FENCED = "a store refused a write as fenced"


def line_at(line):
    """A stamped log line's time, in seconds since the epoch; 0 where it
    has none."""
    whole, _, rest = line.split(" ", 1)[0].rstrip("Z").partition(".")
    try:
        at = datetime.datetime.strptime(whole, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.timezone.utc)
    except ValueError:
        return 0
    return at.timestamp() + float("0." + (rest or "0"))


def shard_lines(pod_name, path):
    """A file the shard driver keeps in its pod's scratch, by line."""
    done = kubectl("exec", pod_name, "-c", "plugin", "--", "cat", path, check=False)
    return done.stdout.decode(errors="replace").splitlines() if done.returncode == 0 else []


# ── Each subject served once (step 3d, checkpoint 5, item 33) ──────────────
#
# What each shard pod served, by the shard driver's own record of what the
# lake acknowledged (driver.py: /tmp/served.log, `SERVED <pod> <shard>
# <subject> <unix s> <unix ms>`, a line per subject a second): read from
# each pod while it is there and kept here, since a pod deleted takes its
# file with it. A pod's lines after its last read -- one rolled or scaled
# away -- are not judged.
SERVED = {}
# What one pod serves now: the subjects it recorded within this many
# seconds. It records each once a second.
SERVING_S = 4
# Two seconds a pod recorded one subject in, this far apart or less, are one
# stretch of its serving it.
STRETCH_GAP_S = 3
# Each window in which the live shards changed, (what, from, until) in
# seconds since the epoch: the shards settling, each lapse and take, the
# roll and the scaling -- the only time two pods may serve one subject.
LAPSES = []
# The cluster's clock and the run's: a stamp of either may be this far off
# the other's (said_at's lines are judged so too).
SKEW_S = 1


def served_read():
    """Each shard pod's served lines read and kept, by pod: a list of
    (shard, subject, second, acknowledged at). A line is the pod's it is
    read from: the chart gives a plugin's container the instance's name as
    its hostname, so the pod a driver says is the instance's, not its own."""
    for pod in shard_pods():
        name = pod["metadata"]["name"]
        done = kubectl("exec", name, "-c", "plugin", "--", "cat", "/tmp/served.log", check=False)
        if done.returncode != 0:
            continue
        lines = []
        for line in done.stdout.decode(errors="replace").splitlines():
            parts = line.split()
            if len(parts) == 6 and parts[0] == "SERVED":
                lines.append((parts[2], parts[3], int(parts[4]), int(parts[5]) / 1000))
        if len(lines) >= len(SERVED.get(name, [])):
            SERVED[name] = lines
    return SERVED


def served_between(since, until=None):
    """Each pod's subjects it served between two times (by acknowledgement),
    from what was last read; a pod that served none is left out."""
    held = {}
    for pod, lines in SERVED.items():
        subjects = {subject for _, subject, _, at in lines if at >= since and (until is None or at < until)}
        if subjects:
            held[pod] = subjects
    return held


def served_once(s, when, seconds=120):
    """Each subject served by one shard alone: within `seconds`, a moment
    at which SHARDS pods served in the last SERVING_S, their sets cover all
    of the subjects, and none of them twice; then each held. The wait asks
    what the checks ask (PR 147's CI, after the scale back: the pod that
    stayed still covered all of them within the window, the wait passed
    on its first look, and the check then found one pod serving)."""
    now = {}

    def once():
        served_read()
        now.clear()
        now.update(served_between(time.time() - SERVING_S))
        sets = list(now.values())
        return (len(now) == SHARDS
                and set().union(*sets) >= set(SHARD_SUBJECTS)
                and all(sum(subject in held for held in sets) == 1 for subject in SHARD_SUBJECTS))
    happens(once, seconds, every=1.0)
    sets = list(now.values())
    covered = set().union(*sets) if sets else set()
    twice = sorted(subject for subject in SHARD_SUBJECTS if sum(subject in held for held in sets) > 1)
    counts = {pod.rsplit("-", 1)[-1]: len(held) for pod, held in sorted(now.items())}
    s.check(covered >= set(SHARD_SUBJECTS),
            f"served once, {when}: the shards' served sets cover all {len(SHARD_SUBJECTS)} subjects "
            f"({len(covered & set(SHARD_SUBJECTS))} served; by pod {counts})")
    s.check(len(now) == SHARDS and not twice,
            f"served once, {when}: the {SHARDS} shards' served sets are disjoint, the standby's empty "
            f"({len(now)} pods serving; {len(twice)} subject(s) served by two{': ' + ', '.join(twice[:4]) if twice else ''})")
    return now


def served_alone(s, stayed, seconds=90):
    """The shard that stayed, scaled to one, serves every subject alone
    (item 33), waited for as it is judged: the only pod with a line in the
    last SERVING_S is `stayed`, and its set covers every subject. A pod
    scaled away keeps the lines it was last read with, so the wait waits
    them out (PR CI 44615932110: a wait on the pods' union passed while the
    gone pod's lines were still in the window, and the check then failed)."""
    now = {}

    def alone():
        served_read()
        now.clear()
        now.update(served_between(time.time() - SERVING_S))
        return list(now) == [stayed] and now[stayed] >= set(SHARD_SUBJECTS)
    happens(alone, seconds, every=1.0)
    s.check(list(now) == [stayed] and now[stayed] >= set(SHARD_SUBJECTS),
            f"served once: the shard that stayed alone serves all {len(SHARD_SUBJECTS)} subjects "
            f"({ {pod.rsplit('-', 1)[-1]: len(held) for pod, held in now.items()} })")
    return now


def served_taken(s, what, lost_pod, began, taker):
    """After a lapse: the subjects `lost_pod` served in the SERVING_S before
    `began` -- the lost shard's -- each served by `taker` within the
    takeover's bound of `began`."""
    served_read()
    lost = served_between(began - SERVING_S - SKEW_S, began).get(lost_pod, set())

    def first_served():
        firsts = {}
        for _, subject, _, at in SERVED.get(taker, []):
            if subject in lost and at >= began - SKEW_S:
                firsts[subject] = min(firsts.get(subject, at), at)
        return firsts
    happens(lambda: served_read() is not None and set(first_served()) >= lost, SHARD_TAKEOVER + 20, every=1.0)
    firsts = first_served()
    late = max((at - began for at in firsts.values()), default=None)
    unserved = sorted(lost - set(firsts))
    held = bool(lost) and not unserved and late <= SHARD_TAKEOVER
    # The time measured is said only where the bound was not held.
    s.check(held, f"served once, {what}: the taker serves each of the lost shard's {len(lost)} subjects within "
                  f"{SHARD_TAKEOVER} s" + ("" if held else
                  f" ({'never' if late is None else round(late, 2)} s for the last; {len(unserved)} not served"
                  f"{': ' + ', '.join(unserved[:4]) if unserved else ''}; {lost_pod} served {len(lost)} in the "
                  f"{SERVING_S} s before, {taker} took)"))
    figure(f"served once: {what}, the lost shard's subjects served again", serving_s=late,
           subjects=len(lost), unserved=len(unserved))


# Each take of shard 1 not yet judged: (what, the pod that lost it, when,
# the pod that took it), its subjects judged after the second kill
# (served_taken), or at the step's end where it ended before.
TAKES = []


def lapse(what, began):
    """A window in which the live shards changed, from `began` to now."""
    LAPSES.append((what, began, time.time()))


def overlap_in_lapses(s):
    """Two pods served one subject only inside a lapse's window: each pod's
    stretches of serving each subject (its seconds recorded, STRETCH_GAP_S
    apart at most), and every stretch of two pods' that overlaps by a second
    or more lies within a window LAPSES holds, give or take the clocks'."""
    served_read()
    stretches = {}
    for pod, lines in SERVED.items():
        seconds = {}
        for _, subject, second, _ in lines:
            seconds.setdefault(subject, set()).add(second)
        for subject, held in seconds.items():
            ordered = sorted(held)
            start = last = ordered[0]
            for second in ordered[1:]:
                if second - last > STRETCH_GAP_S:
                    stretches.setdefault(subject, []).append((pod, start, last + 1))
                    start = second
                last = second
            stretches.setdefault(subject, []).append((pod, start, last + 1))
    overlaps = []
    for subject, held in stretches.items():
        for i, (pod_a, from_a, to_a) in enumerate(held):
            for pod_b, from_b, to_b in held[i + 1:]:
                lo, hi = max(from_a, from_b), min(to_a, to_b)
                if pod_a != pod_b and hi - lo >= 1:
                    overlaps.append((subject, lo, hi))
    outside = [(subject, lo, hi) for subject, lo, hi in overlaps
               if not any(began - SKEW_S <= lo and hi <= until + SKEW_S + 1 for _, began, until in LAPSES)]
    within = sorted({what for _, lo, hi in overlaps for what, began, until in LAPSES
                     if began - SKEW_S <= lo and hi <= until + SKEW_S + 1})
    said = ", ".join(f"{subject[-4:]} {time.strftime('%H:%M:%S', time.gmtime(lo))}+{hi - lo}s"
                     for subject, lo, hi in outside[:4])
    lines = sum(len(each) for each in SERVED.values())
    s.check(lines > 0 and not outside,
            f"served once: two pods served one subject only inside a lapse's window ({len(overlaps)} overlap(s) "
            f"over {lines} served lines, in {within or 'none'}; {len(outside)} outside{': ' + said if said else ''})")


def wire_fields(body):
    """A protobuf message's fields in order: (number, wire type, value), a
    length-delimited value as its bytes."""
    fields, at = [], 0

    def varint():
        nonlocal at
        value = shift = 0
        while True:
            byte = body[at]
            at += 1
            value |= (byte & 0x7F) << shift
            shift += 7
            if byte < 0x80:
                return value
    while at < len(body):
        key = varint()
        number, kind = key >> 3, key & 7
        if kind == 0:
            value = varint()
        elif kind == 1:
            value, at = body[at:at + 8], at + 8
        elif kind == 2:
            size = varint()
            value, at = body[at:at + size], at + size
        elif kind == 5:
            value, at = body[at:at + 4], at + 4
        else:
            raise ValueError(f"protobuf wire type {kind} at {at}")
        fields.append((number, kind, value))
    return fields


# What the lake or the sidecar stamps on a row, not what its source said:
# ObservationMeta's sent and recorded times, version, sequence and previous
# sequence (8 to 12); and Source's instance and plugin version (1, 2).
STAMPED_META = {8, 9, 10, 11, 12}
STAMPED_SOURCE = {1, 2}
# The prefix the SDK gives a raw reference on a shard (item 30). The lake
# counts a raw reference in its sameness rule, so one value recorded by two
# shards, their references differing, is kept as a second version (item
# 180, 2026-11-19: so ruled for v30; item 172's prefix rule is withdrawn).
SHARD_RAW = re.compile(rb"shard-\d+/")
# Each row key with more than one version, judged by its bodies: at most
# this many rows read. More is said, and fails the strict check, rather than
# passing it unread.
VERSIONS_READ = 20000


def row_said(body):
    """A recorded price (meridian.v1.Price) as the lake's sameness rule reads
    it: (its values, its raw reference, the raw reference without the shard
    prefix)."""
    values, raw = [], b""
    for number, kind, value in wire_fields(body):
        if number != 1:
            values.append((number, kind, value))
            continue
        for n, k, v in wire_fields(value):
            if n == 13:
                raw = v
            elif n == 3:
                values.append(("source", tuple(f for f in wire_fields(v) if f[0] not in STAMPED_SOURCE)))
            elif n not in STAMPED_META:
                values.append(("meta", n, k, v))
    return tuple(values), raw, SHARD_RAW.sub(b"", raw)


def row_keys_once(s):
    """Each row key of the shards' dataset recorded once, as ruled (item
    180, 2026-11-19): no row key has a second version with other values;
    a second version of the same values is listed, not failed. The shard
    driver's price for a subject and a second is the same in every pod, so a
    second version with other values is two writes of one key disagreeing
    (the driver's fault, or a restatement). Each second version is said as
    one of three kinds, each key's kept in .e2e-shards-versions.log --
      other values: red;
      shard prefix only: the same values, the raw references differing in
        nothing but the shard prefix;
      same values otherwise: the same values, the raw references differing
        otherwise or not at all."""
    rows = psql(f"select count(*) from lake_row where dataset = '{SHARD_DATASET}'", quiet=True)
    # One statement, so the rows read and their count are one snapshot: the
    # count is taken over every versioned row before the limit cuts.
    listed = psql(f"select row_key, version, encode(body, 'hex'), count(*) over () from lake_row "
                  f"where dataset = '{SHARD_DATASET}' and row_key in (select row_key from lake_row "
                  f"where dataset = '{SHARD_DATASET}' group by row_key having count(*) > 1) "
                  f"order by row_key, version limit {VERSIONS_READ}", quiet=True)
    versions, many = {}, 0
    for line in listed.splitlines():
        key, version, body, many = line.split("|")
        versions.setdefault(key, []).append((int(version), row_said(bytes.fromhex(body))))
    kinds = {"other values": [], "shard prefix only": [], "same values otherwise": []}
    lines = []
    for key, held in sorted(versions.items()):
        values, raw, bare = held[0][1]
        for version, (later, later_raw, later_bare) in held[1:]:
            kind = ("other values" if later != values else
                    "shard prefix only" if later_raw != raw and later_bare == bare else
                    "same values otherwise")
            if key not in kinds[kind]:
                kinds[kind].append(key)
            lines.append(f"VERSION {key} {version} {kind}\n")
    with open(".e2e-shards-versions.log", "w", encoding="utf-8") as kept:
        kept.writelines(lines)

    def said(kind):
        keys = kinds[kind]
        return f"{len(keys)} {kind}{': ' + ', '.join(keys[:4]) if keys else ''}"
    unread = int(many) - sum(len(held) for held in versions.values())
    s.check(int(rows or 0) > 0 and not kinds["other values"] and unread == 0,
            f"served once: no row key of {SHARD_DATASET} recorded twice with other values "
            f"({rows} rows; {said('other values')}"
            f"{f'; {unread} versioned row(s) past the {VERSIONS_READ} read, not judged' if unread else ''})")
    s.note(f"served once: second versions of the same values, listed, not failed (item 180): "
           f"{said('shard prefix only')}; {said('same values otherwise')}; kept in .e2e-shards-versions.log")


def shards_fenced(s, settled):
    """Step 3d, checkpoint 5, item 32: the epoch fence for plugin writes, as
    the shards' sidecars meet it with the shard driver writing (item 33).

    (a) Every pod of the instance cut from the API server, both ways, past
    a Lease's lapse: a data shard writes on through it, fenced by its epoch,
    and is never stopped by the clock -- no sidecar restarts, each Lease's
    holder and each shard's epoch as they were, no take recorded, and every
    subject's rows landing through it.

    (b) The holder of shard 1 alone cut from it: the standby takes the
    Lease once it lapses, within the takeover's bound, and serves the lost
    shard's subjects within it; the lake records one take and raises the
    shard's epoch by one; the cut holder writes on until a write of its is
    refused as fenced, or it hears the API server again and finds another
    holder, then stands by again in its process, its container never
    restarted (item 49, ruled 2026-11-16), within a lapse and the takeover's
    bound of hearing the API server again.

    Answers the cut holder, its sidecar container's status before the cut,
    and when its Lease was taken; none where the shards did not settle
    again."""
    names = {p["metadata"]["name"] for p in shard_pods()}
    held = [shard_holder(n)[1] for n in range(SHARDS)]
    epochs = [shard_epochs(n) for n in range(SHARDS)]
    s.check(all(e >= 1 for each in epochs for e in each.values()),
            f"shards: each shard's epoch raised in every lake and book database as its pod took it ({epochs})")
    takes = shard_takes()
    restarted = {n: restarts(n) for n in names}
    began = time.time()
    cut_api("-I", only=names)
    try:
        time.sleep(SHARD_CUT_S)
    finally:
        cut_api("-D", only=names)
    time.sleep(5)
    holder_pods = [h.split("_", 1)[0] for h in held]
    bit = all(max(said_at(h, "the shard's lease was not renewed") or 0,
                  said_at(h, "the shard's lease renewal did not answer") or 0) >= began - 1
              for h in holder_pods)
    s.check(bit, f"shards: the cut reached each shard: its renewals failed for {SHARD_CUT_S} s")
    # Every pod still listed, none replaced: a pod deleted and made again
    # would pass a check of the pods listed alone (review c5a M5).
    # A serving lost stands by again in its process, so no restart says
    # it (item 49): judged by every end's words since the cut began --
    # lost, fenced, or a take not completed -- never by restarts alone
    # (review c5a6 L1).
    listed = {p["metadata"]["name"] for p in shard_pods()}
    ended = [h for h in holder_pods
             if max(said_at(h, LOST) or 0, said_at(h, STANDING_AGAIN) or 0) >= began - 1]
    s.check(listed == names and all(restarts(n) == restarted[n] for n in names) and not ended,
            f"shards: through the API server's outage, no shard stopped: written on, fenced by its epoch "
            f"(pods {sorted(names)} -> {sorted(listed)}; serving ended in {ended})")
    s.check([shard_holder(n)[1] for n in range(SHARDS)] == held,
            "shards: each Lease's holder as it was after the outage")
    after = [shard_epochs(n) for n in range(SHARDS)]
    s.check(after == epochs and shard_takes() == takes,
            f"shards: no take, each shard's epoch as it was ({epochs} -> {after})")
    # Its rows landing through the outage (item 32's e2e; item 33): each
    # holder recorded its subjects, all of them between the two, in the
    # outage's last seconds -- past a Lease's lapse.
    served_read()
    late = served_between(began + SHARD_LAPSE_S + SKEW_S, began + SHARD_CUT_S - SKEW_S)
    landed = set().union(*late.values()) if late else set()
    s.check(set(late) == set(holder_pods) and landed >= set(SHARD_SUBJECTS),
            f"shards: through the API server's outage, past a lapse, each shard's rows kept landing "
            f"({len(landed & set(SHARD_SUBJECTS))} of {len(SHARD_SUBJECTS)} subjects, by {len(late)} pod(s))")

    # (b) The holder of shard 1 alone cut.
    if not a_standby_stands_by(s, "the holder alone is cut"):
        return False
    lost, was = shard_holder(1)
    standby = the_standby()
    epoch, takes = shard_epoch(1), shard_takes()
    before = restarts(lost)
    sidecar_before = sidecar_status(json.loads(out("get", "pod", lost, "-o", "json")))
    refused_before = shard_lines(lost, "/tmp/shard-refused.log")
    began = time.time()
    cut_api("-I", only={lost})
    try:
        taken = happens(lambda: shard_holder(1)[1] not in ("", was), SHARD_TAKEOVER + 10, every=0.25)
        took = time.time() - began
        now_pod, now = shard_holder(1)
        s.check(taken and took <= SHARD_TAKEOVER,
                f"shards: the holder alone cut from the API server, another took its lease within "
                f"{SHARD_TAKEOVER} s ({round(took, 2)} s; {now}, was {was})")
        raised = happens(lambda: set(shard_epochs(1).values()) == {epoch + 1}
                         and shard_takes() == takes + 1, 15, every=0.5)
        s.check(raised, f"shards: one take recorded, the shard's epoch raised by one in every lake and "
                        f"book database ({epoch} -> {shard_epochs(1)}; takes {takes} -> {shard_takes()})")
        # Take to serving: the new holder's sidecar serving within the
        # takeover's bound of the cut, not only its Lease changed (M5).
        serving = happens(lambda: (said_at(now_pod, "the sidecar is serving") or 0) >= began,
                          SHARD_TAKEOVER + 10, every=0.25)
        served = (said_at(now_pod, "the sidecar is serving") or 0) - began
        s.check(serving and served <= SHARD_TAKEOVER,
                f"shards: the standby serves the shard within {SHARD_TAKEOVER} s of the cut "
                f"({round(served, 2)} s)")
        on_time = taken and took <= SHARD_TAKEOVER and serving and served <= SHARD_TAKEOVER
        s.check(restarts(lost) == before and (said_at(lost, LOST) or 0) < began,
                "shards: the cut holder serves on while it cannot hear the API server")
        TAKES.append(("the holder alone cut", lost, began, now_pod))
        figure("shards: the holder alone cut, its lease taken", lease_s=round(took, 2),
               serving_s=round(served, 2))
    finally:
        cut_api("-D", only={lost})
    lifted, lost_at = time.time(), began + took
    # Its writes after the take refused (item 32's e2e): fenced, a write of
    # its own the first after the take, while it could not hear the API
    # server.
    fenced = happens(lambda: any("fenced" in line for line in
                                 shard_lines(lost, "/tmp/shard-refused.log")[len(refused_before):]),
                     5, every=0.5)
    s.check(fenced, "shards: the cut holder's write after the take refused as fenced, its plugin told "
                    f"({(shard_lines(lost, '/tmp/shard-refused.log')[len(refused_before):] or ['none'])[0][:200]})")
    # The standby's own words through the cut, kept whatever the result,
    # once the cut is lifted: its pod is killed next, and its log went with
    # it once (CI 43973174092, PO item 54). The standby last, so the tail CI
    # prints is its log.
    shard_said_kept("the holder alone cut", [lost, standby], began, say=not on_time)
    # By the sidecar's own words, in the run it was serving in: the lease
    # lost, held by the pod that took it, standing by again -- and then
    # standing by -- and its plugin no longer answered. Judged by its log
    # and its container's last end, never by restartCount, which the
    # kubelet raises only once a back-off has passed (review c5a5 F1).
    # Its serving ended by a write refused as fenced (item 32), or by the
    # Lease read again, whichever came first.
    def stood_by_again():
        said = shard_log(lost, stamped=True).splitlines()
        at = next((n for n, line in enumerate(said)
                   if STANDING_AGAIN in line and (FENCED in line or (LOST in line and f"held by {now_pod}" in line))
                   and line_at(line) >= began - SKEW_S), None)
        return (at is not None and any("standing by" in line and STANDING_AGAIN not in line for line in said[at + 1:])
                and not plugin_answered(shard_log(lost, container="plugin")))
    again = happens(stood_by_again, 30, every=0.5)
    sidecar_after = sidecar_status(json.loads(out("get", "pod", lost, "-o", "json")))
    kept_on = (sidecar_after.get("restartCount") == sidecar_before.get("restartCount")
               and sidecar_after.get("lastState") == sidecar_before.get("lastState")
               and "running" in sidecar_after.get("state", {}))
    words = [line for line in shard_log(lost).splitlines() if LOST in line or FENCED in line]
    s.check(again and kept_on,
            f"shards: the cut holder, fenced or hearing the API server again, stood by again in its "
            f"process, its plugin no longer answered ({'stood by again' if again else 'did not stand by again'}; "
            f"{'its container ran on' if kept_on else 'its container ended: ' + json.dumps(sidecar_after.get('lastState'))}"
            f"; {words[-1:] or 'it said nothing of it'})")
    # A pod that lost its shard is a standby within the bound, with no
    # back-off between (item 49; review c5a6 M4): timed from the API
    # server heard again.
    stood_at = said_at(lost, STOOD_BY)
    stood_in = None if stood_at is None else round(stood_at - lifted, 2)
    s.check(again and stood_in is not None and stood_in <= SHARD_LAPSE_S + SHARD_TAKEOVER,
            f"shards: the cut holder stood by again within {SHARD_LAPSE_S + SHARD_TAKEOVER} s of hearing the API "
            f"server again ({stood_in} s)")
    back = happens(lambda: settled() and len([p for p in shard_pods() if ready(p)]) == SHARDS, 180)
    s.check(back, f"shards: two shards and a standby again ({len(shard_pods())} pods)")
    lapse("the holder alone cut", began)
    return (lost, sidecar_before, lost_at) if back else None


def shards(s):
    """Two shards and a standby pod of one instance: three pods of its one
    Deployment, each shard's Lease held by a pod of its own, the pod holding
    none standing by and not ready -- nothing listens in it, so its plugin
    is not answered -- and the members' token in the sidecar's container
    alone. A shard's sidecar killed outright (SIGKILL, so nothing is
    released) is another process's once its Lease has lapsed, never before,
    and that one serves within the bound, the other shard never taken: the
    pod that lost that shard in the fence's step (b), standing by in its
    process since (item 49). Its taker killed in turn within 60 s, the
    second taker serves within the bound too. Its pods then roll a shard at a time, and a shard
    gone and back is read by the one that stayed.

    Each subject served once (step 3d, checkpoint 5, item 33): the shard
    driver in each pod serves its share of the reader's standing want of
    the 32 subjects. Settled, the shards' served sets are disjoint and
    cover all 32, the standby's empty; after each lapse the taker serves
    the lost shard's subjects within the takeover's bound; two pods serve
    one subject only inside a lapse's window; and no row key is recorded
    twice."""
    print("S: a plugin instance at two shards with a standby pod", flush=True)

    def settled():
        names = {p["metadata"]["name"] for p in shard_pods()}
        holders = [shard_holder(n)[0] for n in range(SHARDS)]
        return len(names) == SHARDS + 1 and all(h in names for h in holders) and len(set(holders)) == SHARDS
    there = happens(settled, 240)
    live = shard_pods()
    names = sorted(p["metadata"]["name"] for p in live)
    holders = [shard_holder(n)[0] for n in range(SHARDS)]
    s.check(there, f"shards: {SHARDS + 1} pods of the one instance, each shard's lease held by a pod of its own "
                   f"({len(names)} pods; holders {holders})")
    shard_restarts_said(s, "as they first settled")
    if not there:
        return
    standby = next(n for n in names if n not in holders)
    waited = happens(lambda: sorted(p["metadata"]["name"] for p in shard_pods() if ready(p)) == sorted(holders), 60)
    ready_now = sorted(p["metadata"]["name"] for p in shard_pods() if ready(p))
    s.check(waited, f"shards: the two shards ready, the standby not: it serves nothing ({ready_now}; "
                    f"standby {standby})")
    s.check("standing by" in shard_log(standby) and "the sidecar is serving" not in shard_log(standby),
            "shards: the standby says it stands by, and serves nothing")
    s.check(all("this pod holds a shard's lease" in shard_log(h) for h in holders),
            "shards: each shard's sidecar says it holds its lease")
    # What a plugin's container is given, and reaches: no token in any pod's;
    # its sidecar answering it in each shard's, and not in the standby's.
    spec = live[0]["spec"]
    containers = sorted(c["name"] for c in spec["containers"])
    mounted = [c["name"] for c in spec["containers"]
               if any(m["mountPath"].startswith("/var/run/secrets/kubernetes.io") or m["name"] == "member-token"
                      for m in c.get("volumeMounts", []))]
    s.check(containers == ["plugin", "sidecar"] and spec.get("automountServiceAccountToken") is False
            and mounted == ["sidecar"],
            f"shards: the lease right's token is mounted in the sidecar's container alone, a plugin's beside it "
            f"(containers {containers}; mounted in {mounted})")
    # Said by the shard driver itself, a real plugin's container (item 31).
    plugins = {n: shard_log(n, container="plugin") for n in names}
    s.check(all(SHARD_NO_TOKEN in said for said in plugins.values()),
            "shards: no pod's plugin container holds the token, as the shard driver finds it")
    answered = happens(lambda: all(plugin_answered(shard_log(h, container="plugin")) for h in holders), 30)
    # The standby's as it stands now: a pod whose shard was lost before
    # (its sidecar ended and came back standing by) said both, in turn.
    waits = happens(lambda: not plugin_answered(shard_log(standby, container="plugin")), 10)
    if SHARD_ANSWERED in shard_log(standby, container="plugin"):
        restarts = out("get", "pod", standby, "-o", "jsonpath={.status.containerStatuses[?(@.name=='sidecar')]"
                       ".restartCount}")
        s.note(f"shards: the standby {standby} held a shard before (its sidecar restarted {restarts} time(s)); "
               f"its plugin: {shard_log(standby, container='plugin').strip()[-400:]}")
    s.check(answered and waits,
            "shards: each shard's plugin is answered by its sidecar; the standby's is not, so its run() waits")
    # The reader's want shared (item 33): until the shards first serve it
    # whole, they are settling.
    served_once(s, "as the shards settled", seconds=240)
    lapse("the shards settling", 0)
    deployment = json.loads(out("get", "deployment", "-l", f"meridian.dev/instance={SHARDED}", "-o", "json"))["items"][0]
    shard_admission(s, deployment)
    # Its own account (item 31): its pods run under it, its Role names its
    # shards' Leases alone, and it may do nothing else; the launcher's
    # admission policy holds what the launcher makes to that shape.
    accounts = {p["spec"].get("serviceAccountName") for p in shard_pods()}
    s.check(accounts == {shard_accounts.account_of(FULL, SHARDED)},
            f"accounts: the instance's pods run under its own account, never the members' ({sorted(accounts)})")
    names = shard_accounts.role_names(kubectl, FULL, SHARDED)
    s.check(names == [f"{SHARDED}-shard-{n}" for n in range(SHARDS)],
            f"accounts: its Role names exactly its {SHARDS} shards' Leases ({names})")
    for held, what in (shard_accounts.rights(kubectl, NAMESPACE, FULL, SHARDED, SHARDS, "another-1", f"{FULL}-lake")
                       + shard_accounts.admission(kubectl, NAMESPACE, FULL, SHARDED)
                       + shard_accounts.deployments(kubectl, NAMESPACE, FULL, SHARDED)
                       + shard_accounts.kept(kubectl, SHARDED, SHARDS)
                       + shard_accounts.lease_deletes(kubectl, SHARDED, f"{FULL}-lake")):
        s.check(held, what)
    # The epoch fence (item 32): through an outage of the API server, and
    # with the holder alone cut from it. Its holders as they stand after.
    fenced = shards_fenced(s, settled)
    if not fenced:
        return
    cut_holder, cut_sidecar, cut_lost_at = fenced
    if not a_standby_stands_by(s, "the kill"):
        return
    holders = [shard_holder(n)[0] for n in range(SHARDS)]

    # Shard 1's sidecar killed outright: no signal it can answer, so nothing
    # is released and its Lease lapses. Its taker does so no sooner than the
    # lapse, and serves within the bound.
    #
    # And shard 1 lost twice within 60 s (item 49; review c5a6 M4): its
    # holder cut in (b), and now the pod that took it killed. The killed pod
    # is cut from the API server until its shard is taken, so its sidecar,
    # started again, cannot take it back: the taker is the pod that lost it
    # the first time, standing by in its process -- its sidecar never
    # restarted -- and it serves within the bound.
    lost, other = holders[1], holders[0]
    was, other_held = shard_holder(1)[1], shard_holder(0)[1]
    began = time.time()
    cut_api("-I", only={lost})
    try:
        crash(lost, "sidecar")
        taken = happens(lambda: shard_holder(1)[1] not in ("", was), SHARD_TAKEOVER + 20, every=0.25)
        took = time.time() - began
        now_pod, now = shard_holder(1)
        s.check(taken, f"shards: another process holds the killed shard's lease ({now}, was {was})")
        s.check(taken and took >= 3.0,
                "shards: no sooner than its lapse: its holder stops unrenewed first")
        served = happens(lambda: (said_at(now_pod, "the sidecar is serving") or 0) >= began - 1, 30, every=0.5)
        serving = (said_at(now_pod, "the sidecar is serving") or 0) - began if served else None
        s.check(served and serving <= SHARD_TAKEOVER,
                f"shards: its taker serves within {SHARD_TAKEOVER}s of the kill"
                + ("" if served else " (never)"))
        TAKES.append(("the first kill", lost, began, now_pod))
    finally:
        cut_api("-D", only={lost})
    after = sidecar_status(json.loads(out("get", "pod", cut_holder, "-o", "json")))
    ran_on = (after.get("restartCount") == cut_sidecar.get("restartCount")
              and after.get("lastState") == cut_sidecar.get("lastState") and "running" in after.get("state", {}))
    apart = round(began - cut_lost_at, 1)
    s.check(taken and now_pod == cut_holder and ran_on and apart <= 60,
            f"shards: shard 1 lost twice within 60 s ({apart} s apart), its holder cut and then its taker killed: "
            f"the pod that lost it the first time took it back, its sidecar never restarted ({now_pod}, lost by "
            f"{cut_holder}; {'its container ran on' if ran_on else 'its container ended: ' + json.dumps(after.get('lastState'))})")
    figure("shards: the lease changed hands, and its taker served, after the kill",
           lease_s=round(took, 2), serving_s=None if serving is None else round(serving, 2))
    s.check(shard_holder(0)[1] == other_held, "shards: the live shard's lease never changed hands")
    plugged = happens(lambda: plugin_answered(shard_log(now_pod, container="plugin")), 30)
    s.check(plugged, "shards: the taker's plugin is answered by its sidecar")
    back = happens(lambda: settled() and len([p for p in shard_pods() if ready(p)]) == SHARDS, 180)
    s.check(back, f"shards: two shards and a standby again ({len(shard_pods())} pods)")

    # Twice within 60 s (item 49; review c5a5 F3): the taker killed in its
    # turn once a standby stands by again, and the second taker serves
    # within the bound. A pod that loses its shard stands by in its process,
    # so no back-off but the first kill's own stands between.
    first_kill, lost, was = began, now_pod, now
    # Asked once at least, however long the first kill's checks took
    # (review c5a6 L6).
    left = 60 - (time.time() - first_kill)
    stood = ((bool(the_standby()) and standing_by(the_standby()))
             or happens(lambda: bool(the_standby()) and standing_by(the_standby()), left, every=1.0))
    stood = stood and time.time() - first_kill <= 60
    s.check(stood, f"shards: within 60 s of the kill, a standby stood by for a lapse again ({the_standby() or 'none'})")
    if not stood:
        return
    began = time.time()
    # The first kill's window ends as the second's begins.
    lapse("the first kill", first_kill)
    crash(lost, "sidecar")
    taken = happens(lambda: shard_holder(1)[1] not in ("", was), SHARD_TAKEOVER + 20, every=0.25)
    now_pod, now = shard_holder(1)
    served = taken and happens(lambda: (said_at(now_pod, "the sidecar is serving") or 0) >= began - 1, 30, every=0.5)
    serving = (said_at(now_pod, "the sidecar is serving") or 0) - began if served else None
    s.check(served and serving <= SHARD_TAKEOVER and began - first_kill <= 60,
            f"shards: shard 1's holder killed twice, {round(began - first_kill, 1)} s apart, its second taker serves "
            f"within {SHARD_TAKEOVER} s ({'never' if serving is None else round(serving, 2)}; {now}, was {was})")
    figure("shards: the second kill within 60 s, its taker served",
           apart_s=round(began - first_kill, 1), serving_s=None if serving is None else round(serving, 2))
    TAKES.append(("the second kill", lost, began, now_pod))
    back = happens(lambda: settled() and len([p for p in shard_pods() if ready(p)]) == SHARDS, 180)
    s.check(back, f"shards: two shards and a standby again ({len(shard_pods())} pods)")
    # Each take's subjects, judged once the second kill is over: the 60 s
    # between the two kills (item 49) waits for nothing of them.
    while TAKES:
        served_taken(s, *TAKES.pop(0))
    served_once(s, "after the second kill")
    lapse("the second kill", began)

    # The instance's pods rolled: a shard at a time, a new pod standing by
    # and taking the Lease an old one gives up as it stops. Never fewer
    # than one shard ready, and it ends.
    shard_restarts_said(s, "before its pods roll")
    name = deployment["metadata"]["name"]
    before = {p["metadata"]["name"] for p in shard_pods()}
    served_read()
    rolling = time.time()
    kubectl("rollout", "restart", f"deployment/{name}")
    # Judged by its pods, not by `kubectl rollout status`: that waits for
    # every replica available, and the standby pod is never ready, by
    # design, so it never ends for this Deployment (PR CI 43783060111: it
    # ran to its 300 s with the three new pods there, two ready and the
    # third standing by, which is the rollout done).
    least, rolled, until, read_at = SHARDS, False, time.time() + 300, 0
    while time.time() < until and not rolled:
        try:
            # What the pods going served, read while they are there.
            if time.time() - read_at >= 3:
                served_read()
                read_at = time.time()
            live = shard_pods()
            least = min(least, len([p for p in live if ready(p)]))
            rolled = (not ({p["metadata"]["name"] for p in live} & before) and settled()
                      and len([p for p in shard_pods() if ready(p)]) == SHARDS)
        except (Exception, SystemExit):
            pass
        time.sleep(0.5)
    s.check(rolled, f"shards: its pods rolled to new ones, two shards and a standby "
                    f"({len(shard_pods())} pods, {len([p for p in shard_pods() if ready(p)])} ready)")
    s.check(rolled and least >= 1, f"shards: a shard at a time: never fewer than one ready (least {least})")
    if not rolled:
        return
    served_once(s, "after the roll")
    lapse("the roll", rolling)

    # A shard gone and back, read by the one that stayed: down to one pod
    # (the standby goes first, being not ready, then a shard, which gives
    # its Lease up), then back to three.
    changed = "the instance's live shards changed"
    shard_restarts_said(s, "before they are scaled down")
    served_read()
    scaling = time.time()
    kubectl("scale", f"deployment/{name}", "--replicas=1")
    alone = happens(lambda: len(shard_pods()) == 1 and ready(shard_pods()[0]), 120)
    stayed = shard_pods()[0]["metadata"]["name"] if alone else ""
    # Its neighbour gone: a change said after the scale was asked, by the
    # cluster's stamp (k3d's pods share the run's clock), not one said
    # before it, which a count could take for it.
    gone = alone and happens(lambda: (said_at(stayed, changed) or 0) > scaling, 30)
    if alone:
        served_alone(s, stayed)
    kubectl("scale", f"deployment/{name}", f"--replicas={SHARDS + 1}")
    again = happens(lambda: settled() and len([p for p in shard_pods() if ready(p)]) == SHARDS, 180)
    lines = shard_log(stayed).count(changed) if alone else 0
    told = alone and happens(lambda: shard_log(stayed).count(changed) >= 3, 60)
    lines = shard_log(stayed).count(changed) if alone else 0
    s.check(bool(gone) and again and bool(told),
            f"shards: the shard that stayed read its neighbour live, gone and back, and shared its subjects again "
            f"each time ({lines} changes said, at least 3)")
    served_once(s, "after the scale back")
    lapse("scaled to one and back", scaling)


def served_whole(s):
    """The shards step's whole record (item 33), however far it got: two
    pods served one subject only inside a lapse's window, and no row key
    was recorded twice. The shard drivers and the reader stopped after, so
    nothing they write reaches the steps after."""
    # A take the step did not get to judge (it ended early).
    while TAKES:
        served_taken(s, *TAKES.pop(0))
    overlap_in_lapses(s)
    row_keys_once(s)
    # What every pod served, and the windows, kept beside the run (CI keeps
    # .e2e-*.log when a run fails, and prints each one's tail: the windows).
    with open(".e2e-shards-served.log", "w", encoding="utf-8") as kept:
        for pod, lines in sorted(SERVED.items()):
            kept.writelines(f"SERVED {pod} {shard} {subject} {second} {int(at * 1000)}\n"
                            for shard, subject, second, at in lines)
        kept.writelines(f"LAPSE {what} {began:.1f} {until:.1f}\n" for what, began, until in LAPSES)
    print("    - served once: what each shard pod served, and each lapse's window, kept in .e2e-shards-served.log",
          flush=True)
    for pod in [p["metadata"]["name"] for p in shard_pods()] + [driver_pod(SHARD_READER)]:
        if pod:
            kubectl("exec", pod, "-c", "plugin", "--", "touch", "/tmp/stop", check=False)


# What analytics says once it has its stream, and where it could not make one.
QUEUE_MADE = "the units go by JetStream's work queue"
QUEUE_NOT_THERE = "the units' work queue is not there"


# The lake's write-ahead stream (meridian_bus::write_ahead::STREAM).
WRITE_AHEAD_STREAM = "LAKE_WRITE_AHEAD"


def write_ahead_made():
    """Each broker server's view of the lake's write-ahead stream: its
    `created`, which the lake takes as the stream's identity (a plugin
    shard's raise keeps its place in it, review c5a2 N4), and its leader,
    by server, from /jsz on every server holding a replica of it."""
    path = "/jsz?streams=true&config=true"
    seen = {}
    for server in broker_servers():
        status, said = monitored(server, (path,))[path]
        if status != 200:
            continue
        try:
            jsz = json.loads(said)
        except ValueError:
            continue
        for account in jsz.get("account_details") or []:
            for stream in account.get("stream_detail") or []:
                if stream.get("name") == WRITE_AHEAD_STREAM:
                    seen[server] = (stream.get("created"), (stream.get("cluster") or {}).get("leader"))
    return seen


def jetstream_rolled(s):
    """The broker's servers restarted one at a time, as an upgrade rolls
    them, and JetStream still there after: each analytics copy restarted
    makes or finds its stream at three, rather than going by the queue
    group. CI 42294258528: with the stream in memory, a roll left the three
    answering "no suitable peers for placement" for twelve minutes.

    In ha, the lake's write-ahead stream's `created` is the same on every
    server, and the same after the roll, which moves its leader on each
    leader's restart (review c5a3 I2): the lake's identity of the stream
    never changes with the server that answers."""
    before = write_ahead_made() if PROFILE == "ha" else {}
    began = time.time()
    kubectl("rollout", "restart", f"statefulset/{broker_set()}")
    rolled = kubectl("rollout", "status", f"statefulset/{broker_set()}", "--timeout=300s",
                     check=False).returncode == 0
    s.check(rolled, "broker: its servers restarted one at a time")
    if PROFILE == "ha":
        servers = len(broker_servers())
        happens(lambda: len(write_ahead_made()) == servers, 120)
        after = write_ahead_made()
        made = {created for created, _ in list(before.values()) + list(after.values())}
        leaders = sorted({leader or "none" for _, leader in list(before.values()) + list(after.values())})
        s.check(len(before) == servers and len(after) == servers and len(made) == 1 and None not in made,
                f"broker: the lake's write-ahead stream's created is one on every server, before and after "
                f"its leader moved ({len(before)} and {len(after)} of {servers} servers; created "
                f"{sorted(str(m) for m in made)}; leaders {', '.join(leaders)})")
        made_again_since(s, began, "broker: its servers rolled, the write-ahead stream's leader moved")
    parts = ("analytics-coordinator", "analytics-worker")
    for part in parts:
        kubectl("rollout", "restart", f"deployment/{FULL}-{part}")
    for part in parts:
        kubectl("rollout", "status", f"deployment/{FULL}-{part}", "--timeout=300s", check=False)

    def said():
        return {p["metadata"]["name"]: kubectl("logs", p["metadata"]["name"], "--all-containers",
                                               check=False).stdout.decode(errors="replace")
                for c in parts for p in pods(c)}
    made = happens(lambda: all(QUEUE_MADE in log or QUEUE_NOT_THERE in log for log in said().values()), 120)
    logs = said()
    fell_back = sorted(name for name, log in logs.items() if QUEUE_MADE not in log)
    s.check(made and not fell_back,
            f"analytics: every copy has JetStream's work queue after the roll ({len(logs) - len(fell_back)} "
            f"of {len(logs)}{'; not: ' + ', '.join(fell_back) if fell_back else ''})")


# ── The write driver (step 3b part 5) ──────────────────────────────────────

def driver_values():
    """The driver's instances as sidecars[] values: the SDK's interop
    image running driver.py, handed over whole as MERIDIAN_DRIVER by
    --set-file, so nothing of it is in a values file or an image."""
    sidecars = []
    for instance, role in DRIVERS.items():
        sidecars.append({"instanceId": instance, "roles": DRIVER_ROLES[role], "plugin": {
            "image": INTEROP_IMAGE, "pullPolicy": "IfNotPresent",
            "args": ["python", "-c",
                     "import os; exec(compile(os.environ['MERIDIAN_DRIVER'], 'driver.py', 'exec'))"],
            "env": {"DRIVER_ROLE": role, "DRIVER_INSTRUMENT": DRIVER_INSTRUMENT, "DRIVER_CASH": DRIVER_CASH,
                    "DRIVER_ACCOUNTS": DESK_ACCOUNT if role == "desk" else ",".join(DRIVER_ACCOUNTS),
                    # Appends to the run's end: reconfigure is longer from
                    # stage T (the group counts, a database added), and at
                    # 400 a key the oms instance was full in 48 minutes.
                    "DRIVER_PER_KEY": "800" if PROFILE == "reconfigure" else "400"},
        }})
        if role in ("oms", "desk"):
            sidecars[-1]["plugin"]["existingSecret"] = DRIVER_SIGNER
        if role == "flood":
            # Its own share at its sidecar, as a launch may set it (contract
            # v30, ruled 2026-11-11): FLOOD_PRESSURE readers at once reach
            # the lake past its budget, as memory-pressure asks, now that
            # the chart's default is 16; noisy-plugin's FLOOD_NOISY are still
            # past it. The chart's bounds and the lake's are unchanged.
            sidecars[-1]["limits"] = {"inFlight": FLOOD_IN_FLIGHT}
    path = os.path.join(tempfile.mkdtemp(prefix="failover-driver-"), "driver.json")
    with open(path, "w", encoding="utf-8") as kept:
        json.dump({"sidecars": sidecars}, kept)
    said = ["-f", path]
    for at in range(len(sidecars)):
        said += ["--set-file", f"sidecars[{at}].plugin.env.MERIDIAN_DRIVER={DRIVER_SCRIPT}"]
    return said


def driver_signer():
    """A key made for the run (Ed25519, as the dashboard signs with): its
    seed and id in a Secret the oms driver's container reads, and its
    public half, returned, for the keys every sidecar trusts. Nothing of it
    is printed."""
    workdir = tempfile.mkdtemp(prefix="failover-signer-")
    key, public = os.path.join(workdir, "key.pem"), os.path.join(workdir, "public.pem")
    run("openssl", "genpkey", "-algorithm", "ed25519", "-out", key)
    run("openssl", "pkey", "-in", key, "-pubout", "-out", public)
    seed = run("openssl", "pkey", "-in", key, "-outform", "DER", quiet=True).stdout[-32:].hex()
    run("kubectl", "create", "namespace", NAMESPACE, check=False)
    kubectl("create", "secret", "generic", DRIVER_SIGNER, f"--from-literal=DRIVER_SIGNER_SEED={seed}",
            f"--from-literal=DRIVER_SIGNER_KEY_ID={DRIVER_SIGNER_KEY_ID}", quiet=True)
    with open(public, encoding="utf-8") as held:
        said = held.read()
    os.remove(key)
    del seed
    return said


def driver_trusted(public):
    """The run's signer's public half beside the dashboard's own, in the
    ConfigMap every sidecar mounts whole (a key arrives without a restart),
    merged as the dashboard's key Job merges its own."""
    keys = f"{FULL}-dashboard-keys"
    wait_for("the dashboard's keys ConfigMap", lambda: kubectl("get", "configmap", keys, check=False).returncode == 0,
             seconds=120)
    kubectl("patch", "configmap", keys, "--type", "merge", "-p",
            json.dumps({"data": {f"{DRIVER_SIGNER_KEY_ID}.pem": public}}))


def driver_fixtures(s):
    """What an admin would have done before the driver can write: its
    accounts and their grant, and the instrument its rows name (as e2e-book's
    are written), then the driver writing both kinds. In the scaled run, the
    subjects the sharded instance shares too (item 33)."""
    for path in DRIVER_FIXTURES + (SHARD_FIXTURES if PROFILE == "scaled" else []):
        with open(path, encoding="utf-8") as held:
            psql(held.read())
    s.check(True, "the driver's accounts, grant and instrument written")
    # Written behind the instrument store, as e2e-book writes them before it
    # starts: its hot layer holds the answer to every ID asked, a record not
    # held among them, and the drivers start asking for the instrument as
    # soon as they are up -- in tolerant before these were written (CI
    # 42755783823: every price refused, "no record the deployment holds").
    # Its members start again, as after an install, and read the record.
    members = pods("instrument")
    for pod in members:
        kubectl("delete", "pod", pod["metadata"]["name"], "--wait=false")
    restarted = happens(lambda: len([p for p in pods("instrument") if ready(p)]) >= len(members)
                        and not ({p["metadata"]["name"] for p in pods("instrument")}
                                 & {p["metadata"]["name"] for p in members})
                        and answering("instrument") is not None
                        # With a lease (ha), held by a member started again:
                        # the gone writer's runs out first (CI 42760537409).
                        and holder("instrument") in ({""} | {p["metadata"]["name"]
                                                            for p in pods("instrument")}),
                        300, every=2)
    s.check(restarted, "the instrument store started again over the records written behind it")
    if not DRIVEN:
        # The scaled run's instances write only when write-scale tells them.
        return
    writing = happens(lambda: all(driver_acked().get(kind) for kind in ("price", "order")), 300, every=3)
    s.check(writing, "the write driver writing through the stores: prices to the lake, orders to the book "
            f"({', '.join(f'{k} {len(v)}' for k, v in sorted(driver_acked().items())) or 'nothing acknowledged'})")
    if not writing:
        for instance in DRIVERS:
            said = kubectl("logs", driver_pod(instance), "-c", "plugin", "--tail=15", check=False)
            s.note(f"{instance} said: {said.stdout.decode(errors='replace')[-1500:]}")
        # The cluster's and the broker's own view kept at once (CI run
        # 43185314225 kept the broker's logs only from its servers' later
        # roll).
        cluster_said("the write driver writing through the stores")


def driver_pod(instance):
    said = json.loads(out("get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE},meridian.dev/instance={instance}",
                          "-o", "json"))["items"]
    live = [p["metadata"]["name"] for p in said if not p["metadata"].get("deletionTimestamp")]
    return live[0] if live else ""


def driver_file(instance, path):
    pod = driver_pod(instance)
    if not pod:
        return ""
    done = kubectl("exec", pod, "-c", "plugin", "--", "cat", path, check=False)
    return done.stdout.decode(errors="replace") if done.returncode == 0 else ""


def driver_acked():
    """What the driver's instances were told was recorded, by kind: each
    id, and when (unix ms)."""
    acked = {}
    for instance in DRIVERS:
        for line in driver_file(instance, "/tmp/acked").splitlines():
            parts = line.split()
            if len(parts) == 4 and parts[0] == "ACKED" and parts[3].isdigit():
                acked.setdefault(parts[1], {})[parts[2]] = int(parts[3])
    return acked


# What the stores hold of the driver, by kind: the store, and its read --
# each key, and whether this database is its partition's or range's home:
# 'left' where its head here is a move's pointer (POINTER_EPOCH), so the
# key is a copy the move left behind, never read (the review's M1: a lake
# move keeps the source's rows).
KEPT_SQL = {
    "order": ("book", "select o.order_id || '|' || case when exists (select 1 from book_partition_head h "
                      "where h.partition = o.partition and h.epoch = -1) then 'left' else 'home' end "
                      "from book_order o where o.account_id like 'ACC-DRIVE-%'"),
    "placement": ("book", "select p.placement_id || '|' || case when exists (select 1 from book_partition_head h "
                          "where h.partition = p.partition and h.epoch = -1) then 'left' else 'home' end "
                          "from book_placement p where p.account_id like 'ACC-DRIVE-%'"),
    # A row's range: its `partition`, or its dataset's whole range where
    # it names none (kept before 3d).
    "price": ("lake", "select r.row_key || '|' || case when exists (select 1 from lake_partition p "
                      "where p.dataset = coalesce(nullif(r.partition, ''), r.dataset) and p.epoch = -1) "
                      "then 'left' else 'home' end from lake_row r where r.dataset = 'drive-data:drive'"),
}
KEPT_KINDS = {"book": ("order", "placement"), "lake": ("price",)}


def driver_kept_each(pod=None):
    """What each database of each store holds of the driver at home, by
    kind: a set for each of the store's databases, the first first (`pod` in place
    of its primary). A partition moved home to database 1 has its
    projections let go in the first (CI 43788516279: the whole run read the
    first alone and counted P0's 1486 orders lost)."""
    databases = {store: [d for d, _ in store_databases(store)] for store in KEPT_KINDS}
    return {kind: [home_keys(read_kept(store, d, sql, pod)) for d in databases[store]]
            for kind, (store, sql) in KEPT_SQL.items()}


def read_kept(store, d, sql, pod=None):
    """`sql` read in `store`'s database `d`, its primary found at each try
    (`pod` in place of the first's): a primary shutting down for a planned
    restart, or not answering yet as its standby takes over, is asked again
    for up to a minute (CI 43799885541, ha: writer groups read database 0's
    primary as an upgrade's raised connections restarted it)."""
    for attempt in range(30):
        target = pod if d == 0 and pod else (primary() if d == 0 else database_pod(store, d))
        try:
            return psql(sql, pod=target)
        except SystemExit:
            if attempt == 29:
                raise
            time.sleep(2)


def home_keys(said):
    """The keys a KEPT_SQL read found at home in its database: none left
    behind by a move."""
    return {line.split("|", 1)[0] for line in said.split() if not line.endswith("|left")}


def driver_kept(pod=None, each=None):
    """What the stores hold of it, in any of their databases: the book's
    orders in the driver's accounts, the lake's rows in its dataset."""
    each = each if each is not None else driver_kept_each(pod)
    return {kind: set().union(*held) for kind, held in each.items()}


def where_kept(ids, each):
    """Where each of `ids` ((kind, id) pairs) is held: the indices of the
    databases of its store that hold it, () for none."""
    return {(kind, i): tuple(d for d, held in enumerate(each[kind]) if i in held) for kind, i in ids}


def kept_by_database(acked, each):
    """Per store, how many of the acknowledged writes each of its databases
    holds, and how many none holds: counts only."""
    said = {}
    for store, kinds in KEPT_KINDS.items():
        where = where_kept({(kind, i) for kind in kinds for i in acked.get(kind, {})}, each)
        databases = len(each[kinds[0]])
        said[store] = {"each": [sum(1 for at in where.values() if d in at) for d in range(databases)],
                       "none": sum(1 for at in where.values() if not at)}
    return said


def where_now():
    """Where each write the driver has had acknowledged is held now, by
    (kind, id): the indices of its store's databases that hold it."""
    acked = driver_acked()
    return where_kept({(kind, i) for kind in KEPT_SQL for i in acked.get(kind, {})}, driver_kept_each())


def held_words(at):
    return "none" if not at else ("database " if len(at) == 1 else "databases ") + ", ".join(map(str, at))


def moved_and_kept(s, what, sampled):
    """The writes acknowledged before a move home (`sampled`, where_now()
    then), where they were and where they are now, by store: counts of each
    shift. Each key held before is held now at its home -- a copy a move
    left behind in a database it left counts for nothing (the review's M2)
    -- and some are in database 1: the run shows writes moving, none
    vanishing (CI 43788516279 read the first database alone)."""
    now = where_kept(set(sampled), driver_kept_each())
    for store, kinds in KEPT_KINDS.items():
        mine = [key for key in sampled if key[0] in kinds]
        shifts = {}
        for key in mine:
            shift = (sampled[key], now[key])
            shifts[shift] = shifts.get(shift, 0) + 1
        gone = sorted(i for kind, i in mine if sampled[(kind, i)] and not now[(kind, i)])
        moved = any(1 in now[key] for key in mine)
        words = "; ".join(f"{held_words(was)} -> {held_words(is_)}: {n}"
                          for (was, is_), n in sorted(shifts.items()))
        s.check(not gone and moved, f"{what}: each of the {store}'s writes acknowledged before the move home is "
                f"held at its home, database 1 or where it was, none only where a move left it ({words or 'none sampled'}"
                f"{'; gone: ' + ', '.join(gone[:5]) if gone else ''})")


def kept_words(store, counts):
    return (f"{store}: " + ", ".join(f"database {d} {n}" for d, n in enumerate(counts["each"]))
            + f", in none {counts['none']}")


# What a class lost, kept as it is found, so a later class is not charged
# with it again.
DRIVER_LOST = {"order": set(), "placement": set(), "price": set()}


def store_rpo(since_ms=0, pod=None, each=None):
    """RPO through the stores: every acknowledged order and price looked
    for in the book's and the lake's own rows, in every database of each
    (read after what was acknowledged, so nothing acknowledged after is
    counted). Says the acknowledgements since `since_ms`, what is newly
    lost (in none of them), and how many each database holds."""
    acked = driver_acked()
    each = each if each is not None else driver_kept_each(pod)
    kept = driver_kept(each=each)
    said = {}
    for name, kinds in (("book", ("order", "placement")), ("lake", ("price",))):
        held = {}
        lost = set()
        for kind in kinds:
            mine = acked.get(kind, {})
            gone = {i for i in mine if i not in kept[kind]} - DRIVER_LOST[kind]
            DRIVER_LOST[kind] |= gone
            held.update(mine)
            lost |= gone
        said[name] = {"acknowledged": sum(1 for at in held.values() if at >= since_ms),
                      "lost": sorted(lost), "lost_s": round(
                          (max(held[i] for i in lost) - min(held[i] for i in lost)) / 1000, 2) if lost else 0}
    for store, counts in kept_by_database(acked, each).items():
        said[store]["by_database"] = counts
    return said


def rpo_extra(rpo):
    """The claims' RPO fields: the book's acknowledged orders and those
    lost (none may be: RPO 0 for the record), and the lake's acknowledged
    rows and those lost (none may be: in ha its writes are held by the
    write-ahead stream before they are acknowledged, contract v30 stage E)."""
    return {"through": "the stores: RecordOrder to the book, RecordPrices to the lake",
            "acknowledged_commits": rpo["book"]["acknowledged"], "rpo_commits_lost": len(rpo["book"]["lost"]),
            "lake_rows_acknowledged": rpo["lake"]["acknowledged"], "lake_rows_lost": len(rpo["lake"]["lost"]),
            "lake_rpo_s": rpo["lake"]["lost_s"]}


def driver_rpo_held(s, after, rpo=None, lake_may_lose=False):
    """Every acknowledged order kept, after a class; and every price, unless
    the caller says the lake may lose some (no class does since the lake's
    write-ahead stream, contract v30 stage E)."""
    rpo = rpo or store_rpo()
    book, lake = rpo["book"], rpo["lake"]
    # Lost is in none of the store's databases; each one's count said.
    s.check(not book["lost"], f"{after}: every order and placement the book acknowledged is in it "
            f"({len(book['lost'])} lost{': ' + ', '.join(book['lost'][:5]) if book['lost'] else ''}; "
            f"{kept_words('held by', book['by_database'])})")
    if lake_may_lose:
        s.note(f"{after}: the lake lost {len(lake['lost'])} acknowledged row(s), within {lake['lost_s']}s "
               f"(its commits are local; {kept_words('held by', lake['by_database'])})")
    else:
        s.check(not lake["lost"], f"{after}: every price the lake acknowledged is in it "
                f"({len(lake['lost'])} lost{': ' + ', '.join(lake['lost'][:5]) if lake['lost'] else ''}; "
                f"{kept_words('held by', lake['by_database'])})")
    return rpo


def acks_again(began_ms, seconds=90):
    """The driver acknowledged by both stores again after `began_ms`."""
    return happens(lambda: all(any(at > began_ms for at in driver_acked().get(kind, {}).values())
                               for kind in ("price", "order")), seconds, every=2)


BOOK_SAID = ".e2e-failover-book.log"


def book_said():
    """The book's members' logs -- its writers, standbys and readers, each
    container's and its previous one's, timestamped -- kept beside the
    driver's history (.e2e-failover-book.log, which CI keeps with
    .e2e-cluster.log when any step fails). Elle checks the history after
    the run has passed and its cluster is gone, and a read it refuses was
    answered by a member whose log went with the cluster (merge-queue run
    45104369464: which member answered was inferred, never shown). Never
    decides the run: a log that cannot be read is said so in its place."""
    parts = [f"== the book's members at the run's end, {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}"]
    listed = json.loads(out("get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json",
                            check=False) or "{}").get("items", [])
    for pod in listed:
        if not pod["metadata"].get("labels", {}).get("meridian.dev/component", "").startswith("bor"):
            continue
        pod_name = pod["metadata"]["name"]
        spec = pod.get("spec", {})
        for container in [c["name"] for c in spec.get("initContainers", []) + spec.get("containers", [])]:
            for previous in ((), ("--previous",)):
                said = kubectl("logs", pod_name, "-c", container, "--timestamps", f"--tail={SAID_LINES}",
                               *previous, check=False, quiet=True)
                parts.append(f"== {pod_name}, {container}{', before, if it restarted' if previous else ''}")
                parts.append((said.stdout or said.stderr).decode(errors="replace"))
    with open(BOOK_SAID, "w", encoding="utf-8") as kept:
        kept.write("\n".join(parts))
    print(f"R: the book's members' logs, kept in {BOOK_SAID}", flush=True)


def driver_history(s):
    """The driver stopped, within its calls' bound, and the oms instance's
    history kept for Elle (the ha failover CI job checks it)."""
    for instance in DRIVERS:
        pod = driver_pod(instance)
        if pod:
            kubectl("exec", pod, "-c", "plugin", "--", "touch", "/tmp/stop", check=False)
    stopped = happens(lambda: all("DONE" in driver_file(i, "/tmp/done") for i in DRIVERS), 60, every=2)
    s.check(stopped, "the write driver stopped when told, within a minute")
    # Section 15 asks every class under the driver's load: an oms instance
    # whose keys all filled wrote no order from then on (driver.py keeps
    # when in /tmp/full), and what ran after it was not under load.
    full = driver_file("drive-oms", "/tmp/full").strip()
    s.check(not full, "the oms driver appended to the run's end: its keys never all filled"
            + (f" ({full.splitlines()[0]})" if full else ""))
    history = driver_file("drive-oms", "/tmp/history.edn")
    with open(HISTORY, "w", encoding="utf-8") as kept:
        kept.write(history)
    book_said()
    counts = {}
    for line in history.splitlines():
        kind = line.split(":type :", 1)[-1].split(",", 1)[0]
        f = "read" if "[[:r " in line else "append"
        counts[f"{f} {kind}"] = counts.get(f"{f} {kind}", 0) + 1
    s.check(counts.get("append ok", 0) > 0 and counts.get("read ok", 0) > 0,
            f"the driver's history kept for Elle in {HISTORY}: "
            + ", ".join(f"{k} {v}" for k, v in sorted(counts.items())))


# ── The order-plugin class (step 3b part 5) ────────────────────────────────

# Proposed against 1048 and 1056: a plugin's pod lost is found by Kubernetes
# within the order path's component tier, 10 s, in either profile; it is
# back, registered again with its sidecar, within the tolerant bound for a
# pod, 2 min (1056), and within the database tier, 30 s, in ha, where a
# plugin has no standby of its own yet (spec section 2, item 7) and comes
# back by restart there too. RPO is the record's: nothing it was told was
# kept is lost, and nothing it sent twice is kept twice.
ORDER_PLUGIN_DETECT = 10
ORDER_PLUGIN_RECOVER = {"tolerant": 120, "ha": 30}


def field_of(record, number):
    """A protobuf message's string field, by number, read from its bytes
    (the book keeps each order whole, as meridian.v1.Order): the first
    one, or ''."""
    at = 0

    def varint():
        nonlocal at
        shift = value = 0
        while True:
            byte = record[at]
            at += 1
            value |= (byte & 0x7F) << shift
            if not byte & 0x80:
                return value
            shift += 7
    while at < len(record):
        key = varint()
        field, wire = key >> 3, key & 7
        if wire == 0:
            varint()
        elif wire == 1:
            at += 8
        elif wire == 5:
            at += 4
        elif wire == 2:
            size = varint()
            if field == number:
                return record[at:at + size].decode(errors="replace")
            at += size
        else:
            return ""
    return ""


def desk_in_book():
    """The ems stand-in's account as the book holds it: each order's number
    (its venue order ID, Order field 32) and the placements of each."""
    orders = {}
    for line in psql(f"select order_id || ' ' || encode(record, 'hex') from book_order "
                     f"where account_id = '{DESK_ACCOUNT}'").splitlines():
        order_id, _, record = line.partition(" ")
        orders[order_id] = field_of(bytes.fromhex(record), 32)
    placements = {}
    for line in psql(f"select placement_id || ' ' || order_id from book_placement "
                     f"where account_id = '{DESK_ACCOUNT}'").splitlines():
        placement_id, _, order_id = line.partition(" ")
        placements.setdefault(order_id, []).append(placement_id)
    return orders, placements


def desk_acked():
    acked = {}
    for line in driver_file("drive-desk", "/tmp/acked").splitlines():
        parts = line.split()
        if len(parts) == 4 and parts[0] == "ACKED" and parts[3].isdigit():
            acked.setdefault(parts[1], {})[parts[2]] = int(parts[3])
    return acked


def order_plugin(s):
    """The ems stand-in's pod lost mid-flow (step 3b part 5): every process
    in it killed outright from its node, its sidecar's and its plugin's, as
    a pod's processes go when it is lost, and the pod kept, so what the
    plugin kept and logged is read after. Detection is the pod not ready;
    recovery, the plugin registered with its sidecar again; then the book's
    changes held against what the stand-in was told: no order or placement
    it was told was kept is missing, and none is in the book twice."""
    instance = "drive-desk"
    print("F: order-plugin: the ems stand-in's pod's processes killed outright, mid-flow", flush=True)
    placed = lambda: len(desk_acked().get("placement", {}))
    wait_for("the ems stand-in placing", lambda: placed() >= 3, seconds=300)
    pod = driver_pod(instance)
    before = restarts(pod)
    began = time.time()
    began_ms = int(began * 1000)
    for container in ("plugin", "sidecar"):
        crash(pod, container)

    def found():
        # Not ready, or already restarted by the kubelet that found it: a
        # restart can be quicker than one look at the pod.
        held = json.loads(out("get", "pod", pod, "-o", "json"))
        return not ready(held) or sum(c.get("restartCount", 0)
                                      for c in held["status"].get("containerStatuses", [])) > before
    detected = happens(found, 60, every=0.2)
    detect_s = time.time() - began
    bound = ORDER_PLUGIN_RECOVER["tolerant" if PROFILE == "tolerant" else "ha"]

    def registered():
        return any(line.startswith("REGISTERED ") and int(line.split()[1]) > began_ms
                   for line in driver_file(instance, "/tmp/registered").splitlines())
    back = happens(registered, bound + 30, every=0.5)
    recover_s = time.time() - began
    s.check(detected, "order-plugin: the pod not ready, found")
    if not back:
        s.check(False, f"order-plugin: the stand-in never registered again within {bound + 30}s")
        return
    placing = happens(lambda: any(at > began_ms for at in desk_acked().get("placement", {}).values()), 120, every=1)
    s.check(placing, "order-plugin: the stand-in placing again after its restart")
    acked = desk_acked()
    orders, placements = desk_in_book()
    lost = sorted([i for i in acked.get("order", {}) if i not in orders]
                  + [i for i in acked.get("placement", {}) if not any(i in held for held in placements.values())])
    numbers = {}
    for order_id, number in orders.items():
        numbers.setdefault(number, []).append(order_id)
    twice = sorted([f"order {n}: {', '.join(ids)}" for n, ids in numbers.items() if len(ids) > 1]
                   + [f"placements of {o}: {', '.join(ids)}" for o, ids in placements.items() if len(ids) > 1])
    s.check(not lost, f"order-plugin: every order and placement the stand-in was told was kept is in the book "
            f"({len(acked.get('order', {}))} orders, {len(acked.get('placement', {}))} placements; "
            f"{len(lost)} lost{': ' + ', '.join(lost[:5]) if lost else ''})")
    s.check(not twice, f"order-plugin: none is in the book twice ({len(twice)}{': ' + '; '.join(twice[:3]) if twice else ''})")
    unplaced = sorted(o for o in orders if o not in placements)
    claim(s, "order-plugin", "ems-plugin", recover_s, bound,
          {"detected_s": round(detect_s, 2), "detect_bound_s": ORDER_PLUGIN_DETECT,
           "orders_acknowledged": len(acked.get("order", {})),
           "placements_acknowledged": len(acked.get("placement", {})),
           "acknowledged_commits": len(acked.get("order", {})) + len(acked.get("placement", {})),
           "rpo_commits_lost": len(lost), "duplicated": len(twice), "unplaced_after": len(unplaced)})
    s.check(detected and detect_s <= ORDER_PLUGIN_DETECT,
            f"order-plugin: detected within {ORDER_PLUGIN_DETECT}s")
    wait_for("the stand-in's pod ready", lambda: ready(json.loads(out("get", "pod", pod, "-o", "json"))), seconds=180)


# ── Failure classes ────────────────────────────────────────────────────────

def writer_pod(component):
    live = pods(component)
    if PROFILE == "tolerant":
        return live[0]["metadata"]["name"] if live else ""
    return holder(component)


def crash(pod_name, component):
    """The member's process killed outright (SIGKILL), from its node: no
    signal it can answer, so its lease is never released and lapses. A pod
    deleted, even with no grace, is told to stop first, and hands over."""
    node = node_of(pod_name)
    script = ("for c in $(crictl ps -q --name '^" + component + "$' --label io.kubernetes.pod.name=" + pod_name + "); do "
              "kill -9 $(crictl inspect --output go-template --template '{{.info.pid}}' $c); done")
    run("docker", "exec", node, "sh", "-c", script)


def kill_writer(s, failure, component):
    print(f"F: {failure}: the {component} writer's process killed outright (SIGKILL)", flush=True)
    wait_for(f"{component} settled", lambda: members_settled(component), seconds=300)
    writer = writer_pod(component)
    epoch = epoch_now(component)
    before = restarts(writer)
    began = time.time()
    crash(writer, component)
    if PROFILE == "tolerant":
        bound = TOLERANT_POD
        # The one member restarts in its pod: answering again once it has.
        back = happens(lambda: restarts(writer) > before and answering(component), bound + 30, every=0.5)
    else:
        bound = HA[component]
        back = happens(lambda: answering(component, avoid=(writer,), above_epoch=epoch), bound + 30, every=0.5)
    took = time.time() - began
    if not back:
        s.check(False, f"{failure}: {component} never answered again within {bound + 30}s")
        explain(s, failure, component)
        return
    extra = {}
    if PROFILE != "tolerant":
        now = epoch_now(component)
        extra = {"epoch_before": epoch, "epoch_after": now}
        if component != "launcher":
            record = took_over_record(component, now)
            s.check("lease lapsed" in record, f"{failure}: the takeover recorded at epoch {now} ({record.split('|')[-1]})")
    claim(s, failure, component, took, bound, extra)
    wait_for(f"{component}'s members back", lambda: members_settled(component), seconds=300)
    if PROFILE != "tolerant":
        modes = modes_of(component)
        s.check(modes == ["standby", "writer"], f"{failure}: the killed writer came back as the standby ({modes})")


def zone_of(node_name):
    return next((n["metadata"]["labels"].get("topology.kubernetes.io/zone", "") for n in nodes()
                 if n["metadata"]["name"] == node_name), "")


def node_of(pod_name):
    return json.loads(out("get", "pod", pod_name, "-o", "json"))["spec"]["nodeName"]


def node_freeze(s):
    component = "bor"
    print(f"F: node-freeze: the {component} writer's node paused past its lease, then resumed", flush=True)
    wait_for(f"{component} settled", lambda: members_settled(component), seconds=300)
    writer = writer_pod(component)
    node = node_of(writer)
    before = restarts(writer)
    epoch = epoch_now(component)
    began = time.time()
    run("docker", "pause", node)
    try:
        back = happens(lambda: answering(component, avoid=(writer,), above_epoch=epoch), HA[component] + 30, every=0.5)
        took = time.time() - began
        # Paused past the lease and the standby's look, then some.
        time.sleep(max(0.0, 14 - (time.time() - began)))
    finally:
        run("docker", "unpause", node)
    if not back:
        s.check(False, "node-freeze: the standby never took over while the writer's node was paused")
        explain(s, "node-freeze", component)
        return
    claim(s, "node-freeze", component, took, HA[component], {"epoch_before": epoch})
    # The old writer wakes, finds the lease another's, and steps down.
    stepped = happens(lambda: restarts(writer) > before, 60)
    s.check(stepped, "node-freeze: the frozen writer stepped down on waking (its container restarted)")
    s.check(holder(component) != writer, "node-freeze: the lease stays with the member that took it over")
    wait_for(f"{component}'s members back", lambda: members_settled(component), seconds=300)
    s.check(epoch_now(component) == epoch + 1, "node-freeze: one takeover, no flapping back")


def pod_ip(pod_name):
    return json.loads(out("get", "pod", pod_name, "-o", "json"))["status"].get("podIP", "")


# Where every cut's rules go: the mangle table, which every packet crosses
# before the filter table and in which nothing of the cluster's reorders a
# rule. The filter table's FORWARD and INPUT are kube-router's: on each of its
# syncs, which pods coming and going start, it puts its own jump back above
# whatever is first, and its pods' chains accept an established connection
# before a rule below them is read. A cut placed first in filter's FORWARD was
# lifted for the connections it was there to cut (diagnostic run 43100786136:
# KUBE-ROUTER-FORWARD back at 1 above the write-ahead-bound cut half way
# through it; 43090858893, ha run 3: the lake's writer, its connection to the
# broker kept, never held from the stream, so no gap and the class red).
CUT = "iptables -t mangle"


def cut_rules(pod_name, targets, verb):
    """Drop every packet between a pod and each target, on the pod's node,
    first in the mangle table's FORWARD (CUT): established connections too,
    which a NetworkPolicy (kube-router's, as most) lets carry on.

    Every rule in one exec on the node, so the cut starts (and ends) at
    once: a docker exec per rule, some thirty for the broker class, spent
    four to six seconds of the measured bound putting the cut in place and
    taking it out (broker 9.46 s of 10 in a local run)."""
    node, ip = node_of(pod_name), pod_ip(pod_name)
    at = "1 " if verb == "-I" else ""
    rules = [f"{CUT} {verb} FORWARD {at}-s {a} -d {b} -j DROP"
             for target in targets for a, b in ((ip, target), (target, ip))]
    # Put in, every one must hold; taken out, each is gone whether or not.
    joined = " && ".join(rules) if verb == "-I" else "; ".join(f"{r} || true" for r in rules)
    run("docker", "exec", node, "sh", "-c", joined or "true")


def split_brain(s):
    component = "lake"
    print(f"F: split-brain: the {component} writer cut from the broker and the API server, not the database", flush=True)
    wait_for(f"{component} settled", lambda: members_settled(component), seconds=300)
    writer = writer_pod(component)
    epoch = epoch_now(component)
    broker = [pod_ip(p["metadata"]["name"]) for p in pods("broker")]
    api = run("kubectl", "get", "endpoints", "kubernetes", "-o",
              "jsonpath={.subsets[*].addresses[*].ip}").stdout.decode().split()
    targets = [t for t in broker + api if t]
    s.note(f"cut from {len(broker)} broker and {len(api)} API server address(es)")
    began = time.time()
    cut_rules(writer, targets, "-I")
    try:
        back = happens(lambda: answering(component, avoid=(writer,), above_epoch=epoch), HA[component] + 30, every=0.5)
        took = time.time() - began
        if back:
            claim(s, "split-brain", component, took, HA[component], {"epoch_before": epoch})
        else:
            s.check(False, "split-brain: the standby never took over from the cut writer")
            explain(s, "split-brain", component)
        # The cut writer stepped down before its lease ended, and cannot
        # come back as anything but an unready standby while cut.
        s.check(happens(lambda: not ready(json.loads(out("get", "pod", writer, "-o", "json"))), 60),
                "split-brain: the cut member is no longer ready")
        s.check(holder(component) != writer, "split-brain: the lease is the standby's: one writer")
    finally:
        cut_rules(writer, targets, "-D")
    healed = happens(lambda: members_settled(component), 180)
    s.check(healed, "split-brain: healed once the cut is lifted, the cut member back as the standby")


def explain(s, failure, component):
    """What each member said, for reading a takeover not seen: its node, its
    mode, what its change log answered, and who holds the lease."""
    for pod in pods(component):
        said = probe(pod["metadata"]["name"], component) or {}
        s.note(f"{failure}: {pod['metadata']['name']} on {pod['spec'].get('nodeName')}: "
               f"member {json.dumps(said.get('member'))}, answers {json.dumps(said.get('answers'))}, "
               f"lease held by {holder(component)}")


def node_lost(s):
    component = "bor"
    print(f"F: node: the {component} writer's node stopped", flush=True)
    wait_for(f"{component} settled", lambda: members_settled(component), seconds=300)
    writer = writer_pod(component)
    node = node_of(writer)
    epoch = epoch_now(component)
    began = time.time()
    run("docker", "stop", node)
    try:
        back = happens(lambda: answering(component, avoid=(writer,), above_epoch=epoch), HA[component] + 30, every=0.5)
        took = time.time() - began
        if back:
            claim(s, "node", component, took, HA[component], {"epoch_before": epoch})
        else:
            s.check(False, "node: the standby never took over from the stopped node")
            # What each member said, for reading: its mode, and what its
            # change log answered.
            for pod in pods(component):
                said = probe(pod["metadata"]["name"], component) or {}
                s.note(f"node: {pod['metadata']['name']} on {pod['spec'].get('nodeName')}: "
                       f"member {json.dumps(said.get('member'))}, answers {json.dumps(said.get('answers'))}, "
                       f"lease held by {holder(component)}")
    finally:
        run("docker", "start", node)
    wait_for("the node back", lambda: all(
        any(c["type"] == "Ready" and c["status"] == "True" for c in n["status"]["conditions"])
        for n in nodes()), seconds=300)
    healed = happens(lambda: members_settled(component), 300)
    s.check(healed, "node: the stopped node's members back, as standbys")


def network_policy(s):
    component = "street"
    print(f"F: network-policy: the {component} cut from its database by a NetworkPolicy, then the policy removed", flush=True)
    wait_for(f"{component} settled", lambda: members_settled(component), seconds=300)
    live = [p["metadata"]["name"] for p in pods(component)]
    before = {name: restarts(name) for name in live}
    subnet = os.environ.get("E2E_K3D_SUBNET", "172.28.0.0/16")
    policy = {
        "apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": {"name": "deny-database", "namespace": NAMESPACE},
        "spec": {
            "podSelector": {"matchLabels": {"meridian.dev/component": component}},
            "policyTypes": ["Egress"],
            "egress": [
                {"to": [{"podSelector": {"matchLabels": {"meridian.dev/component": "broker"}}}]},
                {"to": [{"namespaceSelector": {}, "podSelector": {"matchLabels": {"k8s-app": "kube-dns"}}}]},
                # The nodes (the API server among them), not the database.
                {"to": [{"ipBlock": {"cidr": subnet}}]},
            ],
        },
    }
    kubectl("apply", "-f", "-", input=json.dumps(policy).encode())
    try:
        def cut():
            said = probe(live[0], component) or {}
            database = next((p for p in said.get("paths", []) if p.get("path") == "database"), {})
            return database.get("ok") is False and ":5432" in database.get("address", "")
        named = happens(cut, 90)
        s.check(named, f"network-policy: the probe names the database path it cannot reach, by its address")
    finally:
        kubectl("delete", "networkpolicy", "deny-database", "--ignore-not-found")
    began = time.time()
    healed = happens(lambda: all(
        next((p for p in (probe(name, component) or {}).get("paths", []) if p.get("path") == "database"), {}).get("ok")
        for name in live), 120, every=1)
    took = time.time() - began
    s.check(healed, "network-policy: every path answers again once the policy is removed")
    if PROFILE == "tolerant":
        s.check(all(restarts(name) == before[name] for name in live), "network-policy: healed with no restart")
    else:
        # With a standby, a writer cut from its database may step down and the
        # standby take over: held to one writer, the members back.
        settled = happens(lambda: members_settled(component), 180)
        modes = modes_of(component)
        one = settled and modes == ["standby", "writer"] and holder(component) in {
            p["metadata"]["name"] for p in pods(component)}
        s.check(one, f"network-policy: one writer and its standby once healed ({modes})")
        if not one:
            explain(s, "network-policy", component)
    if healed:
        claim(s, "network-policy", component, took, 60)


# ── The database and the broker (step 3b part 2) ────────────────────────────

WRITER = r"""
export PGPASSWORD="${PATRONI_SUPERUSER_PASSWORD:-$POSTGRES_PASSWORD}"
end=$(( $(date +%s) + SECONDS_ ))
while [ "$(date +%s)" -lt "$end" ]; do
  id=$(psql -X -q -At -h HOST -U postgres -d meridian -v ON_ERROR_STOP=1 \
       -c "insert into failover_e2e.marks (at) values (clock_timestamp()) returning id" 2>/dev/null) \
    && echo "ACKED $id"
  sleep 0.05
done
"""


def marks_table():
    """The marks' table, in a schema of this run's own: in the stores'
    `public` it was the superuser's, and every migration after it failed
    granting the stores' role its tables ("permission denied for table
    failover_marks": migrate-3 Init:Error in failover-repeat 43078770407, ha
    run 1, and PR CI 43078729225, after write-ahead-bound's upgrades)."""
    psql("create schema if not exists failover_e2e; "
         "create table if not exists failover_e2e.marks (id bigserial primary key, at timestamptz not null)")


def write_marks(pod_name, seconds, pause=0.05):
    """Commits, one every 50 ms through the database's Service, from inside
    a member that is not killed, each acknowledged id printed; started in
    the background. Its output names every commit the database acknowledged
    (RPO under load, at the database)."""
    host = f"{FULL}-database.{NAMESPACE}.svc.cluster.local"
    script = (WRITER.replace("HOST", host).replace("SECONDS_", str(int(seconds)))
              .replace("sleep 0.05", f"sleep {pause}" if pause else ":"))
    return subprocess.Popen(
        ["kubectl", "--namespace", NAMESPACE, "exec", "-i", pod_name, "-c", "postgres", "--",
         "sh", "-c", script],
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)


def acknowledged(writer):
    said = writer.communicate(timeout=300)[0].decode(errors="replace")
    return {int(line.split()[1]) for line in said.splitlines()
            if line.startswith("ACKED ") and line.split()[1].isdigit()}


def database_writes(avoid=()):
    """The database's primary takes a commit: it is serving."""
    try:
        psql("insert into failover_e2e.marks (at) values (clock_timestamp())", pod=primary(avoid))
        return True
    except SystemExit:
        return False


def db_primary(s, failure="db-primary"):
    """The database's primary killed outright under the write driver's
    load: RPO read through the stores, every order the book acknowledged
    kept (RPO 0 for the record), and every row the lake acknowledged (its
    commits are local, and in ha its write-ahead stream holds each write
    until the lake has it again). torn-write adds heavy commits straight to the
    database from a member that is not killed, as its load, each
    acknowledged one held too."""
    if failure == "db-primary" and PROFILE != "tolerant" and DRIVEN:
        for component in ("bor", "lake"):
            wait_for(f"{component} settled", lambda c=component: members_settled(c), seconds=300)
        standby_lost(s)
    if failure == "db-primary":
        print("F: db-primary: the database's primary killed outright (SIGKILL), under the driver's writes", flush=True)
    for component in ("bor", "lake"):
        wait_for(f"{component} settled", lambda c=component: members_settled(c), seconds=300)
    marks_table()
    # In ha the driver's prices go by the lake's write-ahead stream (contract
    # v30, stage E): the lake's place in it, read before and after, moves.
    stream_before = write_ahead_position() if PROFILE != "tolerant" and DRIVEN else None
    victim = primary()
    members = [p["metadata"]["name"] for p in pods("database")]
    writer, acked_before = None, None
    if failure == "torn-write":
        # The heavy load, from a member that is not killed. The tolerant
        # profile has one member, killed with its writer: there the commits
        # before the kill are what is held.
        writing_in = next((m for m in members if m != victim), None)
        writer = write_marks(writing_in, 45, pause=0) if writing_in else None
        time.sleep(3)
        psql("insert into failover_e2e.marks (at) values (clock_timestamp())")
        acked_before = int(psql("select max(id) from failover_e2e.marks") or 0)
    began = time.time()
    # With standbys, the primary is kept down: cut off from everything on
    # its node, then killed, so its Patroni cannot renew the leader key as
    # its container comes back and a standby is promoted -- the failover
    # timed, not a restart. Alone, it is killed and restarts on its volume.
    isolated = PROFILE != "tolerant"
    if isolated:
        isolate(victim, "-I")
    crash(victim, "postgres")
    bound = {"bor": TOLERANT_POD, "lake": TOLERANT_POD} if PROFILE == "tolerant" else HA_DATABASE
    took = {}

    def back():
        if not database_writes(avoid=(victim,) if isolated else ()):
            return False
        for component in ("bor", "lake"):
            if component not in took and answering(component):
                took[component] = time.time() - began
        return len(took) == 2

    try:
        happens(back, max(bound.values()) + 30, every=0.5)
    finally:
        if isolated:
            isolate(victim, "-D")
    new_primary = primary(avoid=(victim,) if isolated else ())
    if PROFILE != "tolerant":
        s.check(new_primary != victim, f"{failure}: a standby promoted ({new_primary})")
    extra = {}
    if DRIVEN:
        # The driver acknowledged by both stores again: what it wrote across
        # the failover is all in, and read once it is.
        again = acks_again(int(began * 1000))
        s.check(again, f"{failure}: the write driver acknowledged by the book and the lake again")
        # Every row the lake acknowledged is in it, in either profile: in ha
        # its writes are held by the write-ahead stream first, and its
        # writer goes back to the position the promoted standby holds
        # (contract v30, stage E; the resilience spec's section 4).
        rpo = driver_rpo_held(s, failure, store_rpo(int(began * 1000) - 30_000, pod=new_primary))
        extra = rpo_extra(rpo)
        if failure == "db-primary":
            extra.update(STANDBY_LOST)
        if stream_before is not None:
            moved = happens(lambda: write_ahead_position(new_primary) > stream_before, 60, every=2)
            s.check(moved, f"{failure}: the lake's writes went by its write-ahead stream, its place in it "
                    f"moved past {stream_before} across the failover (now {write_ahead_position(new_primary)})")
    if acked_before is not None:
        acked = acknowledged(writer) if writer else set()
        acked.add(acked_before)
        kept = {int(i) for i in psql("select id from failover_e2e.marks", pod=new_primary).split()}
        lost = sorted(acked - kept)
        s.check(not lost, f"{failure}: every commit the database acknowledged under the heavy load kept "
                f"({len(acked)} acknowledged, {len(lost)} lost{': ' + ', '.join(map(str, lost[:5])) if lost else ''})")
        extra.update({"database_commits_acknowledged": len(acked), "database_commits_lost": len(lost)})
    for component in ("bor", "lake"):
        if component not in took:
            s.check(False, f"{failure}: {component} never answered again within {bound[component] + 30}s")
            continue
        claim(s, failure, component, took[component], bound[component], extra)
    healed = happens(lambda: len(pods("database")) == database_members()
                     and all(ready(p) for p in pods("database")), 300)
    s.check(healed, f"{failure}: the killed member back, as a standby where there are standbys")


def write_ahead_position(pod=None):
    """The lake's place in its write-ahead stream (ha; contract v30, stage
    E), as its store committed it: 0 where it has none."""
    try:
        said = psql("select coalesce(max(sequence), 0) from lake_write_ahead", pod=pod, quiet=True)
    except SystemExit:
        return 0
    return int(said or 0)


def isolate(pod_name, verb):
    """Every packet to and from a pod dropped on its node, whatever its
    peer: routed through the node (FORWARD) or the node itself (INPUT,
    OUTPUT), the API server and the kubelet included; in the mangle table
    (CUT)."""
    node, ip = node_of(pod_name), pod_ip(pod_name)
    for chain, rule in (("FORWARD", ["-s", ip]), ("FORWARD", ["-d", ip]),
                        ("INPUT", ["-s", ip]), ("OUTPUT", ["-d", ip])):
        run("docker", "exec", node, *CUT.split(), verb, chain, *(["1"] if verb == "-I" else []),
            *rule, "-j", "DROP", check=verb == "-I")


def broker_lost(s):
    print("F: broker: a broker server cut off from every member, then killed outright", flush=True)
    wait_for("bor settled", lambda: members_settled("bor"), seconds=300)
    servers = [p["metadata"]["name"] for p in pods("broker")]
    victim = servers[0]
    epoch = epoch_now("bor")
    others = [pod_ip(p["metadata"]["name"])
              for c in ("bor", "lake", "street", "instrument", "conductor", "broker", "dashboard")
              for p in pods(c) if p["metadata"]["name"] != victim]
    others = [ip for ip in others if ip]
    began = time.time()
    if len(servers) > 1:
        cut_rules(victim, others, "-I")
    try:
        time.sleep(2)
        crash(victim, "broker")
    finally:
        if len(servers) > 1:
            cut_rules(victim, others, "-D")
    bound = TOLERANT_BROKER if PROFILE == "tolerant" else HA["bor"]
    back = happens(lambda: answering("bor") is not None, bound + 30, every=0.5)
    took = time.time() - began
    if not back:
        s.check(False, f"broker: the book never answered again within {bound + 30}s")
        return
    took_over = epoch_now("bor") != epoch
    claim(s, "broker", "bor", took, bound, {"servers": len(servers), "takeover": took_over})
    if took_over:
        # A writer whose server was cut off may step down before its client
        # finds another, and its standby, on a live server, take over: the
        # bound above holds either way, and the claim says which it was.
        s.note("broker: the book's writer stepped down and its standby took over")
    healed = happens(lambda: len(pods("broker")) == len(servers)
                     and all(ready(p) for p in pods("broker")), 180)
    s.check(healed, "broker: the killed server back")
    if PROFILE == "ha":
        made_again_since(s, began, "broker: a server cut off and killed")


# ── Storage: bit rot, a torn write, a volume lost (step 3b part 2) ─────────


def scrub(component, container=None):
    """One scrub, run in the component's writer, as its JSON."""
    pod = writer_pod(component)
    done = kubectl("exec", pod, "-c", container or component, "--", BINARY[component], "scrub", check=False)
    for line in done.stdout.decode(errors="replace").splitlines():
        if line.strip().startswith("{"):
            return json.loads(line)
    raise SystemExit(f"{component}'s scrub said nothing it could read: {done.stderr.decode(errors='replace')[-400:]}")


def relation_file(table):
    """A table's first data file on its member's node: the node, and the path."""
    pod = primary()
    relative = psql(f"select pg_relation_filepath('{table}')", pod=pod)
    claim = f"data-{pod}"
    volume = out("get", "pvc", claim, "-o", "jsonpath={.spec.volumeName}")
    pv = json.loads(run("kubectl", "get", "pv", volume, "-o", "json").stdout)
    path = (pv["spec"].get("hostPath") or pv["spec"].get("local") or {}).get("path", "")
    return node_of(pod), f"{path}/pgdata/{relative}"


# Tables a class corrupted on purpose in this run, which a later class's
# read of every page passes over.
CORRUPTED = set()


def bit_rot(s):
    print("F: bit-rot: a captured row altered beneath the configuration store; a page's byte flipped", flush=True)
    wait_for("conductor settled", lambda: members_settled("conductor"), seconds=300)
    # A row of the configuration store's log altered beneath it.
    target = psql("select partition || ' ' || sequence || ' ' || ordinal from config_change "
                  "order by sequence desc, ordinal limit 1").split()
    s.check(len(target) == 3, "bit-rot: the configuration store's log holds a row to alter")
    if len(target) != 3:
        return
    partition, sequence, ordinal = target
    began = time.time()
    psql(f"update config_change set body = body || '{{\"rot\": true}}'::jsonb "
         f"where partition = '{partition}' and sequence = {sequence} and ordinal = {ordinal}")
    said = scrub("conductor")
    took = time.time() - began
    named = [f for f in said.get("findings", [])
             if f.get("partition") == partition and str(f.get("sequence")) == sequence]
    s.check(bool(named), f"bit-rot: the scrub names partition {partition}, sequence {sequence} "
            f"({len(said.get('findings', []))} finding(s))")
    kept = psql(f"select count(*) from config_integrity_finding where partition = '{partition}' "
                f"and sequence = {sequence}")
    s.check(kept == "1", f"bit-rot: kept as an IntegrityFinding ({kept})")
    probed = probe(writer_pod("conductor"), "conductor") or {}
    database = next((p for p in probed.get("paths", []) if p.get("path") == "database"), {})
    s.check(any(f.get("sequence") == int(sequence) for f in (database.get("integrity") or {}).get("findings", [])),
            "bit-rot: the probe that status reads says so")
    if named:
        claim(s, "bit-rot", "conductor", took, TOLERANT_POD if PROFILE == "tolerant" else 60,
              {"detected_by": "the scrub", "partition": partition, "sequence": int(sequence)})
    if PROFILE != "tolerant":
        return
    # A byte of a page flipped beneath the stopped database: read back, the
    # page fails its checksum, loudly, and the scrub records it.
    node, path = relation_file("config_change")
    CORRUPTED.add("config_change")
    member = primary()
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=0")
    # Gone, not going: a pod being deleted is not among pods(), and the flip
    # once landed while Postgres was still in its shutdown checkpoint, which
    # wrote the page the alteration above had dirtied back over it (#92's
    # merge-queue run 42418064524: shut down cleanly at 22:09:33, the flip
    # within that second, and the page read back whole by the select and the
    # scrub alike).
    wait_for("the database's pod gone", lambda: kubectl(
        "get", "pod", member, check=False).returncode != 0, seconds=180)
    flipped = "ffffffff"
    run("docker", "exec", node, "sh", "-c",
        f"printf '\\377\\377\\377\\377' | dd of='{path}' bs=1 seek=4000 conv=notrunc 2>/dev/null")
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=1")
    wait_for("the database back", lambda: pods("database") and all(ready(p) for p in pods("database"))
             and psql("select 1", database="postgres") == "1", seconds=300)
    # The bytes still flipped on disk once it serves: nothing wrote the page
    # again (a crash recovery replaying its full-page image would have).
    on_disk = run("docker", "exec", node, "sh", "-c",
                  f"dd if='{path}' bs=1 skip=4000 count=4 2>/dev/null | od -An -tx1 | tr -d ' \\n'",
                  check=False).stdout.decode().strip()
    s.check(on_disk == flipped, f"bit-rot: the flipped bytes are on disk as the database serves ({on_disk or 'unread'})")
    # Read through the heap, whatever the plan: count(body) needs every
    # tuple's body, which no index holds.
    refused = kubectl("exec", primary(), "-c", "postgres", "--", "psql", "-X", "-At", "-U", "postgres",
                      "-d", "meridian", "-c", "select count(body) from config_change", check=False)
    s.check(refused.returncode != 0 and b"invalid page" in refused.stderr,
            "bit-rot: a page whose byte was flipped fails its checksum when read")
    wait_for("conductor settled", lambda: answering("conductor"), seconds=300)
    said = scrub("conductor")
    s.check(any("checksum" in f.get("what", "") for f in said.get("findings", [])),
            "bit-rot: the scrub records the page that failed its checksum")


def torn_write(s):
    """The database killed outright under heavy commits; every table's pages
    read back after crash recovery (or on the promoted standby)."""
    print("F: torn-write: the database killed outright under heavy commits, every page read back", flush=True)
    db_primary(s, failure="torn-write")
    tables = psql("select string_agg(format('%I', tablename), ' ') from pg_tables where schemaname = 'public'")
    failed = []
    for table in tables.split():
        if table in CORRUPTED:
            s.note(f"torn-write: {table} passed over: bit-rot corrupted it on purpose in this run")
            continue
        done = kubectl("exec", primary(), "-c", "postgres", "--", "psql", "-X", "-At", "-U", "postgres",
                       "-d", "meridian", "-c", f"select count(*) from {table}", check=False)
        if done.returncode != 0:
            failed.append(table)
    s.check(not failed, f"torn-write: every table's pages read back after recovery "
            f"({len(tables.split())} tables{', failed: ' + ', '.join(failed) if failed else ''})")


def standby():
    """A standby of the database's, running and ready."""
    lead = primary()
    for pod in pods("database"):
        name = pod["metadata"]["name"]
        if name != lead and ready(pod):
            return name
    raise SystemExit("no standby is ready")


def member_file(pod, table):
    """A table's first data file on a member's node: the node, and the path."""
    relative = psql(f"select pg_relation_filepath('{table}')", pod=pod)
    volume = out("get", "pvc", f"data-{pod}", "-o", "jsonpath={.spec.volumeName}")
    pv = json.loads(run("kubectl", "get", "pv", volume, "-o", "json").stdout)
    path = (pv["spec"].get("hostPath") or pv["spec"].get("local") or {}).get("path", "")
    return node_of(pod), f"{path}/pgdata/{relative}"


def on_disk(node, path, at=4000, count=4):
    return run("docker", "exec", node, "sh", "-c",
               f"dd if='{path}' bs=1 skip={at} count={count} 2>/dev/null | od -An -tx1 | tr -d ' \\n'",
               check=False).stdout.decode().strip()


def replica_rot(s):
    """A standby's page rotted beneath it (step 3b part 4): the replica scrub
    Job finds its copy no longer the primary's, re-clones it through
    Patroni, and keeps the finding with its outcome."""
    print("F: replica-rot: a standby's page flipped beneath it; the replica scrub re-clones it", flush=True)
    member = standby()
    # The largest store log, so its first page is the likeliest full, which
    # no append then writes again before the scrub reads it.
    sizes = psql("select relname || ' ' || pg_relation_size(oid) from pg_class "
                 "where relname in ('config_change', 'instrument_change', 'street_change', "
                 "'lake_row', 'book_entry') and relkind = 'r'")
    table, size = max((line.split() for line in sizes.splitlines()), key=lambda t: int(t[1]))
    s.note(f"replica-rot: {table}, {size} bytes, its first page flipped")
    # The page settled everywhere first, so nothing replays over the flip:
    # frozen (no hint bit to set again and log), checkpointed on the
    # primary, replayed and restartpointed on the standby.
    psql(f"vacuum (freeze) {table}")
    psql("checkpoint")
    lsn = psql("select pg_current_wal_lsn()")
    wait_for("the standby to replay the checkpoint", lambda: psql(
        f"select pg_last_wal_replay_lsn() >= '{lsn}'::pg_lsn", pod=member) == "t", seconds=120)
    psql("checkpoint", pod=member)
    node, path = member_file(member, table)
    before = on_disk(node, path)
    run("docker", "exec", node, "sh", "-c",
        f"printf '\\377\\377\\377\\377' | dd of='{path}' bs=1 seek=4000 conv=notrunc 2>/dev/null")
    # Its server restarted, so the page is read again from disk; a clean
    # page in memory is never written back over it.
    kubectl("exec", member, "-c", "postgres", "--", "sh", "-c",
            patroni_api("restart", method="POST", body="{}") + " >/dev/null", quiet=True)
    wait_for("the standby streaming again", lambda: psql(
        "select pg_is_in_recovery()", pod=member) == "t", seconds=180)
    flipped = on_disk(node, path)
    s.check(flipped == "ffffffff" and flipped != before,
            f"replica-rot: the standby's page is flipped on disk as it streams ({flipped or 'unread'})")
    job = "e2e-replicas"
    kubectl("delete", "job", job, "--ignore-not-found", "--wait")
    began = time.time()
    kubectl("create", "job", job, "--from", f"cronjob/{FULL}-database-replicas")
    wait_for("the replica scrub Job to end", lambda: any(json.loads(out(
        "get", "job", job, "-o", "json"))["status"].get(k) for k in ("succeeded", "failed")),
        seconds=600)
    took = time.time() - began
    said = {}
    for line in out("logs", f"job/{job}", check=False).splitlines():
        if line.strip().startswith("{"):
            try:
                said = json.loads(line)
            except ValueError:
                pass
    found = [f for f in said.get("findings", []) if f.get("member") == member]
    s.check(said.get("replicas") == "differed" and bool(found),
            f"replica-rot: the replica scrub finds {member}'s copy differs ({said.get('replicas', 'nothing said')})")
    if not found:
        return
    finding = found[0]
    s.check("matches the primary's again" in finding.get("outcome", ""),
            f"replica-rot: {member} re-cloned through Patroni and matching ({finding.get('outcome', '')})")
    prefix = {"lake_row": "lake", "book_entry": "book"}.get(table, table.removesuffix("_change"))
    kept = psql(f"select count(*) from {prefix}_integrity_finding where member = '{member}' "
                "and partition = 'replicas' and what like '%re-cloned through Patroni%'")
    s.check(kept != "0", f"replica-rot: kept as an IntegrityFinding with its outcome ({kept})")
    s.check(on_disk(node, path) != "ffffffff", "replica-rot: the re-cloned copy's page is whole on disk")
    others = [f for f in said.get("findings", []) if f.get("member") != member]
    s.check(not others, f"replica-rot: no other member is named ({len(others)})")
    detected = (finding.get("detected_ms") or 0) / 1000
    claim(s, "replica-rot", "database-scrub", detected, REPLICA_DETECT,
          {"member": member, "job_s": round(took, 2)})
    # The re-clone's time only where it re-cloned and matched: a failed one
    # is a failure above, never a figure.
    if "matches the primary's again" in finding.get("outcome", "") and finding.get("recloned_ms"):
        claim(s, "replica-rot", "database", finding["recloned_ms"] / 1000, REPLICA_RECLONE,
              {"member": member, "outcome": "re-cloned, matching"})


def volume_lost(s):
    """A standby's volume lost: its claim and its pod deleted, and Patroni
    clones it again from the primary, nothing felt by the stores."""
    print("F: volume-lost: a database standby's claim deleted", flush=True)
    if PROFILE == "tolerant":
        s.note("volume-lost: the tolerant database's one volume lost is a restore, the admin's act "
               "(e2e-cluster-backup); not injected here")
        return
    standby = next(p["metadata"]["name"] for p in pods("database")
                   if p["metadata"]["labels"].get("meridian.dev/database-role") == "replica")
    epoch = epoch_now("bor")
    began = time.time()
    kubectl("delete", "pvc", f"data-{standby}", "--wait=false")
    kubectl("delete", "pod", standby, "--grace-period=0", "--force")
    # The claim goes once its pod has; a pod made again before that waits on
    # a claim being deleted, and is made again once it is gone.
    def gone():
        # Read once: gone, or a new claim with no deletion pending.
        said = kubectl("get", "pvc", f"data-{standby}", "-o", "jsonpath={.metadata.deletionTimestamp}",
                       check=False)
        return said.returncode != 0 or said.stdout.decode().strip() == ""
    wait_for("the lost claim gone", gone, seconds=120)
    pending = [p for p in json.loads(out("get", "pods", "-l", "meridian.dev/component=database", "-o", "json"))["items"]
               if p["metadata"]["name"] == standby and p["status"].get("phase") == "Pending"]
    if pending:
        kubectl("delete", "pod", standby, "--grace-period=0", "--force", check=False)
    back = happens(lambda: len(pods("database")) == database_members()
                   and all(ready(p) for p in pods("database"))
                   and kubectl("get", "pvc", f"data-{standby}", check=False).returncode == 0, 300, every=2)
    took = time.time() - began
    s.check(back, "volume-lost: the standby re-cloned onto a new volume and ready")
    s.check(answering("bor") is not None and epoch_now("bor") == epoch,
            "volume-lost: the book answered throughout, no takeover")
    if back:
        claim(s, "volume-lost", "database", took, 300, {"member": standby, "re_cloned": True})


# ── The zone class ────────────────────────────────────────────────────────

def zone_lost(s):
    """Every node of one zone stopped at once: the zone holding the most
    writers, so as many stores as can lose their writer together do. The
    database's members and the broker's servers are on the server node in
    this profile (their classes are their own), so what is measured is the
    stores' members' loss. Every store answers again within the zone tier,
    each from a member outside the zone, at a raised epoch where its writer
    was there."""
    print("F: zone: every node of one zone stopped at once, the zone holding the most writers", flush=True)
    for component in STORES:
        wait_for(f"{component} settled", lambda c=component: members_settled(c), seconds=300)
    writers = {c: writer_pod(c) for c in STORES}
    by_zone = {}
    for component, pod in writers.items():
        if pod:
            by_zone.setdefault(zone_of(node_of(pod)), []).append(component)
    zone, there = max(by_zone.items(), key=lambda held: len(held[1]))
    lost = [n["metadata"]["name"] for n in nodes()
            if n["metadata"].get("labels", {}).get("topology.kubernetes.io/zone") == zone]
    epochs = {c: epoch_now(c) for c in there if c != "launcher"}
    s.note(f"zone {zone}: {', '.join(lost)}; the writers there: {', '.join(sorted(there))}")
    began = time.time()
    for node in lost:
        run("docker", "stop", "--time", "0", node)
    took = {}

    def one(component):
        avoid = tuple(p["metadata"]["name"] for p in pods(component) if p["spec"].get("nodeName") in lost)
        return answering(component, avoid=avoid, above_epoch=epochs.get(component)) is not None

    try:
        from concurrent.futures import ThreadPoolExecutor
        with ThreadPoolExecutor(max_workers=len(STORES)) as asking:
            def back():
                left = [c for c in STORES if c not in took]
                for component, answered in zip(left, asking.map(one, left)):
                    if answered:
                        took[component] = time.time() - began
                return len(took) == len(STORES)
            happens(back, HA_ZONE + 30, every=0.5)
    finally:
        for node in lost:
            run("docker", "start", node)
    extra = {"zone": zone, "nodes": len(lost)}
    if DRIVEN:
        # RPO through the stores across the zone's loss: the database is on
        # the server node in this profile, so nothing acknowledged may go,
        # the lake's included.
        s.check(acks_again(int(began * 1000)), "zone: the write driver acknowledged by the book and the lake again")
        extra.update(rpo_extra(driver_rpo_held(s, "zone", store_rpo(int(began * 1000) - 30_000))))
    for component in STORES:
        if component not in took:
            s.check(False, f"zone: {component} never answered again from outside zone {zone} within {HA_ZONE + 30}s")
            explain(s, "zone", component)
            continue
        claim(s, "zone", component, took[component], HA_ZONE, dict(extra, writer_in_zone=component in there))
    wait_for("the zone's nodes back", lambda: all(
        any(c["type"] == "Ready" and c["status"] == "True" for c in n["status"]["conditions"])
        for n in nodes()), seconds=300)
    healed = all(happens(lambda c=component: members_settled(c), 300) for component in STORES)
    s.check(healed, f"zone: zone {zone}'s members back, as standbys")


# ── e2e-cluster-reconfigure: a live install moved between the profiles ────

def record_now():
    """Each store's change log as it stands, by partition: its sequences
    and epochs, which a move between profiles must keep as they were."""
    held = {}
    for store, table, partition, first, last, _ in CHANGE_LOGS:
        held[store] = psql(f"select {partition} || ':' || {first} || '-' || {last} || '@' || epoch "
                           f"from {table}")
    return held


# Why settled_everywhere last said no: the clause it stopped at and what it
# saw, for the check that waited on it (CI 44556229125 said only that the
# move to ha never settled, and its dump named the stores' members alone).
UNSETTLED = {"why": "", "at": 0.0}


def names_of(pods_said):
    return ", ".join(p["metadata"]["name"] for p in pods_said) or "none"


def unsettled():
    """Why the deployment is not settled everywhere: the first clause that
    does not hold and what it saw, or "" where every one holds."""
    if not all_ready():
        return "not every Deployment of the release is Available"
    for component in STORES:
        if not members_settled(component):
            live = pods(component)
            want = 2 if SHAPE in ("ha", "scaled") else 1
            return (f"{component}: {sum(ready(p) for p in live)} of {len(live)} running member(s) ready, "
                    f"{want} wanted (not ready: {names_of(p for p in live if not ready(p))})")
    database = pods("database")
    if len(database) != database_members() or not all(ready(p) for p in database):
        return (f"the database: {sum(ready(p) for p in database)} of {len(database)} running member(s) "
                f"ready, {database_members()} wanted (not ready: {names_of(p for p in database if not ready(p))})")
    every = pods("broker")
    servers = [p for p in every if p["metadata"]["labels"].get("meridian.dev/broker-set") == broker_set()]
    want = 3 if SHAPE == "ha" else 1
    if len(servers) != want or not all(ready(p) for p in servers):
        return (f"the broker: {sum(ready(p) for p in servers)} of {len(servers)} running server(s) of "
                f"{broker_set()} ready, {want} wanted (not ready: {names_of(p for p in servers if not ready(p))})")
    if len(every) != len(servers):
        return (f"the broker: server(s) of another set still running beside {broker_set()}: "
                f"{names_of(p for p in every if p not in servers)}")
    return ""


def settled_everywhere():
    UNSETTLED["why"], UNSETTLED["at"] = unsettled(), time.time()
    return not UNSETTLED["why"]


def broker_health(s, when):
    """Each broker server's pod and health, for a check that waited on the
    deployment settling: its set, readiness and restarts, and its full
    health check with its details (CI 44556229125: a server 0/1 for over
    30 minutes, its consumer "not current" at every check, not restarted)."""
    try:
        every = json.loads(out("get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE},meridian.dev/component=broker",
                               "-o", "json", check=False) or "{}").get("items", [])
    except ValueError as failed:
        s.note(f"{when}: the broker's servers could not be listed: {failed}")
        return
    if not every:
        s.note(f"{when}: no broker server is there")
    for pod in every:
        name = pod["metadata"]["name"]
        status, said = monitored(name, ("/healthz?details=true",))["/healthz?details=true"]
        restarted = sum(c.get("restartCount", 0) for c in pod["status"].get("containerStatuses", []))
        s.note(f"{when}: {name} (set {pod['metadata'].get('labels', {}).get('meridian.dev/broker-set', '?')}) "
               f"{pod['status'].get('phase')}, ready {ready(pod)}, restarted {restarted} time(s); "
               f"/healthz?details=true {status} {' '.join(said.split())[:600]}")


def not_settled(s, when):
    """What a wait for every member found last, where it ended unsettled:
    the clause that did not hold, and every broker server's health."""
    ago = round(time.time() - UNSETTLED["at"]) if UNSETTLED["at"] else None
    s.note(f"{when}: not settled, at the last look ({'never looked' if ago is None else f'{ago} s ago'}): "
           f"{UNSETTLED['why'] or 'every member, database member and broker server ready'}")
    broker_health(s, when)


def helm_takes_counts():
    """Helm 4 applies server-side, and the database's member count was set
    by first run's scale (a field manager of its own): the chart, whose
    value the count is, takes it back (`--force-conflicts`), as `meridian
    upgrade-deployment` must under Helm 4. Helm 3 merges, and needs nothing."""
    said = run("helm", "version", "--short", check=False).stdout.decode()
    return ["--force-conflicts"] if said.startswith("v4") else []


def upgrade(*sets):
    """An upgrade as `meridian upgrade-deployment --set` runs Helm (meridian-
    cli, upgrade_deployment.rs): this chart's defaults under the deployment's
    own values (--reset-then-reuse-values), fifteen minutes for its hooks,
    no --wait (the CLI waits itself, as this run does after), and under
    Helm 4 the chart's own counts taken back (--force-conflicts). CI runs it
    on Helm 4.2.3, the newest the CLI meets and the stricter (ci.yaml)."""
    values = [arg for value in sets for arg in ("--set", value)]
    # The pin is this run's, not the chart's: Helm puts the chart's spread
    # back on the sets it pinned (Helm 3 merges it in, Helm 4 applies it),
    # the zones' node affinity beside the pin's selector of the server node,
    # which no zone holds, and the member each set rolled first could never
    # be placed again (failover-repeat 43078770407, ha run 1, and PR CI
    # 43078729225: broker-jetstream-2 and database-2 Pending, "4 node(s)
    # didn't match Pod's node affinity/selector", from write-ahead-bound's
    # upgrade on). So each pinned set is held from rolling through the
    # upgrade (its partition at its count) and its pin put back after, the
    # roll let go: a template the upgrade changed rolls as the pin places it,
    # one it left as it was rolls nothing.
    held = {name: int(out("get", "statefulset", name, "-o", "jsonpath={.spec.replicas}") or 0)
            for name in pinned_sets()}
    for name, count in held.items():
        kubectl("patch", "statefulset", name, "--type", "merge", "-p", json.dumps(
            {"spec": {"updateStrategy": {"type": "RollingUpdate", "rollingUpdate": {"partition": count}}}}))
    try:
        return run("helm", "upgrade", RELEASE, CHART, "--namespace", NAMESPACE, "--reset-then-reuse-values",
                   "--timeout", "15m", *helm_takes_counts(), *values, check=False)
    finally:
        for name in held:
            back = pinned()
            back["spec"]["updateStrategy"] = {"type": "RollingUpdate", "rollingUpdate": {"partition": 0}}
            kubectl("patch", "statefulset", name, "--type", "merge", "-p", json.dumps(back))


def revision():
    """The release's revision now."""
    return int(json.loads(run("helm", "status", RELEASE, "--namespace", NAMESPACE, "-o", "json")
                          .stdout)["version"])


def migrated(s, when):
    """The migration Job of the revision just applied run to its end, and
    succeeded: the CLI reports an upgrade whose migration failed as failed,
    though the components settle (step 3b's CLI rehearsal, ha -> tolerant).
    Its containers' logs said where it did not."""
    job = f"{FULL}-migrate-{revision()}"

    def finished():
        status = json.loads(out("get", "job", job, "-o", "json")).get("status", {})
        return status.get("succeeded") or status.get("failed")
    ended = happens(finished, 600, every=2)
    status = json.loads(out("get", "job", job, "-o", "json", check=False) or "{}").get("status", {})
    held = ended and bool(status.get("succeeded"))
    s.check(held, f"{when}: the migration, job/{job}, succeeded"
            + ("" if held else f" ({'failed' if status.get('failed') else 'never finished'})"))
    if not held:
        for pod in out("get", "pods", "-l", f"job-name={job}", "-o", "name", check=False).split():
            said = kubectl("logs", pod, "--all-containers", "--prefix", "--timestamps", "--tail=40",
                           check=False).stdout.decode(errors="replace")
            print(f"R: {pod} said:\n{said}", flush=True)
        database_said(s, when)


def tls_turned_on(s):
    """Internal TLS turned on by an upgrade, on a database serving in the
    clear (step 3b part 6): its preflight runs first and passes, connecting
    as the database serves now -- before, it waited on mounting the CA the
    same upgrade makes, until it failed -- then every connection over TLS
    and the plaintext one refused, and the driver acknowledged again."""
    global TLS
    print("R: internal TLS turned on by an upgrade, its preflight first", flush=True)
    began = time.time()
    with HookLog("preflight") as preflight:
        upgraded = upgrade("tls.internal.enabled=true")
    print(f"R: the preflight said:\n{preflight.said or '(nothing read)'}", flush=True)
    s.check(upgraded.returncode == 0, "internal TLS on: the upgrade applied, its preflight passed first"
            + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
    if upgraded.returncode != 0:
        database_said(s, "internal TLS on")
        raise SystemExit("internal TLS could not be turned on")
    hooks = run("helm", "get", "hooks", RELEASE, "--namespace", NAMESPACE, check=False).stdout.decode()
    s.check(f"name: {FULL}-preflight" in hooks,
            "internal TLS on: the upgrade ran with its preflight, which read the database as it served")
    migrated(s, "internal TLS on")
    settled = happens(settled_everywhere, 900, every=5)
    s.check(settled, "internal TLS on: every member ready after the upgrade")
    figure("internal TLS on: every member ready after the upgrade", took_s=round(time.time() - began))
    if not settled:
        not_settled(s, "internal TLS on")
        raise SystemExit("the deployment did not settle with internal TLS on")
    upgrade_rolled_out(s, "internal TLS on")
    TLS = True
    internal_tls(s)
    if DRIVEN:
        s.check(acks_again(int(began * 1000), seconds=180),
                "internal TLS on: the write driver acknowledged by the book and the lake again")


def database_said(s, when):
    """What the database and the preflight said, where an upgrade's hook
    refused: each member's pod, when it was made, its manager and its role
    label, the Service's endpoints, and the preflight's own report."""
    said = json.loads(out("get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE},meridian.dev/component=database",
                          "-o", "json", check=False) or '{"items": []}')["items"]
    for pod in said:
        labels = pod["metadata"].get("labels", {})
        s.note(f"{when}: {pod['metadata']['name']} {pod['status'].get('phase')}, ready {ready(pod)}, "
               f"made {pod['metadata'].get('creationTimestamp')}, manager "
               f"{labels.get('meridian.dev/database-manager', 'none')}, role {labels.get('meridian.dev/database-role', 'none')}")
    s.note(f"{when}: the database Service's endpoints: "
           + (out("get", "endpoints", f"{FULL}-database", "-o", "jsonpath={.subsets}", check=False) or "none"))
    logs = kubectl("logs", f"job/{FULL}-preflight", "--all-containers", "--timestamps", "--tail=20", check=False)
    s.note(f"{when}: the preflight said: {logs.stdout.decode(errors='replace').strip()[-2000:] or 'nothing'}")


def rollout_unfinished():
    """Each Deployment and StatefulSet of the release whose rollout is not
    finished, and why, judged as `meridian upgrade-deployment` judges it
    before it says an upgrade is done (e2e/cluster/rollout.py)."""
    items = {kind: json.loads(out("get", kind, "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]
             for kind in ("deployments", "statefulsets")}
    return rollout.unfinished(items["deployments"], items["statefulsets"])


def rollout_left():
    """What of the release has not rolled out, but the sharded instance's
    standby pod, never ready by design (scaled)."""
    return [left for left in rollout_unfinished() if SHARDED not in left]


def rolled_out():
    """Every workload of the release rolled out (rollout_left). Not
    `condition=Available` (all_ready): a Deployment that replaces a pod at a
    time is Available all through its rollout, an old pod still to go (PR
    CI 44604574060: the dashboard's, two copies, maxSurge 0)."""
    return not rollout_left()


def upgrade_rolled_out(s, what, seconds=600):
    """After an upgrade, every workload of the release rolled out, as
    `meridian upgrade-deployment` waits for it before it says the upgrade
    is done; checked, and says whether."""
    done = happens(rolled_out, seconds, every=5)
    s.check(done, f"{what}: every workload of the release rolled out after the upgrade"
            + ("" if done else f" ({'; '.join(rollout_left()[:4])})"))
    return done


def ready_of(pod):
    return bool(pod) and ready(pod)


class HookLog:
    """A hook Job's log, read every second while an upgrade runs, the last
    read kept: a hook that passes is deleted with its pod when it succeeds,
    so what it said -- how long it waited for the database, and why -- is
    read while it runs or never."""

    def __init__(self, job):
        import threading
        self.job = f"{FULL}-{job}"
        self.said = ""
        self.seen = []
        self.done = threading.Event()
        self.thread = threading.Thread(target=self.follow, daemon=True)

    def follow(self):
        last = None
        while not self.done.is_set():
            done = kubectl("logs", f"job/{self.job}", "--all-containers", "--timestamps", "--tail=60", check=False)
            text = done.stdout.decode(errors="replace").strip()
            if done.returncode == 0 and text:
                self.said = text
            # Beside it, what the database's Service routes to and what
            # member 0 says of itself, each change kept with its time: a
            # refused connection is either no endpoint behind the Service,
            # or a new pod not yet admitted by the database's policy.
            ready = out("get", "endpoints", f"{FULL}-database", "-o",
                        "jsonpath={.subsets[*].addresses[*].ip}/{.subsets[*].notReadyAddresses[*].ip}", check=False)
            pod = json.loads(out("get", "pod", f"{FULL}-database-0", "-o", "json", check=False) or "{}")
            now = (f"endpoints {ready or '/'}; member 0 {pod.get('status', {}).get('podIP', '')} role "
                   f"{pod.get('metadata', {}).get('labels', {}).get('meridian.dev/database-role', 'none')} "
                   f"ready {ready_of(pod)}")
            if now != last:
                self.seen.append(f"{time.strftime('%H:%M:%S', time.gmtime())} {now}")
                last = now
            self.done.wait(1)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.done.set()
        self.thread.join(timeout=20)


def member_now(when):
    """Which of the database's StatefulSet revisions member 0 is from, and
    what it is: printed, for reading a move under Patroni."""
    sts = json.loads(out("get", "statefulset", f"{FULL}-database", "-o", "json", check=False) or "{}")
    pod = json.loads(out("get", "pod", f"{FULL}-database-0", "-o", "json", check=False) or "{}")
    labels = pod.get("metadata", {}).get("labels", {})
    revision = labels.get("controller-revision-hash", "")
    status = sts.get("status", {})
    print(f"R: {when}: member 0 from revision {revision} (the set's current {status.get('currentRevision')}, "
          f"update {status.get('updateRevision')}), made {pod.get('metadata', {}).get('creationTimestamp')}, "
          f"manager {labels.get('meridian.dev/database-manager', 'none')}, "
          f"role {labels.get('meridian.dev/database-role', 'none')}, ready {ready(pod) if pod else False}, "
          f"going {bool(pod.get('metadata', {}).get('deletionTimestamp'))}", flush=True)


def connections_held(s, when):
    """The database's max_connections against the count the chart gave it
    (ruled 2026-11-08: counted from what the chart deploys), on every
    member, and the connections in use. A member still at another count --
    a start-only setting changed under Patroni and its restart never asked
    for -- refuses the new pods' connections once they pass it."""
    set_ = json.loads(out("get", "statefulset", f"{FULL}-database", "-o", "json"))
    container = next(c for c in set_["spec"]["template"]["spec"]["containers"] if c["name"] == "postgres")
    counted = None
    for arg in container.get("args", []):
        if arg.startswith("max_connections="):
            counted = int(arg.split("=", 1)[1])
    for env in container.get("env", []):
        if env["name"] == "PATRONI_DYNAMIC":
            counted = int(json.loads(env["value"])["postgresql"]["parameters"]["max_connections"])
    members = [p["metadata"]["name"] for p in json.loads(out(
        "get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE},meridian.dev/component=database",
        "-o", "json"))["items"] if not p["metadata"].get("deletionTimestamp")]
    allowed = {member: psql("show max_connections", pod=member, quiet=True) for member in sorted(members)}
    in_use = psql("select count(*) from pg_stat_activity where backend_type = 'client backend'", quiet=True)
    print(f"R: {when}: the chart counted {counted} connections; the members allow "
          f"{', '.join(f'{m} {n}' for m, n in allowed.items())}; {in_use} in use", flush=True)
    s.check(counted is not None and all(n == str(counted) for n in allowed.values()),
            f"{when}: every database member allows the {counted} connections the chart counted")
    s.check(counted is not None and int(in_use or 0) < counted,
            f"{when}: {in_use} connections in use, under the {counted} allowed")


def under_patroni():
    """Whether the database runs under Patroni now: once it has, it stays
    under it, standbys or none (templates/database.yaml)."""
    set_ = json.loads(out("get", "statefulset", f"{FULL}-database", "-o", "json"))
    return set_["spec"]["template"]["metadata"].get("labels", {}).get("meridian.dev/database-manager") == "patroni"


# How much of each container's log a failed check keeps: the Makefile's
# E2E_K3D_LOG_LINES, which .e2e-cluster.log keeps at a run's end too. 5000
# lines: ten minutes and more of every quiet container (a database member
# under Patroni says something every two seconds), and minutes of one
# answering a flood (memory-pressure's refusals are a line each), so the
# window a class failed in survives in the dump taken as it fails.
SAID_LINES = os.environ.get("E2E_K3D_LOG_LINES", "5000")
SAID_KEPT = []

# The broker's own view of itself (stage D2's open item): each server's
# health, JetStream (its meta leader, its streams' and consumers' state)
# and routes, from its monitoring endpoint, which listens on the pod's
# loopback alone (`http: 127.0.0.1:8222`, broker_config) and is reached by
# a port-forward, which lands on the pod's own loopback.
BROKER_MONITOR_PORT = 8222
# Its full health with the details of every check that failed, and every
# Raft group it holds -- the metadata group, each stream's and each
# consumer's -- not the metadata group alone (CI 44556229125: a consumer's
# group "not current" on one server, which neither showed). /raftz names
# one account's groups, the system account's where none is asked: the
# metadata group there, the streams' and consumers' in the global account,
# $G, where the deployment's components keep them.
BROKER_SAID = ("/healthz?details=true", "/jsz?streams=true&consumers=true&config=true",
               "/raftz", "/raftz?acc=$G", "/routez", "/varz")


def broker_servers():
    """The broker's servers' pods, by name."""
    return [p["metadata"]["name"] for p in json.loads(
        out("get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE},meridian.dev/component=broker",
            "-o", "json", check=False) or "{}").get("items", [])]


def broker_said():
    """Each broker server's monitoring endpoint's answers, in text, for a
    failed check's dump: never raises."""
    parts = []
    try:
        servers = broker_servers()
    except (ValueError, SystemExit) as failed:
        return [f"== the broker's servers could not be listed: {failed}"]
    for server in servers:
        for path, (_, said) in monitored(server, BROKER_SAID).items():
            parts.append(f"== {server}, {path}")
            parts.append(said)
    return parts


def broker_jetstream_said():
    """Each broker server's JetStream as it sees itself, a few lines each,
    for the job's log: its full health, its storage reserved against its
    bound, its metadata group (the leader, the size, and the peers it
    knows, itself among them or not: CI 43539633884 had a server outside
    the group while every /healthz said ok), and each
    stream with its replicas, its bound and where it is placed. Never
    raises."""
    paths = ("/healthz", "/jsz?accounts=true&streams=true&config=true", "/raftz?group=_meta_")
    lines = []
    try:
        servers = broker_servers()
    except (ValueError, SystemExit) as failed:
        return [f"the broker's servers could not be listed: {failed}"]
    for server in servers:
        said = monitored(server, paths)
        status, health = said[paths[0]]
        lines.append(f"{server}: /healthz {status} {health.strip()[:160]}")
        try:
            jsz = json.loads(said[paths[1]][1])
            config = jsz.get("config") or {}
            meta = jsz.get("meta_cluster") or {}
            lines.append(f"{server}:   storage reserved {jsz.get('reserved_storage')} of "
                         f"{config.get('max_storage')}, used {jsz.get('storage')}; memory reserved "
                         f"{jsz.get('reserved_memory')} of {config.get('max_memory')}; meta leader "
                         f"{meta.get('leader')!r}, cluster_size {meta.get('cluster_size')}")
            for account in jsz.get("account_details") or []:
                for stream in account.get("stream_detail") or []:
                    sc = stream.get("config") or {}
                    cluster = stream.get("cluster") or {}
                    lines.append(
                        f"{server}:   stream {account.get('name')} > {stream.get('name')}: replicas "
                        f"{sc.get('num_replicas')}, max_bytes {sc.get('max_bytes')}, leader "
                        f"{cluster.get('leader')!r}, replicas at "
                        f"{[r.get('name') for r in cluster.get('replicas') or []]}")
        except (ValueError, AttributeError, TypeError) as failed:
            lines.append(f"{server}:   /jsz unread: {failed}: {said[paths[1]][1][:160]}")
        try:
            groups = json.loads(said[paths[2]][1])
            group = next(iter(groups.values()))["_meta_"]
            known = sorted(p.get("name") for p in (group.get("peers") or {}).values() if p.get("known"))
            # Its own peer is never listed; a member knows size - 1 others.
            member = len(known) == (group.get("size") or 0) - 1
            lines.append(f"{server}:   meta group as it sees it: {group.get('state')}, size "
                         f"{group.get('size')}, leader {group.get('leader')!r}, knows {known}: "
                         f"{'a member' if member else 'NOT a member'}")
        except (ValueError, AttributeError, TypeError, KeyError, StopIteration) as failed:
            lines.append(f"{server}:   /raftz unread: {failed}: {said[paths[2]][1][:160]}")
    return lines


def monitored(server, paths):
    """One broker server's monitoring endpoint's answers to `paths`, each
    its HTTP status (0 where none came) and its text: never raises."""
    answers = {}
    forward = subprocess.Popen(
        ["kubectl", "--namespace", NAMESPACE, "port-forward", f"pod/{server}", f":{BROKER_MONITOR_PORT}"],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        # Read on a thread of its own: a port-forward that can reach
        # nothing (the API server cut, a node gone) may say nothing and
        # never end, and the dump must not wait on it.
        lines = queue.Queue()
        threading.Thread(target=lambda: [lines.put(line) for line in forward.stdout],
                         daemon=True).start()
        local = None
        began = time.time()
        while local is None and time.time() - began < 10:
            try:
                line = lines.get(timeout=1).decode(errors="replace")
            except queue.Empty:
                continue
            found = re.search(r"Forwarding from 127\.0\.0\.1:(\d+)", line)
            if found:
                local = found.group(1)
        if local is None:
            return {path: (0, "its monitoring endpoint: no port-forward within 10 s") for path in paths}
        for path in paths:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{local}{path}", timeout=5) as answered:
                    answers[path] = (answered.status, answered.read().decode(errors="replace"))
            except urllib.error.HTTPError as refused:
                answers[path] = (refused.code,
                                 f"HTTP {refused.code}: {refused.read().decode(errors='replace')}")
            except (urllib.error.URLError, OSError) as failed:
                answers[path] = (0, f"unanswered: {failed}")
    finally:
        forward.terminate()
        try:
            forward.wait(timeout=5)
        except subprocess.TimeoutExpired:
            forward.kill()
    return answers


def broker_formed(servers):
    """Every one of the broker's `servers` up and saying JetStream is
    healthy -- its routes, a meta leader and its own state current
    (`/healthz`, NATS's full check) -- so nothing makes a stream on a
    cluster still forming."""
    names = broker_servers()
    if len(names) != servers:
        return False
    return all(monitored(name, ("/healthz",))["/healthz"][0] == 200 for name in names)


def cluster_said(when):
    """What the cluster said when a check failed, kept at once beside the
    run (.e2e-said-<n>.log, which CI keeps with .e2e-cluster.log): the
    pods, the namespace's events in order, and each container's log and
    its previous one, timestamped. For reconfigure, whose later moves
    replace every pod a failed check was about."""
    SAID_KEPT.append(when)
    name = f".e2e-said-{len(SAID_KEPT)}.log"
    parts = [f"== {when}, at {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}"]
    parts.append(kubectl("get", "pods", "-o", "wide", check=False).stdout.decode(errors="replace"))
    parts.append(kubectl("get", "events", "--sort-by=.lastTimestamp", check=False).stdout.decode(errors="replace"))
    parts.extend(broker_said())
    for pod in json.loads(out("get", "pods", "-o", "json", check=False) or "{}").get("items", []):
        pod_name = pod["metadata"]["name"]
        spec = pod.get("spec", {})
        for container in [c["name"] for c in spec.get("initContainers", []) + spec.get("containers", [])]:
            for previous in ((), ("--previous",)):
                said = kubectl("logs", pod_name, "-c", container, "--timestamps", f"--tail={SAID_LINES}",
                               *previous, check=False)
                parts.append(f"== {pod_name}, {container}{', before, if it restarted' if previous else ''}")
                parts.append((said.stdout or said.stderr).decode(errors="replace"))
    with open(name, "w", encoding="utf-8") as kept:
        kept.write("\n".join(parts))
    print(f"R: {when}: what the cluster said, kept in {name}", flush=True)


def jetstream_claims():
    """The JetStream broker set's volume claims, by name: its servers'
    `jetstream-<set>-<n>`, with each one's creation time."""
    said = json.loads(out("get", "pvc", "-o", "json", check=False) or "{}").get("items", [])
    prefix = f"jetstream-{FULL}-broker-jetstream-"
    return {c["metadata"]["name"]: c["metadata"]["creationTimestamp"] for c in said
            if c["metadata"]["name"].startswith(prefix)}


def jetstream_servers():
    """The JetStream broker set's servers running and not going, by name."""
    try:
        return [p["metadata"]["name"] for p in pods("broker")
                if p["metadata"]["labels"].get("meridian.dev/broker-set") == f"{FULL}-broker-jetstream"]
    except (ValueError, SystemExit):
        return []


def write_ahead_last():
    """The furthest the lake's write-ahead stream has reached, as any
    JetStream server says it (/jsz): 0 where none says. Never raises."""
    last = 0
    for server in jetstream_servers():
        _, said = monitored(server, ("/jsz?accounts=true&streams=true",))["/jsz?accounts=true&streams=true"]
        try:
            for account in json.loads(said).get("account_details") or []:
                for stream in account.get("stream_detail") or []:
                    if stream.get("name") == WRITE_AHEAD_STREAM:
                        last = max(last, int((stream.get("state") or {}).get("last_seq") or 0))
        except (ValueError, AttributeError, TypeError):
            continue
    return last


# What the lake's writer says where the stream's identity changed under it
# (crates/lake/src/write_ahead.rs, write_forever): the stream deleted and
# made again under its name. Never on a server lost or a leader moved.
MADE_AGAIN = "the write-ahead stream was made again under its name"


def lake_said_since(began):
    """Each lake pod's logs since `began` (a time.time()), all containers."""
    return {p["metadata"]["name"]: kubectl("logs", p["metadata"]["name"], "--all-containers",
                                           f"--since={int(time.time() - began) + 30}s", check=False,
                                           quiet=True).stdout.decode(errors="replace")
            for p in pods("lake")}


def made_again_since(s, began, what):
    """The review of item 191's finding 8: the stream's identity (its
    `created`) is the same through a broker server lost or its leader
    moved, so the lake never takes it as a stream made again."""
    said = sorted(name for name, log in lake_said_since(began).items() if MADE_AGAIN in log)
    s.check(not said, f"{what}: the lake never took its write-ahead stream as made again "
            f"({', '.join(said) or 'no lake pod said so'})")


def write_ahead_places():
    """The lake's committed places in its write-ahead streams, by stream
    identity (item 191: kept under `LAKE_WRITE_AHEAD.<identity>`, and
    `.group-<g>` past group 0): each identity's slowest group's sequence."""
    places = {}
    for line in psql("select stream, sequence from lake_write_ahead "
                     f"where stream like '{WRITE_AHEAD_STREAM}.%'", quiet=True).splitlines():
        key, _, sequence = line.partition("|")
        identity = key.split(".")[1]
        if identity.isdigit():
            places[identity] = min(places.get(identity, int(sequence)), int(sequence))
    return places


def reconfigure(s):
    global SHAPE
    failed_before = len(s.failures)
    tls_turned_on(s)
    archived_in_tolerant(s)
    if len(s.failures) > failed_before:
        cluster_said("reconfigure's internal TLS and archive")
    print("R: the live install moved between the profiles, under the write driver's load", flush=True)
    identities_before = None
    left_ha_ns = 0
    for target in RECONFIGURE:
        failed_before = len(s.failures)
        # Ended early -- a wait that stopped the move, or a return after a
        # failed check -- unless the move's last line is reached.
        stopped = True
        try:
            before = record_now()
            # Alone, it moves under Patroni in two upgrades; under it already
            # (back from ha), one upgrade gives it its standbys.
            alone = not under_patroni()
            print(f"R: {SHAPE} -> {target}, by the value `meridian upgrade-deployment --set` gives", flush=True)
            # Leaving ha (item 191): the furthest the write-ahead stream
            # reached before its set went, read until its servers are gone.
            leaving = SHAPE == "ha" and target == "tolerant"
            reached = [write_ahead_last()] if leaving else []
            began = time.time()
            upgraded = upgrade(f"resilience.profile={target}")
            if leaving:
                def gone():
                    reached.append(write_ahead_last())
                    return not jetstream_servers()
                happens(gone, 180, every=2)
            s.check(upgraded.returncode == 0, f"reconfigure: helm upgrade to {target} applied"
                    + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
            if upgraded.returncode != 0:
                return
            SHAPE = target
            # Each upgrade's migration succeeds, the way back included, where
            # the database's own restart for a start-only setting once cut it.
            migrated(s, f"reconfigure to {target}")
            if target == "ha" and alone:
                # A database alone moves under Patroni first, alone, and is given
                # its standbys by a second upgrade with the same values, as the
                # chart says. Run once the first has rolled out, as `meridian
                # upgrade-deployment` waits for every workload of the release
                # before it returns: member 0's postgres answering on its own
                # address is not the Service routing to it, which it does only
                # once Patroni says the member is ready, and a second upgrade
                # started before that met its preflight refused (CI runs
                # 42453056096, 42462993343 and 42482853675's run 3: no ready
                # endpoint for over a minute).
                said = upgraded.stdout.decode(errors="replace")
                moved = "upgrade again with" in said
                s.check(moved, "reconfigure: the chart says the database moves under Patroni first, alone")
                if moved:
                    wait_for("the first upgrade rolled out, as meridian upgrade-deployment waits for it",
                             lambda: not rollout_unfinished(), seconds=600)
                    print("R: the first upgrade rolled out; the second upgrade", flush=True)
                    figure("reconfigure: the first upgrade rolled out", took_s=round(time.time() - began))
                    member_now("before the second upgrade")
                    with HookLog("preflight") as preflight:
                        again = upgrade(f"resilience.profile={target}")
                    print(f"R: the second upgrade's preflight said:\n{preflight.said or '(nothing read)'}", flush=True)
                    print("R: the database's Service and member 0 meanwhile:\n" + "\n".join(preflight.seen), flush=True)
                    member_now("after the second upgrade")
                    s.check(again.returncode == 0, "reconfigure: the second upgrade adds the database's standbys"
                            + ("" if again.returncode == 0 else f": {again.stderr.decode(errors='replace')[-400:]}"))
                    if again.returncode != 0:
                        database_said(s, "the second upgrade")
                    else:
                        migrated(s, f"reconfigure to {target}, the second upgrade")
            settled = happens(settled_everywhere, 900, every=5)
            took = time.time() - began
            s.check(settled, f"reconfigure: every member in the {target} shape and ready after the upgrade")
            if not settled:
                not_settled(s, f"reconfigure to {target}")
                for component in STORES:
                    explain(s, f"reconfigure to {target}", component)
                return
            upgrade_rolled_out(s, f"reconfigure to {target}")
            connections_held(s, f"reconfigure to {target}")
            extra = {"from": "tolerant" if target == "ha" else "ha"}
            if leaving:
                # JetStream's volumes go with its set, and the writes the
                # stream held past the lake's place are stranded: never
                # answered, so their plugins ask again (item 191; no drain).
                deleted = happens(lambda: not jetstream_claims(), 180, every=2)
                s.check(deleted, "reconfigure to tolerant: JetStream's volumes deleted with its set (item 191)"
                        + ("" if deleted else f": {sorted(jetstream_claims())} left"))
                # What the stream held past the lake's slowest writer
                # group's committed place as its set went: an upper bound on
                # the writes stranded, never answered. A write past it that
                # its own range's group committed, or that its plugin's
                # retry already sent again, is in it too (the review of item
                # 191's finding 10).
                places = write_ahead_places()
                place = places[max(places, key=int)] if places else 0
                past = max(0, max(reached) - place)
                figure("reconfigure: writes the write-ahead stream held past the lake's slowest group's "
                       "place leaving ha (at most this many stranded)",
                       stranded_at_most=past, reached=max(reached), place=place)
                extra["stranded_at_most"] = past
                identities_before = set(places)
                left_ha_ns = time.time_ns()
            elif target == "ha" and identities_before is not None:
                # Back to ha (item 191): the servers on volumes made by this
                # upgrade, and the lake in the stream made again, its own
                # place kept under its identity, taken from its first write.
                since = datetime.datetime.fromtimestamp(began, datetime.timezone.utc)
                claims = jetstream_claims()
                fresh = len(claims) == 3 and all(
                    datetime.datetime.fromisoformat(made.replace("Z", "+00:00")) >= since.replace(microsecond=0)
                    for made in claims.values())
                s.check(fresh, f"reconfigure back to ha: the broker's servers on fresh volumes (item 191): {claims}")
                taken = happens(lambda: set(write_ahead_places()) - identities_before, 180, every=5)
                s.check(taken, "reconfigure back to ha: the lake keeps its place in the stream made again "
                        f"under its own identity (before {sorted(identities_before)}, now "
                        f"{sorted(write_ahead_places())})")
                # Nothing counted lost (the review of item 191's finding
                # 10): no run the lake found dropped under the new stream's
                # identity, and no dataset gap recorded since ha was left.
                new = sorted(set(write_ahead_places()) - identities_before)
                runs = psql("select count(*) from lake_write_ahead_dropped where "
                            + " or ".join(f"stream like '{WRITE_AHEAD_STREAM}.{identity}%'" for identity in new),
                            quiet=True).strip() if new else "?"
                gaps = psql(f"select count(*) from lake_dataset_gap where recorded_at_ns >= {left_ha_ns}",
                            quiet=True).strip()
                s.check(runs == "0" and gaps == "0",
                        "reconfigure back to ha: the lake took the stream made again from its first write, "
                        f"nothing counted lost ({runs} runs found dropped in it, {gaps} dataset gaps since ha "
                        "was left)")
            if DRIVEN:
                # RPO through the stores across the move: the database moved
                # under Patroni and back, every member replaced, and nothing
                # acknowledged lost, the lake's included (no primary was lost).
                s.check(acks_again(int(began * 1000), seconds=180),
                        f"reconfigure to {target}: the write driver acknowledged by the book and the lake again")
                extra.update(rpo_extra(driver_rpo_held(s, f"reconfigure to {target}", store_rpo(int(began * 1000)))))
            claim(s, f"reconfigure-{target}", "deployment", took, None, extra)
            after = record_now()
            kept = all(set(before[store].splitlines()) <= set(after[store].splitlines()) for store in before)
            s.check(kept, f"reconfigure: each store's change log as it was before the move, added to and nothing else")
            properties(s, f"reconfigure to {target}")
            if target == "tolerant" and DRIVEN:
                # Stage T: the writer-group count raised and lowered, here,
                # where one member of each group serves and never surges.
                group_counts(s)
            stopped = False
        finally:
            # What every pod and the namespace said at the move a check
            # failed in, kept now: the moves after it replace every member,
            # so the run's own record at its end no longer holds them (PR CI
            # 43022080248: the first move to ha failed once, its pods' logs
            # gone by then).
            if stopped or len(s.failures) > failed_before:
                cluster_said(f"reconfigure to {target}")
    if DRIVEN and SHAPE == "ha":
        failed_before = len(s.failures)
        try:
            two_groups_two_databases(s)
        finally:
            if len(s.failures) > failed_before:
                cluster_said("reconfigure's two groups on two databases")


# ── Stage T (step 3d; the resilience spec's section 15) ───────────────────
#
# Rollouts keep every public topic served, and the online conversions of
# the writer-group count. Every group's writer serves its store's public
# topics and hands a write on to its owner, so with two groups an upgrade
# that restarts both leaves a topic unserved only where both are down in
# the same seconds (PR CI 43636763392: about 2 s, tolerant). A writer of a
# group past the first, told to stop, serves on until another group's
# writer serves anew (approved 2026-11-15), and a member says it is ready
# only once the broker holds its subscriptions. Measured here on every
# upgrade of this part: the driver's writes refused "nothing serves".

UNSERVED = "nothing serves"
# What each store's driver writes, and the store's component.
SERVED_BY = (("price", "lake"), ("order", "bor"))
# A value that changes nothing a member reads or writes, and restarts every
# member of both stores: how often its writer re-reads its record.
SCRUB_DAYS = 8


def not_answered(began_ms, ended_ms):
    """The driver's writes refused or answered by nothing, whatever the
    cause, from `began_ms` to `ended_ms`: each its time, its kind and its
    words. A write handed to a writer group whose writer is down is never
    told "nothing serves": every group's queue is subscribed to the internal
    topic, so the broker finds a responder, the absent group's member is
    not there to answer, and the caller is refused at its deadline (stage
    T's review, finding 7)."""
    found = []
    for p in driver_lines("/tmp/refused"):
        said = refusal(p)
        if said and began_ms <= said[1] <= ended_ms:
            found.append((said[1], said[2], said[3]))
    return sorted(found)


def refusal(p):
    """A driver's refusal line (`REFUSED <kind> <id> <unix ms refused> <ms
    taken> <words>`, driver.py) as (when its write began, when it was
    refused, its kind, its words), or None for a line that is not one. A
    line with no time taken (a driver from before PR CI 45225932982) began
    when it was refused."""
    if len(p) < 4 or not p[3].isdigit():
        return None
    at = int(p[3])
    if len(p) >= 5 and p[4].isdigit():
        return at - int(p[4]), at, p[1], " ".join(p[5:])
    return at, at, p[1], " ".join(p[4:])


# Two writes not answered this far apart or nearer are one stretch: the
# driver writes each key about once a second, and a call is bounded by its
# own deadline.
STRETCH_APART_MS = 5_000


def longest_stretch(times_ms, apart_ms=STRETCH_APART_MS):
    """The longest stretch of `times_ms`, in seconds, and how many there
    are: a stretch is a run of times each within `apart_ms` of the one
    before. One stray refusal is a stretch of no length."""
    longest, stretches, began, last = 0, 0, None, None
    for at in sorted(times_ms):
        if last is None or at - last > apart_ms:
            stretches += 1
            began = at
        last = at
        longest = max(longest, at - began)
    return longest / 1000, stretches


def rollout_served(s, what, began_ms, groups, held):
    """An upgrade's rollout, as the driver saw its stores written: measured
    per store -- the longest stretch of its writes not answered, refused or
    timed out, whatever the cause, and how many were refused "nothing
    serves" -- and claimed against the shape's own bound for a member lost
    (1048, 1056; none is new). A writer group whose one member restarts is
    not written meanwhile, and its writes are handed on and time out, never
    told "nothing serves": counted here too (stage T's review, finding 7).
    With `held`, an upgrade made at two writer groups with one member each:
    none refused "nothing serves" at all, the public topic served by one
    group's writer throughout."""
    every = not_answered(began_ms, int(time.time() * 1000))
    found = [refused for refused in every if UNSERVED in refused[2]]
    for kind, component in SERVED_BY:
        mine = [at for at, said, _ in every if said == kind]
        span, stretches = longest_stretch(mine)
        claim(s, f"rollout-{what}", component, span, TOLERANT_POD if SHAPE == "tolerant" else HA[component],
              {"not_answered": len(mine), "stretches": stretches,
               "unserved_refusals": len([at for at, said, _ in found if said == kind]),
               "writer_groups": groups, "shape": SHAPE})
    if held:
        for at, kind, words in found[:10]:
            print(f"      unserved: {kind} at {at}: {words}", flush=True)
        s.check(not found, f"{what}: no write refused \"{UNSERVED}\" through the rollout: with two writer groups "
                "a public topic is always served by one")
        figure(f"{what}: writes refused \"{UNSERVED}\" and not answered through the rollout",
               refused=len(found), not_answered=len(every))


def store_databases(store):
    """Each of a store's databases, as (d, its primary's pod): the first,
    and each the chart brings past it."""
    component = {"book": "bor"}.get(store, store)
    return [(0, primary())] + [(d, database_pod(component, d)) for c, d, _, _ in extra_databases() if c == component]


def no_lease_lapsed(s, what, since_ns, groups=1):
    """A planned rollout hands each lease over: every takeover a store
    recorded since `since_ns` was of a lease released, or of one nobody
    held (a new group's first), never of one left to lapse (fixed 66a5cf8b:
    a lapse is a write gap of up to the lease's duration an upgrade).

    Read in every database of the store (stage T's review, finding 4): a
    writer group records its takeover in its own database, so with two
    databases group 1's are in database 1, and the first database says
    nothing of them. And read to have found something: with `groups`
    writer groups rolled, at least that many takeovers are there since, one
    for each group's writer replaced, over the store's databases together."""
    any_lapsed = False
    for store in ("book", "lake"):
        lapsed, took = [], 0
        for d, pod in store_databases(store):
            said = psql(f"select coalesce(string_agg(epoch || ' ' || member || ': ' || why, '; ' order by epoch), '') "
                        f"from {store}_writer_took_over where at_ns >= {since_ns} and why like '%lapsed%'", pod=pod)
            if said:
                lapsed.append(f"database {d}: {said}")
                # Each takeover's time from the rollout's start, kept with
                # the figures: when each lapse ended (c30d-t4, item 3).
                every = psql(f"select coalesce(string_agg(epoch || ' ' || member || ' ' || "
                             f"round((at_ns - {since_ns}) / 1e9, 1) || ' ' || why, '; ' order by epoch), '') "
                             f"from {store}_writer_took_over where at_ns >= {since_ns}", pod=pod)
                figure(f"{what}: the {store}'s takeovers in database {d}, each at seconds from the rollout's start",
                       took_over=every)
            took += int(psql(f"select count(*) from {store}_writer_took_over where at_ns >= {since_ns}",
                             pod=pod) or 0)
        s.check(not lapsed, f"{what}: the {store}'s writers' leases were released, none left to lapse "
                f"({took} takeover(s) since, over {len(store_databases(store))} database(s)"
                f"{': ' + '; '.join(lapsed) if lapsed else ''})")
        any_lapsed = any_lapsed or bool(lapsed)
        s.check(took >= groups, f"{what}: the {store}'s takeovers since were read: {took} recorded over its "
                f"database(s), at least one for each of its {groups} writer group(s) rolled")
    if any_lapsed:
        # The members that stepped down were replaced by this rollout, and the
        # next replaces their replacements: what they said, kept now
        # (CI 43788516279 kept nothing of "ha groups raised"'s lapses).
        cluster_said(f"{what}: a lease lapsed")


# A partition's head a move between databases let go, in the database it
# left: its epoch (`meridian_changes::moves::POINTER_EPOCH`).
POINTER_EPOCH = -1
# Each store's heads: its table, how a row names its partition, and its head.
HEADS = {"bor": ("book_partition_head", "partition", "sequence", "partition <> 'control'"),
         "lake": ("lake_partition", "dataset", "head", "closed_at is null")}


def away_from_home(component, group=1):
    """How many of writer group `group`'s partitions the store's first
    database still holds written and not let go: each is still to be moved
    home to the group's own database, or is being moved now. None, and
    every move of the count's change is done: the first database's head of
    each is a move's pointer."""
    table, _, head, live = HEADS[component]
    return int(psql(f"select count(*) from {table} where writer_group = {group} and {head} > 0 "
                    f"and epoch <> {POINTER_EPOCH} and {live}") or 0)


def moved_home(component, group=1):
    """Whether every partition of writer group `group` is home in its own
    database, none still in the first or on its way: database 1 holds some
    of the store, and the first holds none of the group's that it has not
    let go (stage T's review, findings 2 and 8: one partition home said
    nothing of the rest, whose moves ran on)."""
    table, _, head, live = HEADS[component]
    there = int(psql(f"select count(*) from {table} where {head} > 0 and epoch <> {POINTER_EPOCH} and {live}",
                     pod=database_pod(component, 1)) or 0)
    return there > 0 and away_from_home(component, group) == 0


def group_members_ready(groups):
    """Each store's writer groups' members there and ready, and no member
    of a group past `groups` left."""
    want = 2 if SHAPE in ("ha", "scaled") else 1
    for component in ("lake", "bor"):
        for group in range(1, 3):
            live = pods(f"{component}-group-{group}")
            if group < groups and not (len(live) == want and all(ready(p) for p in live)):
                return False
            if group >= groups and (live or kubectl("get", "deployment", f"{FULL}-{component}-group-{group}",
                                                    check=False).returncode == 0):
                return False
    return True


def groups_upgraded(s, what, groups, held, *sets):
    """One upgrade of this part under the driver's load, as `meridian
    upgrade-deployment --set` makes it: applied, its migration succeeded,
    every workload rolled out as the CLI waits for it, each group's members
    ready, the driver acknowledged by both stores again with nothing
    acknowledged lost, each change log as it was and added to, the record's
    properties held; and its rollout measured (rollout_served). Says
    whether it settled."""
    print(f"T: {what}", flush=True)
    before = record_now()
    began = time.time()
    upgraded = upgrade(*sets)
    s.check(upgraded.returncode == 0, f"{what}: the upgrade applied"
            + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
    if upgraded.returncode != 0:
        return False
    migrated(s, what)
    settled = happens(lambda: not rollout_unfinished() and settled_everywhere() and group_members_ready(groups),
                      900, every=5)
    figure(f"{what}: every workload rolled out and ready after the upgrade", took_s=round(time.time() - began))
    s.check(settled, f"{what}: every workload rolled out and ready after the upgrade"
            + ("" if settled else f" ({'; '.join(rollout_unfinished()[:4])})"))
    if not settled:
        not_settled(s, what)
        return False
    s.check(acks_again(int(time.time() * 1000), seconds=300),
            f"{what}: the write driver acknowledged by the book and the lake again")
    driver_rpo_held(s, what, store_rpo(int(began * 1000)))
    after = record_now()
    kept = all(set(before[store].splitlines()) <= set(after[store].splitlines()) for store in before)
    s.check(kept, f"{what}: each store's change log as it was before, added to and nothing else")
    properties(s, what)
    rollout_served(s, what.split(":")[0].replace(" ", "-"), int(began * 1000), groups, held)
    return True


def group_counts(s):
    """The writer-group count raised and lowered online (section 15, the
    online conversions), in tolerant, under the driver's load: to two
    groups; an upgrade at two groups that restarts every member of both
    stores, through which neither store's public topics go unserved; and
    back to one, the second group's partitions written by the first
    again."""
    if not groups_upgraded(s, "groups raised: lake.writerGroups and bor.writerGroups set to 2", 2, False,
                           "lake.writerGroups=2", "bor.writerGroups=2"):
        return
    # Both groups' writers restarted by one upgrade, the count as it was:
    # what CI 43636763392 saw unserved for 2 s.
    groups_upgraded(s, "two groups rolled: every member of both stores restarted at two writer groups", 2, True,
                    f"lake.scrub.intervalDays={SCRUB_DAYS}", f"bor.scrub.intervalDays={SCRUB_DAYS}")
    # Lowered: the second group's members stop for good, its writer serving
    # on until the first group's serves at the new count.
    if not groups_upgraded(s, "groups lowered: lake.writerGroups and bor.writerGroups set back to 1", 1, True,
                           "lake.writerGroups=1", "bor.writerGroups=1"):
        return
    # What group 1 wrote is the one group's again: written since the lowered
    # count settled, by the one group left (group 1's members are gone:
    # group_members_ready). A partition's head still names group 1 -- no
    # code renames it, and a name past the count is the rendezvous owner's,
    # group 0 at one group (meridian_changes::groups, `assigned`); raised
    # again, the count gives it back -- so the check reads the writes into
    # what group 1 held, never the name (CI 43788516279: "the highest group
    # named: lake 1, book 1", 300 s, with 596 rows and 2844 entries since).
    settled_ns = time.time_ns()

    def one_group_writes():
        # The lake's half reads rows into group 1's open ranges of the
        # driver's dataset, each row by its own range (the review's L3).
        said = psql(
            "select (select count(*) from lake_partition where split_part(dataset, '/', 1) = 'drive-data:drive' "
            "and closed_at is null and writer_group >= 1), "
            "(select count(*) from lake_row r join lake_partition p "
            "on p.dataset = coalesce(nullif(r.partition, ''), r.dataset) "
            "where p.closed_at is null and p.writer_group >= 1 "
            f"and r.dataset = 'drive-data:drive' and r.recorded_at_ns >= {settled_ns}), "
            "(select count(*) from book_partition_head where partition <> 'control' and writer_group >= 1), "
            "(select count(*) from book_entry e join book_partition_head h on h.partition = e.partition "
            f"where h.writer_group >= 1 and e.committed_at_ns >= {settled_ns}), "
            "(select count(*) from book_entry where account_id like 'ACC-DRIVE-%' "
            f"and committed_at_ns >= {settled_ns}), "
            "(select count(*) from lake_row where dataset = 'drive-data:drive' "
            f"and recorded_at_ns >= {settled_ns})")
        # Nothing read is nothing shown: never a pass (the review's L3).
        return [int(n) for n in said.split("|")] if said else None

    happens(lambda: lowered_written(one_group_writes()), 300, every=5)
    said = one_group_writes()
    s.check(lowered_written(said, into_only=True), "groups lowered: what group 1 held is written by the one group "
            "left " + (f"(the driver's dataset: {said[0]} range(s) named group 1, {said[1]} rows into them since; "
                       f"the book: {said[2]} partition(s) named group 1, {said[3]} entries into them since)"
                       if said else "(the stores could not be read)"))
    s.check(bool(said) and said[5] > 0 and said[4] > 0, "groups lowered: the driver's prices and orders committed "
            "by the one group left " + (f"({said[5]} rows, {said[4]} entries since)" if said else "(not read)"))


def lowered_written(said, into_only=False):
    """Whether what group 1 held is written by the one group left, from
    `one_group_writes`' counts: each store's writes into what names group 1
    since the count settled, where anything names it; and, unless
    `into_only`, the driver's prices and entries since at all. Nothing read
    is never written."""
    if not said:
        return False
    lake_named, into_lake, book_named, into_book, entries, prices = said
    into = (lake_named == 0 or into_lake > 0) and (book_named == 0 or into_book > 0)
    return into if into_only else into and prices > 0 and entries > 0


GROUP_COMPONENTS = ("lake", "bor", "lake-group-1", "bor-group-1")


def groups_held():
    """What a writer-group count's change would move, as it stands: each
    store member's pod (its name, uid and restarts), each group's epoch,
    the lake's control epoch, the places kept in the write-ahead stream
    (their keys alone: their sequences move with the driver's load), and
    each partition's group and epoch."""
    members = {p["metadata"]["name"]: (p["metadata"]["uid"], restarts(p["metadata"]["name"]))
               for component in GROUP_COMPONENTS for p in pods(component)}
    rows = {
        "each lake group's epoch": psql("select writer_group, epoch from lake_writer_group order by 1"),
        "each book group's epoch": psql("select writer_group, epoch from book_writer_group order by 1"),
        "the lake's control epoch": psql("select epoch from lake_change_head where partition = 'control'"),
        "the places in the write-ahead stream": psql("select stream from lake_write_ahead order by 1"),
    }
    partitions = {
        "lake": psql("select dataset, writer_group, epoch from lake_partition"),
        "bor": psql("select partition, writer_group, epoch from book_partition_head where partition <> 'control'"),
    }
    return members, rows, {store: set(said.splitlines()) for store, said in partitions.items()}


def groups_lowering_refused(s, what):
    """The writer-group count lowered from two to one in ha, under the
    driver's load, each store's alone: each refused by the chart (PO item
    111, option B; item 103's step), saying why and naming its own store --
    Helm stops at the first refusal, so one upgrade lowering both would
    prove one store's (review c5s, L4) -- and nothing changes: no revision,
    and no store member's pod, epoch, partition's group or place in the
    write-ahead stream moves, through the upgrades and a while after."""
    print(f"T: {what}", flush=True)
    before_revision = revision()
    members, rows, partitions = groups_held()
    for store in ("lake", "bor"):
        upgraded = upgrade(f"{store}.writerGroups=1")
        said = upgraded.stderr.decode(errors="replace")
        s.check(upgraded.returncode != 0 and f"{store}.writerGroups is 1" in said and "can't lower them yet" in said,
                f"{what}: {store}.writerGroups lowered alone, the upgrade refused, saying why"
                + (" (it applied)" if upgraded.returncode == 0 else f": {said[-400:]}"))
    time.sleep(30)
    s.check(revision() == before_revision, f"{what}: the release's revision unchanged")
    members_after, rows_after, partitions_after = groups_held()
    s.check(members_after == members, f"{what}: no store member's pod moved or restarted"
            + ("" if members_after == members else f" (before {sorted(members)}, after {sorted(members_after)})"))
    for name, said_before in rows.items():
        s.check(rows_after[name] == said_before, f"{what}: {name} unchanged"
                + ("" if rows_after[name] == said_before else f" ({said_before!r} -> {rows_after[name]!r})"))
    for store, before in partitions.items():
        moved = sorted(before - partitions_after[store])
        s.check(not moved, f"{what}: no {store} partition's group or epoch moved"
                + ("" if not moved else f" ({len(moved)}, first {moved[:3]})"))


def two_groups_two_databases(s):
    """In ha, last before the restore: two writer groups, a lowering to one
    refused with nothing changed (PO item 103, as ruled by item 111), then a
    database added to each store (section 15, the online conversions),
    under the driver's load; each rollout measured, and every lease handed
    over released. The restore to a time is then across each store's two
    databases."""
    since_ns = time.time_ns()
    if not groups_upgraded(s, "ha groups raised: two writer groups in ha", 2, False,
                           "lake.writerGroups=2", "bor.writerGroups=2"):
        return
    no_lease_lapsed(s, "ha groups raised", since_ns)
    # Lowered in ha (PO item 103, as ruled by item 111, option B): refused.
    groups_lowering_refused(s, "ha groups lowered")
    since_ns = time.time_ns()
    # Where each acknowledged write is before the databases are added, so
    # before any move home: the first database (moved_and_kept, below).
    sampled = where_now()
    if not groups_upgraded(s, "ha databases added: a database added to each store in ha", 2, False,
                           "lake.databases=2", "bor.databases=2"):
        return
    for component in ("lake", "bor"):
        rolled = run("kubectl", "--namespace", NAMESPACE, "rollout", "status",
                     f"statefulset/{FULL}-{component}-database-1", "--timeout=420s", check=False)
        s.check(rolled.returncode == 0, f"ha databases added: {component}'s database 1 brought and ready")
        # In its full shape at once, as the first is in ha (section 6.4; CI
        # 43788516279: one member, alone, waiting for a second upgrade).
        shape = json.loads(out("get", "statefulset", f"{FULL}-{component}-database-1", "-o", "json"))
        wanted, ready_now = shape["spec"].get("replicas", 0), shape.get("status", {}).get("readyReplicas", 0)
        s.check(wanted == database_members() and ready_now == wanted,
                f"ha databases added: {component}'s database 1 runs the first's shape, its standbys with it "
                f"({ready_now} of {wanted} ready, the first's {database_members()})")
    # Read once each store's second database is there: group 1's takeovers
    # are recorded in it (finding 4).
    no_lease_lapsed(s, "ha databases added", since_ns, groups=2)
    # Every one of group 1's partitions moved home to its database by the
    # count's change, once every member seen runs the new count: none still
    # in the first database or on its way, so the restore's time cuts no
    # move in two (finding 8).
    in_one = happens(lambda: moved_home("bor"), 600, every=5)
    s.check(in_one, "ha databases added: every one of group 1's book partitions moved to database 1 by the "
            f"count's change ({away_from_home('bor')} still in the first)")
    lake_in_one = happens(lambda: moved_home("lake"), 600, every=5)
    s.check(lake_in_one, "ha databases added: every one of group 1's lake datasets moved to database 1 by the "
            f"count's change ({away_from_home('lake')} still in the first)")
    s.check(acks_again(int(time.time() * 1000), seconds=300),
            "ha databases added: the write driver acknowledged again once the partitions were home")
    moved_and_kept(s, "ha databases added", sampled)


# ── reconfigure's archive and its last act: a restore to a time in ha ─────

# Backups first on the node (backup.path), as the preflight's own case: a
# directory on member 0's node. The WAL archive then on a claim of the
# administrator's own (backup.existingClaim), as ha needs -- a node
# directory is refused with standbys, the archive split across nodes: here
# a directory every node mounts (E2E_SHARED_PATH, the Makefile's
# E2E_K3D_SHARED), as NFS or a cloud's file store would be. No backup is
# sealed in this run (its CronJob never runs), so the recipient is never
# used: a placeholder.
RESTORE_PATH = "/var/lib/meridian-reconfigure-e2e"
RESTORE_CLAIM = "reconfigure-archive"
SHARED_PATH = os.environ.get("E2E_SHARED_PATH", "")
RESTORE_RECIPIENT = "age1e2ereconfigurerestoretoatimenokeyisused"
POINT = "e2e_point_in_time"
# The act's own limit on the database's start: a replay of this run's
# archive takes seconds, and one that does not end fails in five minutes,
# not the hour a deployment's archive may need.
RESTORE_START_SECONDS = "300"


def extra_databases():
    """Each database the chart brings past the first for a store's writer
    groups (step 3d; section 6.4), as (store, d, its StatefulSet's name,
    its archive's stanza), in order."""
    found = []
    for component in ("bor", "lake"):
        for d in range(1, 3):
            name = f"{FULL}-{component}-database-{d}"
            if kubectl("get", "statefulset", name, check=False).returncode == 0:
                found.append((component, d, name, f"{component}-database-{d}"))
    return found


def stopped_everything():
    """Every component and every database stopped, as `meridian restore
    --to-time` stops them: their counts, to start them again."""
    running = {d["metadata"]["name"]: d["spec"].get("replicas", 1) for d in json.loads(out(
        "get", "deployments", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]}
    for name in running:
        kubectl("scale", "deployment", name, "--replicas=0")
    databases = {"database"} | {f"{component}-database-{d}" for component, d, _, _ in extra_databases()}
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=0")
    for _, _, name, _ in extra_databases():
        kubectl("scale", "statefulset", name, "--replicas=0")
    # Their pods gone: each Deployment's (a ReplicaSet's, by its template
    # hash) and each database's members.
    wait_for("every component and every database to stop", lambda: not [
        p for p in json.loads(out("get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]
        if "pod-template-hash" in p["metadata"].get("labels", {})
        or p["metadata"].get("labels", {}).get("meridian.dev/component") in databases], seconds=300)
    return running


def offline_act(act, database=None, **env):
    """A Job from a database's offline CronJob with the act named, as the
    CLI makes it (meridian-cli, offline.rs), run to its end: whether it
    succeeded, and what it said. `database` names the StatefulSet of one
    past the first; the first's by default."""
    database = database or f"{FULL}-database"
    cron = json.loads(out("get", "cronjob", f"{database}-offline", "-o", "json"))
    template = cron["spec"]["jobTemplate"]
    name = f"{database}-{act}"[:63].rstrip("-")
    job = {"apiVersion": "batch/v1", "kind": "Job",
           "metadata": {"name": name, "namespace": NAMESPACE, "labels": template["metadata"].get("labels", {})},
           "spec": template["spec"]}
    said = {"MERIDIAN_DATABASE_ACT": act, **env}
    container = job["spec"]["template"]["spec"]["containers"][0]
    container["env"] = [e for e in container.get("env", []) if e["name"] not in said] + [
        {"name": k, "value": v} for k, v in said.items()]
    kubectl("delete", "job", name, "--ignore-not-found", "--wait")
    kubectl("create", "-f", "-", input=json.dumps(job).encode())
    wait_for(f"the {act} Job to finish", lambda: any(
        c["type"] in ("Complete", "Failed") and c["status"] == "True"
        for c in json.loads(out("get", "job", name, "-o", "json"))["status"].get("conditions", [])), seconds=900)
    status = json.loads(out("get", "job", name, "-o", "json"))["status"]
    log = kubectl("logs", f"job/{name}", "--tail=-1", check=False).stdout.decode(errors="replace")
    for line in log.splitlines():
        print(f"    | {line}", flush=True)
    return bool(status.get("succeeded")), log


def archived_in_tolerant(s):
    """In tolerant, before the first move: backups turned on by an upgrade,
    its preflight passing on the backup volume the same upgrade makes (step
    3b part 6); then the WAL archive on, kept on a claim every member
    mounts; and a full base backup taken into it, here, alone. The
    configuration that base backup holds is tolerant's -- Postgres's own 100
    connections, the chart's count given on its command line -- and the WAL
    after it holds ha's raise, which a hot standby's recovery paused at for
    good (step 3b part 7; the CLI's rehearsal r2, step 8)."""
    print("R: backups and the WAL archive turned on in tolerant, and a base backup taken there", flush=True)
    began = time.time()
    with HookLog("preflight") as preflight:
        upgraded = upgrade(f"backup.recipient={RESTORE_RECIPIENT}", f"backup.path={RESTORE_PATH}")
    print(f"R: the preflight said:\n{preflight.said or '(nothing read)'}", flush=True)
    s.check(upgraded.returncode == 0, "restore: backups turned on by an upgrade, its preflight passing first"
            + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
    if upgraded.returncode != 0:
        database_said(s, "backups on")
        raise SystemExit("backups could not be turned on")
    migrated(s, "backups on")
    member = f"{FULL}-database-0"
    if not SHARED_PATH:
        raise SystemExit("the archive in ha needs a directory every node mounts: "
                         "make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-reconfigure makes one")
    volume = f"{NAMESPACE}-{RESTORE_CLAIM}"
    kubectl("delete", "pv", volume, "--ignore-not-found", "--wait")
    for made in (
        {"apiVersion": "v1", "kind": "PersistentVolume", "metadata": {"name": volume},
         "spec": {"capacity": {"storage": "1Gi"}, "accessModes": ["ReadWriteMany"],
                  "persistentVolumeReclaimPolicy": "Retain", "storageClassName": "",
                  "claimRef": {"namespace": NAMESPACE, "name": RESTORE_CLAIM},
                  "hostPath": {"path": f"{SHARED_PATH}/{NAMESPACE}", "type": "DirectoryOrCreate"}}},
        {"apiVersion": "v1", "kind": "PersistentVolumeClaim",
         "metadata": {"name": RESTORE_CLAIM, "namespace": NAMESPACE},
         "spec": {"accessModes": ["ReadWriteMany"], "storageClassName": "", "volumeName": volume,
                  "resources": {"requests": {"storage": "1Gi"}}}},
    ):
        kubectl("apply", "-f", "-", input=json.dumps(made).encode())
    upgraded = upgrade("backup.path=", f"backup.existingClaim={RESTORE_CLAIM}", "backup.walArchive.enabled=true")
    s.check(upgraded.returncode == 0, "restore: the WAL archive turned on by an upgrade, on a claim every member mounts"
            + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
    if upgraded.returncode != 0:
        database_said(s, "the archive on")
        raise SystemExit("the WAL archive could not be turned on")
    migrated(s, "the archive on")
    settled = happens(settled_everywhere, 900, every=5)
    s.check(settled, "restore: every member ready with the archive on after the upgrades")
    figure("restore: every member ready with the archive on", took_s=round(time.time() - began))
    if not settled:
        not_settled(s, "the archive on")
        raise SystemExit("the deployment did not settle with the archive on")
    upgrade_rolled_out(s, "restore: the archive on")
    s.check(not under_patroni(), "restore: the base backup is taken from the database alone, as tolerant runs it")

    def archive_said(words):
        return words in kubectl("logs", member, "-c", "archive", "--tail=-1",
                                check=False).stdout.decode(errors="replace")
    wait_for("the archive's stanza", lambda: archive_said("the stanza holds this database"), seconds=180)
    # The base backup the backup Job's archive step takes through the
    # primary's pgBackRest server, taken here in the archive's own container
    # beside it, into the same archive: this run loads no backup image.
    done = kubectl("exec", member, "-c", "archive", "--", "su-exec", "postgres",
                   "pgbackrest", "--stanza=meridian", "backup", "--type=full", check=False)
    s.check(done.returncode == 0, "restore: a full base backup taken into the archive, in tolerant"
            + ("" if done.returncode == 0 else f": {done.stdout.decode(errors='replace')[-300:]}"))
    if done.returncode != 0:
        raise SystemExit("no base backup")


def restored_to_a_time(s):
    """`meridian restore --to-time` in ha, under Patroni with internal TLS
    on, to a time after two moves past the base backup taken in tolerant:
    the WAL replayed holds Patroni's raise of max_connections past the
    base backup's own, where the offline Job's start, a hot standby,
    paused for good (step 3b part 7). Three rows either side of a time;
    everything stopped as the CLI stops it; a time past the archive refused
    in the act's own words, naming its first and last times; the
    restore run; the standbys' volumes deleted and Patroni's record of the
    old primary cleared; everything started; and the rows as they stood at
    the time."""
    print("R: a restore to a time in ha from a base backup taken in tolerant, under Patroni with internal TLS on",
          flush=True)
    s.check(SHAPE == "ha" and under_patroni(), f"restore: the database under Patroni in ha ({SHAPE})")
    members = database_members()
    # Each store's database past the first (stage T; section 15: a restore
    # across two databases to a time), restored to the same time: its own
    # stanza of the archive, its own offline Job, its own rows either side.
    extras = extra_databases()
    extra_members = {name: int(out("get", "statefulset", name, "-o", "jsonpath={.spec.replicas}") or 0)
                     for _, _, name, _ in extras}
    for component, d, name, stanza in extras:
        member = database_pod(component, d)
        wait_for(f"{name}'s stanza of the archive", lambda m=member: "the stanza holds this database" in kubectl(
            "logs", m, "-c", "archive", "--tail=-1", check=False).stdout.decode(errors="replace"), seconds=180)
        done = kubectl("exec", member, "-c", "archive", "--", "su-exec", "postgres",
                       "pgbackrest", f"--stanza={stanza}", "backup", "--type=full", check=False)
        s.check(done.returncode == 0, f"restore: a full base backup of {name} taken into its stanza, {stanza}"
                + ("" if done.returncode == 0 else f": {done.stdout.decode(errors='replace')[-300:]}"))
        if done.returncode != 0:
            return
    points = [None] + [database_pod(component, d) for component, d, _, _ in extras]
    for pod in points:
        psql(f"create database {POINT}", database="postgres", pod=pod)
        psql("create table point (n int primary key, said text not null)", database=POINT, pod=pod)
        for n in (1, 2, 3):
            psql(f"insert into point values ({n}, 'before')", database=POINT, pod=pod)
    # What the book held committed before the time, in each of its
    # databases: read again after the restore.
    time.sleep(1.5)
    to = psql("select to_char(clock_timestamp() at time zone 'UTC', 'YYYY-MM-DD HH24:MI:SS.US') || '+00'",
              database="postgres")
    time.sleep(1.5)
    for pod in points:
        for n in (4, 5, 6):
            psql(f"insert into point values ({n}, 'after')", database=POINT, pod=pod)
    past = psql("select to_char((clock_timestamp() + interval '1 day') at time zone 'UTC', "
                "'YYYY-MM-DD HH24:MI:SS.US') || '+00'", database="postgres")
    for pod in points:
        wal = psql("select pg_walfile_name(pg_switch_wal())", database="postgres", pod=pod)
        wait_for("the last segment to be archived", lambda w=wal, p=pod: psql(
            "select last_archived_wal from pg_stat_archiver", database="postgres", pod=p) >= w, seconds=180)
    running = stopped_everything()
    # Past the archive's end: the act refuses it in its own words, naming
    # the archive's first and last times, before the volume is touched
    # (step 3c: it reads the archive's last commit first).
    ok, log = offline_act("restore-to-time", MERIDIAN_RESTORE_TO_TIME=past,
                          MERIDIAN_DATABASE_START_SECONDS=RESTORE_START_SECONDS)
    s.check(not ok and "is outside the archive, which holds" in log,
            f"restore: a time past the archive, {past}, refused as outside it, naming its first and last times")
    ok, log = offline_act("restore-to-time", MERIDIAN_RESTORE_TO_TIME=to,
                          MERIDIAN_DATABASE_START_SECONDS=RESTORE_START_SECONDS)
    s.check(ok and "promoted at" in log, f"restore: the offline Job restored the database to {to}, replaying "
            "ha's raise of its settings past the base backup's, and starting it without the certificates "
            "Patroni's configuration names")
    if not ok:
        return
    for _, _, name, _ in extras:
        ok, log = offline_act("restore-to-time", database=name, MERIDIAN_RESTORE_TO_TIME=to,
                              MERIDIAN_DATABASE_START_SECONDS=RESTORE_START_SECONDS)
        s.check(ok and "promoted at" in log, f"restore: {name} restored to the same time, {to}, from its own stanza")
        if not ok:
            return
    # The standbys' volumes, which Patroni makes again from member 0, and
    # Patroni's record of the old primary, which member 0's restored
    # position is behind (meridian-cli, offline.rs standbys_to_remake).
    kubectl("delete", "pvc", "--ignore-not-found", *[f"data-{FULL}-database-{n}" for n in range(1, members)])
    scope = f"{FULL}-database"[:63].rstrip("-")
    kubectl("delete", "configmap", "--ignore-not-found",
            *[f"{scope}-{kind}" for kind in ("leader", "sync", "failover")])
    kubectl("scale", "statefulset", f"{FULL}-database", f"--replicas={members}")
    for _, _, name, _ in extras:
        kubectl("delete", "pvc", "--ignore-not-found", *[f"data-{name}-{n}" for n in range(1, extra_members[name])])
        kubectl("delete", "configmap", "--ignore-not-found",
                *[f"{name[:63].rstrip('-')}-{kind}" for kind in ("leader", "sync", "failover")])
        kubectl("scale", "statefulset", name, f"--replicas={extra_members[name]}")
    wait_for("the restored database's primary", lambda: psql(
        "select not pg_is_in_recovery()", database="postgres") == "t", seconds=300)
    for component, d, name, _ in extras:
        wait_for(f"{name}'s restored primary", lambda c=component, n=d: kubectl(
            "exec", database_pod(c, n), "-c", "postgres", "--", "psql", "-X", "-q", "-At", "-U", "postgres",
            "-d", "postgres", "-c", "select not pg_is_in_recovery()", check=False).stdout.decode().strip() == "t",
            seconds=300)
    for name, replicas in running.items():
        kubectl("scale", "deployment", name, f"--replicas={replicas}")
    settled = happens(settled_everywhere, 900, every=5)
    s.check(settled, f"restore: every component ready again, and the database's {members} members")
    if not settled:
        not_settled(s, "restore")
    rows = psql("select string_agg(n || ':' || said, ',' order by n) from point", database=POINT)
    s.check(rows == "1:before,2:before,3:before", f"restore: the rows as they stood at the time ({rows})")
    for component, d, name, _ in extras:
        rows = psql("select string_agg(n || ':' || said, ',' order by n) from point", database=POINT,
                    pod=database_pod(component, d))
        s.check(rows == "1:before,2:before,3:before",
                f"restore: {name}'s rows as they stood at the same time ({rows})")
    if extras:
        restored_across(s, to)
    timeline = psql("select timeline_id from pg_control_checkpoint()", database="postgres")
    s.check(timeline not in ("", "1"), f"restore: on a timeline of its own ({timeline})")
    internal_tls(s)


# Each store's change log: its table, how a row names its partition, its
# first and last sequence, and what says which change a row is.
ACROSS = (("bor", "book_entry", "partition", "first_sequence", "last_sequence", "entry_id"),
          ("lake", "lake_row", "dataset", "sequence", "sequence", "row_key || ':' || version"))


def stretches_held(component, d):
    """Each partition's change log as `component`'s database `d` holds it:
    its unbroken stretches of sequences, as {partition: [(first, last)]}.
    The same change held twice is one; two changes over one sequence are
    two stretches that overlap."""
    _, table, partition, first, last, _ = next(row for row in ACROSS if row[0] == component)
    said = psql(
        f"with d as (select distinct {partition} as p, {first} as f, {last} as l from {table}), "
        f"g as (select p, f, l, f - coalesce(sum(l - f + 1) over (partition by p order by f, l "
        f"rows between unbounded preceding and 1 preceding), 0) as island from d) "
        f"select p || '|' || min(f) || '|' || max(l) from g group by p, island order by 1",
        pod=database_pod(component, d))
    held = {}
    for line in said.splitlines():
        name, first_at, last_at = line.rsplit("|", 2)
        held.setdefault(name, []).append((int(first_at), int(last_at)))
    return {name: sorted(stretches) for name, stretches in held.items()}


def let_go_at(component, d):
    """Each partition `component`'s database `d` let go to another, and the
    sequence it let it go at: its head there a move's pointer."""
    table, partition, head, _ = HEADS[component]
    said = psql(f"select {partition} || '|' || {head} from {table} where epoch = {POINTER_EPOCH}",
                pod=database_pod(component, d))
    return {line.rsplit("|", 1)[0]: int(line.rsplit("|", 1)[1]) for line in said.splitlines()}


def whole_across(held, let_go):
    """What is wrong with one partition's change log over a store's
    databases together. `held` is each database's unbroken stretches of it
    ({d: [(first, last)]}), `let_go` each database's sequence it let it go
    at ({d: sequence}). Whole: every database's stretches laid together
    leave no sequence out between the first and the last, and reach every
    sequence a database let it go at -- where one says it moved at N, another
    holds it to N. Says what is wrong, and each stretch two databases both
    hold, for their changes to be compared."""
    wrong, shared = [], []
    every = sorted((first, last, d) for d, stretches in held.items() for first, last in stretches)
    for d, stretches in held.items():
        for (_, last), (first, _) in zip(stretches, stretches[1:]):
            wrong.append(f"database {d} holds a sequence twice ({first} at or before {last})" if first <= last
                         else f"database {d} is missing {last + 1} to {first - 1}")
    reached = None
    for first, last, d in every:
        if reached is not None and first > reached + 1:
            wrong.append(f"no database holds {reached + 1} to {first - 1}")
        reached = last if reached is None else max(reached, last)
    for d, at in let_go.items():
        if at > 0 and (reached is None or reached < at):
            wrong.append(f"database {d} let it go at {at} and no database holds it past {reached or 0}")
    for i, (first, last, d) in enumerate(every):
        for other_first, other_last, other in every[i + 1:]:
            if other != d and other_first <= last:
                shared.append((d, other, max(first, other_first), min(last, other_last)))
    return wrong, shared


def restored_across(s, to):
    """After a restore across each store's two databases to a time (section
    15; section 6.4). Each database was taken to that time by its own
    archive, so a booking whose legs are in two partitions may have kept
    one and lost the other: the book's sweep completes or reverses each,
    each as its own record. That is the crate's own proof (booking_tests,
    a_restore_across_databases_completes_what_survives_and_reverses_what_
    lost_its_parent): this run's driver records orders, which cross no
    partition, and is stopped by now.

    Held here, over the two databases together (stage T's review, finding
    8: each database whole on its own says nothing of a partition one let
    go and the other never opened, or of one sequence holding two changes):
    each partition's change log is whole across the pair -- no sequence
    left out between its first and its last, and held at least to where a
    database let it go -- and where both hold a stretch of it, they hold
    the same changes. Then both stores answering again over both."""
    for component, table, partition, first, last, change in ACROSS:
        held = {d: stretches_held(component, d) for d in (0, 1)}
        let_go = {d: let_go_at(component, d) for d in (0, 1)}
        broken, compared = [], 0
        for name in sorted(set(held[0]) | set(held[1]) | set(let_go[0]) | set(let_go[1])):
            wrong, shared = whole_across({d: held[d].get(name, []) for d in (0, 1)},
                                         {d: let_go[d][name] for d in (0, 1) if name in let_go[d]})
            broken += [f"{name}: {said}" for said in wrong]
            quoted = name.replace("'", "''")
            for d, other, low, high in shared:
                compared += 1
                digests = [psql(f"select count(*) || ' ' || md5(coalesce(string_agg(c, ',' order by f, c), '')) "
                                f"from (select distinct {first} as f, {last} as l, {change} as c "
                                f"from {table} where {partition} = '{quoted}' and {first} between {low} and {high}) "
                                f"as both_hold", pod=database_pod(component, which)) for which in (d, other)]
                if digests[0] != digests[1]:
                    broken.append(f"{name}: sequences {low} to {high} hold other changes in database {d} "
                                  f"({digests[0].split()[0]}) than in database {other} ({digests[1].split()[0]})")
        for said in broken[:10]:
            print(f"      across databases: {component}: {said}", flush=True)
        s.check(not broken, f"restore across databases: each of {component}'s partitions' change logs whole over "
                f"its two databases together at {to} ({len(broken)} broken; {len(held[0])} and {len(held[1])} "
                f"partition(s) held, {sum(len(x) for x in let_go.values())} let go, {compared} stretch(es) "
                "held by both and compared)")
        s.check(bool(held[1]), f"restore across databases: {component}'s second database holds some of it, "
                "so the pair was read")
    # Each store serving again over both databases, written by both groups.
    since_ns = time.time_ns()
    answering_again = happens(lambda: all(answering(c) is not None for c in ("bor", "lake")), 300, every=3)
    s.check(answering_again, "restore across databases: the book and the lake answer again over both databases")
    took = psql(f"select count(*) from book_writer_took_over where at_ns >= {since_ns - 600_000_000_000}")
    s.note(f"restore across databases: the book's takeovers recorded around the restore: {took}")


# ── The record's properties, held after every class (ruling 1105) ─────────

# Each store's change log: its table, how a row names its partition and its
# sequences, and the store's takeovers.
CHANGE_LOGS = [
    ("config", "config_change", "partition", "sequence", "sequence", "config_writer_took_over"),
    ("street", "street_change", "partition", "sequence", "sequence", "street_writer_took_over"),
    ("instrument", "instrument_change", "partition", "sequence", "sequence", "instrument_writer_took_over"),
    ("lake", "lake_row", "dataset", "sequence", "sequence", "lake_writer_took_over"),
    ("lake control", "lake_control_change", "'control'", "sequence", "sequence", "lake_writer_took_over"),
    ("book", "book_entry", "partition", "first_sequence", "last_sequence", "book_writer_took_over"),
]


def properties(s, after):
    """The record's own properties, read from each store's change log and
    its takeovers: no sequence missing or taken twice in a partition (a gap
    or two writers at one sequence), no epoch going back along the sequence
    (a fenced writer's commit kept after its successor's), every epoch
    above the first raised by a recorded takeover, and the takeovers one
    per epoch with none skipped."""
    broken = []
    for store, table, partition, first, last, took in CHANGE_LOGS:
        if table in CORRUPTED:
            # Its only copy's page flipped on purpose (bit-rot, tolerant):
            # read, it fails its checksum, as that class proved.
            continue
        said = psql(
            f"with d as (select distinct {partition} as p, {first} as f, {last} as l, epoch from {table}), "
            f"c as (select p, f, l, epoch, lag(l) over w as prev_l, lag(epoch) over w as prev_epoch "
            f"from d window w as (partition by p order by f, l, epoch)) "
            f"select count(*), "
            f"count(*) filter (where prev_l is not null and f <> prev_l + 1), "
            f"count(*) filter (where prev_epoch is not null and epoch < prev_epoch), "
            f"count(*) filter (where epoch > 1 and epoch not in (select epoch from {took})) "
            f"from c")
        rows, gaps, back, unrecorded = (int(x) for x in said.split("|"))
        if gaps:
            broken.append(f"{store}: {gaps} sequence(s) missing or taken twice")
        if back:
            broken.append(f"{store}: {back} commit(s) at an epoch below one before it")
        if unrecorded:
            broken.append(f"{store}: {unrecorded} commit(s) at an epoch no takeover recorded")
    for store in ("book", "lake", "config", "street", "instrument"):
        said = psql(f"select count(*), coalesce(max(epoch) - min(epoch) + 1, 0) from {store}_writer_took_over")
        recorded, spanned = (int(x) for x in said.split("|"))
        if recorded != spanned:
            broken.append(f"{store}: {recorded} takeovers recorded over {spanned} epochs")
    s.check(not broken, f"properties after {after}: each change log whole and in epoch order, "
            f"each epoch one recorded takeover{': ' + '; '.join(broken) if broken else ''}")


# ── Memory and isolation (stage M; the resilience spec's section 10a) ───────
#
# `memory-pressure`: the flood instance reads its whole dataset with many
# readers at once, past the lake's budget; `noisy-plugin`: it asks one page
# at a time, as fast as it can, past its own share at its sidecar. Either
# way, for FLOOD_SECONDS: refusals typed for load, each with a retry-after,
# to the flooder alone (REFUSAL_REASON_OVERLOADED, REFUSAL_REASON_RATE_LIMITED);
# every other instance writing within its band all the while; no pod
# restarted and none OOMKilled; and the flooder answered again as the
# flood ends (the plan's Track 7 table).

FLOOD = "drive-flood"
FLOOD_DATASET = f"{FLOOD}:drive"
# Its dataset asked of it, licensed here as served, not kept: what a lake
# reader hands to its writer (contract v30, stage B; reader_hands_on).
FLOOD_UNKEPT = f"{FLOOD}:unkept"
FLOOD_SECONDS = 60
# Every other instance within its band during a flood: the data driver
# records a price every half second and the oms driver an order about every
# quarter to whole second, so each acknowledges at least this many in a
# minute's flood, about a quarter of what each does unflooded.
FLOOD_BAND = {"price": 25, "order": 15}
# ── A standing job's coordinator killed (contract v30, W12.4) ───────────────
#
# Question 13: a standing job is held by one coordinator, by rendezvous
# hashing of its key over the live ones. The coordinator holding the
# standing driver's job killed outright, the job is back -- registered
# again by the SDK with a live coordinator and its answer read -- within
# 1048's 30 s for a component: a lease's renewal plus a first load.

STANDING_BACK = 30


def standing_lines():
    """The standing driver's job IDs, each with when it was held (ms)."""
    said = []
    for line in driver_file("drive-standing", "/tmp/standing.log").splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[0] == "HELD" and parts[2].isdigit():
            said.append((parts[1], int(parts[2])))
    return said


def standing_held():
    """The standing driver's job IDs, in the order it was held under each."""
    return [job for job, _ in standing_lines()]


def standing_settled(seconds=45):
    """Its job held for `seconds` without moving: two renewals at least, so
    the coordinators' set has settled after whatever came before."""
    lines = standing_lines()
    return bool(lines) and time.time() * 1000 - lines[-1][1] >= seconds * 1000


def standing_holder(job_id):
    """The analytics coordinator pod that holds `job_id`, by its line."""
    for pod in pods("analytics-coordinator"):
        name = pod["metadata"]["name"]
        said = kubectl("logs", name, "-c", "analytics-coordinator", check=False)
        for line in said.stdout.decode(errors="replace").splitlines():
            if job_id in line and "a standing job held" in line:
                return name
    return ""


def coordinators_said():
    """Each coordinator's restarts and its last standing lines, for a check
    that found none holding the job."""
    said = []
    for pod in pods("analytics-coordinator"):
        name = pod["metadata"]["name"]
        logs = kubectl("logs", name, "-c", "analytics-coordinator", check=False).stdout.decode(
            errors="replace").splitlines()
        standing = [line for line in logs if "standing" in line][-3:]
        said.append(f"{name} restarts={restarts(name)} lines={len(logs)} standing={standing}")
    return "; ".join(said)


def standing_coordinator(s):
    print("F: standing-coordinator: the coordinator holding a standing job killed", flush=True)
    settled = happens(standing_settled, 300, every=3)
    s.check(settled, f"standing-coordinator: the driver holds a standing job, settled ({standing_held()[-1:]})")
    if not settled:
        return
    holder, job_id = "", ""

    def found():
        nonlocal holder, job_id
        job_id = standing_held()[-1]
        holder = standing_holder(job_id)
        return bool(holder)
    located = happens(found, 60, every=2)
    s.check(located, f"standing-coordinator: {job_id} held by {holder or 'no coordinator found: ' + coordinators_said()}")
    if not located:
        return
    began = time.time()
    kubectl("delete", "pod", holder, "--grace-period=0", "--force", "--wait=false", check=False)
    back = happens(lambda: (standing_held()[-1:] or [job_id])[0] != job_id, STANDING_BACK * 2, every=0.5)
    seconds = time.time() - began if back else STANDING_BACK * 2
    again = standing_held()[-1]
    s.check(back, f"standing-coordinator: the job registered again as {again}")
    if back:
        # The job it settles on, held by a coordinator still running, whose
        # line may follow the driver's by a moment.
        settled = happens(standing_settled, 120, every=2)
        now = standing_held()[-1]
        live = happens(lambda: standing_holder(standing_held()[-1]) not in ("", holder), 30, every=2)
        s.check(settled and live,
                f"standing-coordinator: {now} held by a live coordinator "
                f"({standing_holder(now) or 'none found: ' + coordinators_said()})")
    claim(s, "standing-coordinator", "analytics", seconds, STANDING_BACK,
          {"job_before": job_id, "job_after": again, "killed": holder})


# How soon the flooder is answered again once it stops (the tolerant pod
# bound; nothing restarts, so it is the refusals' retry-after that counts).
FLOOD_RECOVERED = 30
FLOOD_ENTITLED = []


def varint(n):
    said = bytearray()
    while True:
        byte = n & 0x7F
        n >>= 7
        if n:
            said.append(byte | 0x80)
        else:
            said.append(byte)
            return bytes(said)


def text_field(number, value):
    data = value.encode()
    return varint(number << 3 | 2) + varint(len(data)) + data


def flood_entitled(s):
    """The flood instance entitled to its own datasets, as a deployment admin
    entitles a reader (W10.1), and the one asked of it licensed as served,
    not kept (W10.9): its DatasetEntitlements and that DatasetLicence
    recorded where the conductor keeps them, which the conductor publishes
    whole within a minute. Once a run."""
    if FLOOD_ENTITLED:
        return
    now = time.time_ns()
    for dataset, why in ((FLOOD_DATASET, "the memory and isolation classes read it"),
                         (FLOOD_UNKEPT, "a lake reader hands its read to the writer")):
        entitlement = (text_field(1, dataset) + text_field(2, FLOOD) + varint(3 << 3) + varint(1)
                       + text_field(5, ADMIN) + varint(6 << 3) + varint(now) + text_field(9, why))
        psql("INSERT INTO config_dataset_entitlement_change (dataset, instance, entitlement, changed_at_ns) "
             f"VALUES ('{dataset}', '{FLOOD}', decode('{entitlement.hex()}', 'hex'), {now})")
    # Served, not kept: `kept` (2) false, so left out.
    licence = (text_field(1, FLOOD_UNKEPT) + text_field(8, ADMIN) + varint(9 << 3) + varint(now)
               + text_field(12, "served when asked, not kept: the reader-hands-on step"))
    psql("INSERT INTO config_dataset_licence_change (dataset, licence, changed_at_ns) "
         f"VALUES ('{FLOOD_UNKEPT}', decode('{licence.hex()}', 'hex'), {now})")
    seeded = happens(lambda: "SEEDED" in driver_file(FLOOD, "/tmp/seeded"), 300, every=5)
    s.check(seeded, f"the flood instance's {FLOOD_DATASET} recorded")
    # The conductor publishes the data configuration every minute.
    time.sleep(75)
    FLOOD_ENTITLED.append(True)


def flood_set(mode):
    pod = driver_pod(FLOOD)
    kubectl("exec", pod, "-c", "plugin", "--", "sh", "-c", f"echo {mode} > /tmp/flood")


def flood_heard(mode, ended=False):
    """The flood's last count for `mode`, or the one it gave at its end."""
    said = {}
    for line in driver_file(FLOOD, "/tmp/flood.log").splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0] == "FLOOD" and parts[1] == mode and (not ended or parts[-1] == "END"):
            said = dict(p.split("=", 1) for p in parts[2:] if "=" in p)
    return {k: int(v) for k, v in said.items() if v.isdigit()}


def restarted_pods():
    """Every pod's restarts, and each container's last ending reason."""
    pods_now = json.loads(out("get", "pods", "-o", "json"))["items"]
    said = {}
    for pod in pods_now:
        for container in pod["status"].get("containerStatuses", []):
            reason = (container.get("lastState", {}).get("terminated") or {}).get("reason", "")
            said[f"{pod['metadata']['name']}/{container['name']}"] = (container.get("restartCount", 0), reason)
    return said


def flood(s, failure, mode, reason, component):
    print(f"F: {failure}: {FLOOD} floods {component} ({mode}) for {FLOOD_SECONDS}s", flush=True)
    flood_entitled(s)
    before = restarted_pods()
    acked_before = {kind: len(ids) for kind, ids in driver_acked().items()}
    flood_set(mode)
    time.sleep(FLOOD_SECONDS)
    acked_during = {kind: len(ids) for kind, ids in driver_acked().items()}
    # What the flooder heard is read from its log from here: a calm before
    # this one (memory-pressure's, before noisy-plugin) never stands for
    # this one's. The flood's own END is written after the calm is set.
    kubectl("exec", driver_pod(FLOOD), "-c", "plugin", "--", "sh", "-c", ": > /tmp/flood.log")
    flood_set("calm")
    ended = happens(lambda: flood_heard(mode, ended=True), 60, every=2)
    heard = flood_heard(mode, ended=True)
    s.check(ended, f"{failure}: the flood ended and said what it heard ({heard})")
    s.check(heard.get(reason, 0) > 0,
            f"{failure}: the flooder was refused for load, {reason} {heard.get(reason, 0)} time(s)")
    s.check(heard.get("retry_after_min", 0) >= 1,
            f"{failure}: every refusal for load carried a retry-after, the least {heard.get('retry_after_min', 0)} s")
    for kind, band in FLOOD_BAND.items():
        rose = acked_during.get(kind, 0) - acked_before.get(kind, 0)
        s.check(rose >= band, f"{failure}: the other instances within their band, {rose} {kind}(s) "
                f"acknowledged during the flood (at least {band})")
    after = restarted_pods()
    again = sorted(name for name, (count, _) in after.items() if count > before.get(name, (0, ""))[0])
    killed = sorted(name for name, (_, why) in after.items() if why == "OOMKilled")
    s.check(not again, f"{failure}: no pod restarted ({', '.join(again) or 'none'})")
    s.check(not killed, f"{failure}: nothing OOMKilled ({', '.join(killed) or 'none'})")
    began = time.time()
    answered = happens(lambda: flood_heard("calm").get("answered", 0) > 0
                       and flood_heard("calm").get(reason, 0) == 0, FLOOD_RECOVERED * 2, every=1)
    flood_set("")
    claim(s, failure, component, time.time() - began if answered else FLOOD_RECOVERED * 2, FLOOD_RECOVERED,
          {"refused": heard.get(reason, 0), "answered": heard.get("answered", 0),
           "retry_after_min_s": heard.get("retry_after_min", 0)})


# ── A reader held behind (contract v30, stage B) ──────────────────────────

def reader_log(pod_name, said, since="10m", container="bor"):
    """How many times the reader's log says `said`, in the last `since`."""
    logs = kubectl("logs", pod_name, "-c", container, f"--since={since}", check=False, quiet=True)
    return logs.stdout.decode(errors="replace").count(said) if logs.returncode == 0 else 0


def reader_said(pod_name, said, since="10m", container="bor"):
    """The lines of the reader's log that say `said`, in the last `since`."""
    logs = kubectl("logs", pod_name, "-c", container, f"--since={since}", check=False, quiet=True)
    if logs.returncode != 0:
        return []
    return [line for line in logs.stdout.decode(errors="replace").splitlines() if said in line]


FROM_PRIMARY = "answered from the primary"


def reader_held(s):
    """The book's reader held behind its writer on purpose while the oms
    driver appends and reads its accounts through its sidecar: each read
    names the watermark of the driver's own last write (read_after, stamped
    by the sidecar), so one the held reader takes waits up to its bound or
    the call's deadline less a second and is answered from the primary,
    never from behind it, and the reader stays ready. Let go, it reads the
    gap from the writer and answers from its own record again. What every
    read saw is Elle's to hold, after the run (tools/elle-check.sh): every
    acknowledged append in every later read, whichever member answered it
    (the spec's section 5, "Tested")."""
    print("R: a reader held behind answers from the primary", flush=True)
    readers = pods("bor-reader")
    if not readers:
        s.check(False, "reader held: the book has no reader")
        return
    reader = readers[0]["metadata"]["name"]
    before = reader_log(reader, FROM_PRIMARY)
    kubectl("exec", reader, "-c", "bor", "--", "touch", HOLD_FILE)
    try:
        answered = happens(lambda: reader_log(reader, FROM_PRIMARY) > before, 90, every=3)
        s.check(answered, f"reader held: the book's reader, behind, answered from the primary "
                          f"({reader_log(reader, FROM_PRIMARY) - before} reads)")
        s.check(ready_of(next((p for p in pods("bor-reader") if p["metadata"]["name"] == reader), None)),
                "reader held: the reader stayed ready, serving")
    finally:
        kubectl("exec", reader, "-c", "bor", "--", "rm", "-f", HOLD_FILE, check=False)
    # Let go: what it lacks is read from the writer as the writer next
    # answers it, a partition not written since among them; the reads after
    # are its own again. Five seconds to settle, then none answered from the
    # primary in the fifteen after. Counted over those seconds alone: the
    # count over a sliding minute, compared before and after, fell as older
    # lines left the minute, so a reader behind throughout passed or failed
    # by where its lines fell (failover-repeat 43697279874, ha run 2).
    time.sleep(20)
    behind = reader_said(reader, FROM_PRIMARY, since="14s")
    s.check(not behind, "reader held: let go, it caught up and answers from its own record again")
    if behind:
        s.note(f"reader held: {len(behind)} read(s) answered from the primary after it was let go, the last "
               f"{behind[-1][-160:]}")
    probe_said = (probe(reader, "bor") or {}).get("member", {})
    s.check(probe_said.get("mode") == "reader", f"reader held: still a reader ({probe_said.get('mode')})")


HANDED_ON = "a read handed to the writer: answered by it"
# Reads of the flood's dataset served, not kept, each answered the price
# served for it: enough that the broker's queue group gives the lake's
# reader some (each read goes to one of two members).
UNKEPT_READS = 20


def unkept_said():
    """The flood's last word on its reads of the dataset served, not kept,
    as it wrote it."""
    said = {}
    for line in driver_file(FLOOD, "/tmp/unkept.log").splitlines():
        parts = line.split()
        if parts and parts[0] == "UNKEPT":
            said = dict(p.split("=", 1) for p in parts[1:] if "=" in p)
    return said


def unkept_heard():
    """Its counts."""
    return {k: int(v) for k, v in unkept_said().items() if v.isdigit()}


def handed_list_prices(pod_name, since_s):
    """The lake reader's list-prices reads handed to the writer and
    answered by it, in the last `since_s` seconds."""
    logs = kubectl("logs", pod_name, "-c", "lake", f"--since={since_s}s", check=False, quiet=True)
    if logs.returncode != 0:
        return 0
    return sum(1 for line in logs.stdout.decode(errors="replace").splitlines()
               if HANDED_ON in line and "platform.lake.query.list-prices" in line)


def reader_hands_on(s):
    """A read whose answer depends on the lake writer's own acts, taken by
    the lake's reader: the flood instance reads its dataset served, not kept,
    a price at a time, each read raising a want -- the writer's recorded act
    -- that the instance serves. The reader takes some of them (the queue
    group), hands each to the writer on
    platform.lake.internal.read-from-writer and answers its reply, so every
    read is answered the price served for it, whichever member took it (the
    spec's section 5, approved 2026-11-10)."""
    print("R: a lake reader hands its writer a read that depends on it", flush=True)
    readers = pods("lake-reader")
    if not readers:
        s.check(False, "reader hands on: the lake has no reader")
        return
    reader = readers[0]["metadata"]["name"]
    flood_entitled(s)
    started = time.time()
    flood_set("unkept")
    try:
        # Until the licence and entitlement the run recorded are in force and
        # the reads are served (the conductor publishes within a minute).
        enough = happens(lambda: unkept_heard().get("served", 0) >= UNKEPT_READS, 240, every=3)
    finally:
        flood_set("")
    said = unkept_said()
    heard = unkept_heard()
    wants = driver_file(FLOOD, "/tmp/unkept-wants.log").splitlines()
    s.note(f"reader hands on: the flood's last read said {said}; it heard {len(wants)} want line(s), "
           f"the last {wants[-3:]}")
    s.check(enough, f"reader hands on: {UNKEPT_READS} reads of the flood's dataset served, not kept, "
                    f"each answered the price served for its want ({heard})")
    handed = handed_list_prices(reader, int(time.time() - started) + 5)
    s.check(handed > 0, f"reader hands on: the lake's reader handed {handed} of those reads to the writer, "
                        f"each answered by it")


def api_addresses():
    """The API server's addresses, as its Service's endpoints name them."""
    said = run("kubectl", "get", "endpoints", "kubernetes", "-o",
               "jsonpath={.subsets[*].addresses[*].ip}").stdout.decode().split()
    return [a for a in said if a]


def cut_api(verb, only=None):
    """Every pod of the release cut from the API server, both ways, on its
    node: routed through the node (FORWARD), and to the node itself where
    the API server runs there (INPUT), in the mangle table (CUT). One exec
    per node, so the cut starts and ends at once. `only`, a set of pod
    names, cuts those alone (step 3d, checkpoint 5, item 32)."""
    api = api_addresses()
    by_node = {}
    for pod in json.loads(out("get", "pods", "-o", "json"))["items"]:
        ip, node = pod["status"].get("podIP"), pod["spec"].get("nodeName")
        if only is not None and pod["metadata"]["name"] not in only:
            continue
        if ip and node and not pod["metadata"].get("deletionTimestamp"):
            by_node.setdefault(node, set()).add(ip)
    at = "1 " if verb == "-I" else ""
    for node, ips in by_node.items():
        rules = []
        for ip in sorted(ips):
            for target in api:
                rules += [f"{CUT} {verb} FORWARD {at}-s {ip} -d {target} -j DROP",
                          f"{CUT} {verb} FORWARD {at}-s {target} -d {ip} -j DROP",
                          f"{CUT} {verb} INPUT {at}-s {ip} -d {target} -j DROP"]
        joined = " && ".join(rules) if verb == "-I" else "; ".join(f"{r} || true" for r in rules)
        run("docker", "exec", node, "sh", "-c", joined or "true", check=verb == "-I")
    return api


def ack_gaps(began_ms, ended_ms):
    """The longest pause, in seconds, between the driver's acknowledgements
    of each kind within a window, its edges counted."""
    said = {}
    for kind in ("price", "order"):
        times = sorted(at for at in driver_acked().get(kind, {}).values() if began_ms <= at <= ended_ms)
        edges = [began_ms] + times + [ended_ms]
        said[kind] = round(max(b - a for a, b in zip(edges, edges[1:])) / 1000, 2)
    return said


# How long the API server is cut off, and the pause in the driver's
# acknowledgements that counts as a write outage: a write is acknowledged
# every half second, so anything past a few seconds is the stores stopping.
API_CUT_S = 30
WRITE_PAUSE_S = 5


def apiserver_lost(s):
    """FAILURE=apiserver (contract v30, stage C; the resilience spec's
    sections 2, 10 and 15, approved 2026-11-10): the API server cut from
    every pod of the release for 30 s under the driver's load. Writers keep
    writing under their epoch and stay ready: no takeover (each store's
    epoch as it was), no write outage (the book and the lake acknowledging
    the driver throughout), and no readiness flap (every store member, its
    readers' included, ready throughout and after)."""
    print(f"F: apiserver: the API server cut from the release's pods for {API_CUT_S} s, under the driver's "
          "writes", flush=True)
    # Every store settled first, so a member still coming back from an
    # earlier class is never taken for one this one flapped.
    for component in [c for c in STORES if c != "launcher"]:
        wait_for(f"{component} settled", lambda c=component: members_settled(c), seconds=300)
    watched = [c for c in STORES if c != "launcher"] + [f"{c}-reader" for c in ("lake", "bor")]
    wait_for("every member ready", lambda: all(ready(p) for c in watched for p in pods(c)), seconds=300)
    epochs = {c: epoch_now(c) for c in ("bor", "lake", "instrument", "street", "conductor")}
    members = {p["metadata"]["name"]: c for c in watched for p in pods(c)}
    restarted = {name: restarts(name) for name in members}
    flaps = set()
    began = time.time()
    api = cut_api("-I")
    s.note(f"apiserver: {len(api)} API server address(es) cut from every pod")
    try:
        while time.time() - began < API_CUT_S:
            for pod in json.loads(out("get", "pods", "-o", "json"))["items"]:
                if pod["metadata"]["name"] in members and not ready(pod):
                    flaps.add(pod["metadata"]["name"])
            time.sleep(1)
    finally:
        cut_api("-D")
    ended = time.time()
    # And for a while after: a member that stepped down at the cut's end.
    for _ in range(15):
        for pod in json.loads(out("get", "pods", "-o", "json"))["items"]:
            name = pod["metadata"]["name"]
            if name in members and (not ready(pod) or restarts(name) > restarted[name]):
                flaps.add(name)
        time.sleep(1)
    s.check(not flaps, f"apiserver: no member's readiness flapped ({', '.join(sorted(flaps)) or 'none'})")
    after = {c: epoch_now(c) for c in epochs}
    moved = {c: (epochs[c], after[c]) for c in epochs if after[c] != epochs[c]}
    s.check(not moved, f"apiserver: no takeover, every store's epoch as it was "
            f"({', '.join(f'{c} {a}->{b}' for c, (a, b) in moved.items()) or 'none moved'})")
    if DRIVEN:
        gaps = ack_gaps(int(began * 1000), int(ended * 1000))
        s.check(all(g <= WRITE_PAUSE_S for g in gaps.values()),
                f"apiserver: no write outage, the driver acknowledged throughout (longest pause: "
                f"book {gaps['order']} s, lake {gaps['price']} s; at most {WRITE_PAUSE_S} s)")
        claim(s, "apiserver", "bor", gaps["order"], WRITE_PAUSE_S,
              {"what": "the longest pause in the book's acknowledgements while the API server was cut",
               "cut_s": API_CUT_S, "takeovers": len(moved), "readiness_flaps": len(flaps)})
        claim(s, "apiserver", "lake", gaps["price"], WRITE_PAUSE_S,
              {"what": "the longest pause in the lake's acknowledgements while the API server was cut",
               "cut_s": API_CUT_S, "takeovers": len(moved), "readiness_flaps": len(flaps)})


# The write-ahead-bound class's stream bound, short, and how long past it
# the lake's writer is held.
SHORT_MAX_AGE_S = 20
HELD_PAST_S = 25


def gaps_of(dataset, since_ns):
    """Each gap of `dataset` recorded since `since_ns`, by its ID, its states
    in order (lake_dataset_gap; GapState numbers)."""
    said = psql("select gap_id, state from lake_dataset_gap where dataset = "
                f"'{dataset}' and recorded_at_ns >= {since_ns} order by change_id", quiet=True)
    gaps = {}
    for line in said.splitlines():
        parts = line.split("|") if "|" in line else line.split()
        if len(parts) == 2 and parts[1].strip().isdigit():
            gaps.setdefault(parts[0].strip(), []).append(int(parts[1]))
    return gaps


def proto_text(body, number):
    """A protobuf message's string field `number`, from its bytes; empty
    where it is not set. Enough of the wire format to read one field."""
    at = 0

    def varint():
        nonlocal at
        value, shift = 0, 0
        while True:
            byte = body[at]
            at += 1
            value |= (byte & 0x7F) << shift
            shift += 7
            if byte < 0x80:
                return value
    while at < len(body):
        key = varint()
        field, wire = key >> 3, key & 7
        if wire == 0:
            varint()
        elif wire == 1:
            at += 8
        elif wire == 5:
            at += 4
        elif wire == 2:
            length = varint()
            if field == number:
                return body[at:at + length].decode()
            at += length
        else:
            return ""
    return ""


def gap_notices():
    """Each gap's `gap_found` notices in the dashboard's inbox, by the gap's
    ID its words begin with: (when filed, to whom) each. Empty where the
    dashboard's table cannot be read yet."""
    try:
        said = psql("select about, changed_ns, subject from dashboard_notice where kind = 'gap_found'",
                    quiet=True)
    except SystemExit:
        return {}
    found = {}
    for line in said.splitlines():
        # A subject holds a bar itself (local|alice): the about and the time
        # first, the subject whole after them.
        parts = line.split("|", 2)
        if len(parts) == 3 and parts[0].startswith("GAP-") and parts[1].strip().isdigit():
            found.setdefault(parts[0].split()[0], []).append((int(parts[1]), parts[2].strip()))
    return found


def gaps_found_in(dataset, since_ns):
    """Each gap recorded since `since_ns` -- of `dataset`, or with it empty
    the unattributed ones -- by its ID: the gap it was found from
    (DatasetGap.found_in_gap_id, field 9) and when it was first recorded."""
    said = psql("select gap_id, min(recorded_at_ns), encode((array_agg(body order by change_id))[1], 'hex') "
                f"from lake_dataset_gap where dataset = '{dataset}' and recorded_at_ns >= {since_ns} "
                "group by gap_id", quiet=True)
    found = {}
    for line in said.splitlines():
        parts = line.split("|")
        if len(parts) == 3 and parts[1].strip().isdigit():
            found[parts[0].strip()] = (proto_text(bytes.fromhex(parts[2].strip()), 9), int(parts[1]))
    return found


def write_ahead_bound(s):
    """FAILURE=write-ahead-bound (HA; contract v30, stage C; the resilience
    spec's sections 4 and 15): the lake's write-ahead stream bounded short
    (lake.writeAhead.maxAge), and the lake's writer cut from the broker past
    it while the data driver writes, so the stream drops writes the lake
    never took (none acknowledged: a write is answered after the lake
    commits it). Let go, the writer finds the stream's sequence passed rows
    it never committed: a gap recorded at once, its datasets not yet known
    (dataset empty; stage C2, approved 2026-11-11), then the gap of the
    driver's dataset recorded with its next write, naming it
    (found_in_gap_id; GAP_CAUSE_WRITE_AHEAD_BOUND), asked again where the dataset
    declares backfill and unrecovered where it declares none (the driver
    declares none), and read where the dataset is (lake_dataset_gap, which
    ListDatasetGaps answers). The bound set back after."""
    print(f"F: write-ahead-bound: the lake's writer held from its stream past a {SHORT_MAX_AGE_S} s bound",
          flush=True)
    if not DRIVEN:
        s.check(False, "write-ahead-bound: no driver writes ahead")
        return
    before = upgrade(f"lake.writeAhead.maxAge={SHORT_MAX_AGE_S}s")
    s.check(before.returncode == 0, "write-ahead-bound: the stream's bound set short by an upgrade")
    wait_for("lake settled", lambda: members_settled("lake"), seconds=600)
    # The upgrade's roll of the lake's members and readers finished, so no
    # member listed below is one it is still replacing (failover-repeat
    # 43015006850: a lake pod listed was gone by the cut, "get pod ...
    # failed"); then the lease's holder a pod that runs.
    for rolled in ("lake", "lake-reader"):
        kubectl("rollout", "status", f"deployment/{FULL}-{rolled}", "--timeout=600s", check=False)
    upgrade_rolled_out(s, "write-ahead-bound: the bound set short")
    wait_for("the lake's writer running", lambda: writer_pod("lake") in
             [p["metadata"]["name"] for p in pods("lake")], seconds=300)
    since = int(time.time() * 1e9)
    # Every lake member held from the broker, its standby and its readers
    # too: a standby that took over would take the stream on in time.
    held = [p["metadata"]["name"] for c in ("lake", "lake-reader") for p in pods(c)]
    broker = [pod_ip(p["metadata"]["name"]) for p in pods("broker")]
    began = time.time()
    for member in held:
        cut_rules(member, broker, "-I")
    try:
        time.sleep(SHORT_MAX_AGE_S + HELD_PAST_S)
    finally:
        for member in held:
            cut_rules(member, broker, "-D")
    let_go = time.time()
    dataset = "drive-data:drive"
    # Found the moment the stream skips: a gap, its datasets not yet known.
    unattributed = happens(lambda: gaps_found_in("", since), 180, every=1)
    first = gaps_found_in("", since)
    found_at = min((at for _, at in first.values()), default=0)
    at_once = (found_at / 1e9 - let_go) if found_at else None
    s.check(unattributed, "write-ahead-bound: a gap recorded the moment the stream skipped, its datasets not "
            f"yet known ({len(first)} gap(s))")
    figure("write-ahead-bound: a gap recorded after the writer was let go",
           recorded_s=None if at_once is None else round(at_once, 2))
    found = happens(lambda: gaps_of(dataset, since), 180, every=3)
    took = time.time() - let_go
    gaps = gaps_of(dataset, since)
    s.check(found, f"write-ahead-bound: the gap the stream left in {dataset} recorded "
            f"({len(gaps)} gap(s))")
    figure(f"write-ahead-bound: the gap in {dataset} recorded after the writer was let go", recorded_s=round(took, 2))
    # Then the dataset's own gap, naming the one found when the stream
    # skipped (found_in_gap_id).
    named = gaps_found_in(dataset, since)
    s.check(bool(named) and all(found_in in first for found_in, _ in named.values()),
            f"write-ahead-bound: {dataset}'s gap names the gap found when the stream skipped "
            f"({sorted(found_in for found_in, _ in named.values())} in {sorted(first)})")
    s.check(bool(named) and found_at > 0 and all(at >= found_at for _, at in named.values()),
            "write-ahead-bound: the unattributed gap recorded first, then the dataset's")
    # Each gap's notice in the deployment admin's inbox (`gap_found`, ruled
    # 2026-11-11; contract v30, stage D): the unattributed gap's, then the
    # dataset's, once each to each admin, filed within the dashboard's ask
    # (10 s) of the lake recording it.
    wanted = sorted(first) + sorted(g for g in named if g not in first)
    filed = happens(lambda: all(g in gap_notices() for g in wanted), 90, every=2) if wanted else False
    notices = gap_notices()
    s.check(filed, "write-ahead-bound: a gap_found notice in the deployment admin's inbox for each gap "
            f"({', '.join(f'{g}: {len(notices.get(g, []))}' for g in wanted) or 'no gap'})")
    once = all(len(notices.get(g, [])) == len({who for _, who in notices.get(g, [])}) for g in wanted)
    s.check(filed and once, "write-ahead-bound: each gap's notice filed once to each admin")
    first_at = min((at for g in first for at, _ in notices.get(g, [])), default=0)
    named_at = min((at for g in named for at, _ in notices.get(g, [])), default=0)
    s.check(filed and 0 < first_at <= named_at,
            "write-ahead-bound: the unattributed gap's notice first, then the dataset's")
    if not found:
        # What the lake's members said of the stream meanwhile, before the
        # bound set back rolls them away.
        words = ("write-ahead", "dropped writes", "a gap in a dataset", "stepping down", "takes its writes")
        for pod in pods("lake"):
            said = kubectl("logs", pod["metadata"]["name"], "-c", "lake", "--since=6m", check=False, quiet=True)
            lines = [line for line in said.stdout.decode(errors="replace").splitlines()
                     if any(w in line for w in words)]
            for line in lines[-12:]:
                s.note(f"write-ahead-bound: {pod['metadata']['name']}: {line[-300:]}")
    states = [states[-1] for states in gaps.values()]
    # The driver's dataset declares no gap_recovery: none, the cautious
    # reading, so its gap is unrecovered at once and asked of nobody.
    s.check(bool(states) and all(state == 3 for state in states),
            f"write-ahead-bound: each gap unrecovered, its dataset declaring no backfill ({states})")
    if found:
        claim(s, "write-ahead-bound", "lake", took, 60,
              {"what": "from the writer let go to its dataset's gap recorded", "held_s":
               round(let_go - began, 1), "max_age_s": SHORT_MAX_AGE_S, "gaps": len(gaps),
               "unattributed_s": round(at_once, 1) if at_once is not None else None})
    back = upgrade("lake.writeAhead.maxAge=10m")
    s.check(back.returncode == 0, "write-ahead-bound: the stream's bound set back")
    wait_for("lake settled", lambda: members_settled("lake"), seconds=600)
    upgrade_rolled_out(s, "write-ahead-bound: the bound set back")
    # Every member of the sets this run pinned placed and ready after both
    # upgrades: none left Pending where no node can hold it.
    for name in pinned_sets():
        count = int(out("get", "statefulset", name, "-o", "jsonpath={.spec.replicas}") or 0)
        placed = happens(lambda n=name, c=count: int(out("get", "statefulset", n, "-o",
                                                          "jsonpath={.status.readyReplicas}") or 0) == c, 300)
        s.check(placed, f"write-ahead-bound: {name}'s {count} members placed and ready after the upgrades")
    # Nothing acknowledged was lost: the driver's rows held as every class's.
    acks = acks_again(int(let_go * 1000), seconds=180)
    s.check(acks, "write-ahead-bound: the driver acknowledged by the lake again")


STANDBY_LOST = {}


def standby_lost(s):
    """In ha, one of the database's two synchronous standbys killed outright
    under the driver's load (contract v30, stage C; the resilience spec's
    section 4: a quorum of either, ANY 1): the book's commits go on within
    their bound, never waiting on the lost standby."""
    standbys = [p["metadata"]["name"] for p in pods("database")
                if p["metadata"]["labels"].get("meridian.dev/database-role") == "replica"]
    if len(standbys) < 2:
        s.check(False, f"db-primary: two standbys to lose one of ({standbys})")
        return
    victim = standbys[0]
    print(f"F: db-primary: a synchronous standby ({victim}) killed outright, under the driver's writes",
          flush=True)
    began = time.time()
    isolate(victim, "-I")
    crash(victim, "postgres")
    try:
        time.sleep(20)
    finally:
        isolate(victim, "-D")
    ended = time.time()
    gaps = ack_gaps(int(began * 1000), int(ended * 1000))
    s.check(gaps["order"] <= HA["bor"],
            f"db-primary: with a standby lost, the book's commits went on (longest pause "
            f"{gaps['order']} s, within {HA['bor']} s)")
    # Said in the db-primary class's claims, beside its RPO.
    STANDBY_LOST.update({"one_standby_lost_book_pause_s": gaps["order"],
                         "one_standby_lost_lake_pause_s": gaps["price"]})
    healed = happens(lambda: len(pods("database")) == database_members()
                     and all(ready(p) for p in pods("database")), 300)
    s.check(healed, "db-primary: the lost standby back")


# The buy-side trust targets (contract v30, stage C): a streamed price's
# stale threshold (spec/sessions-and-data-report-their-latency-and-quality,
# 5 s), and the 2 s past it a stale flag may take (approved 2026-11-11).
STALE_AFTER_S = 5
STALE_FLAG_S = 2


def page_reads(since_ms):
    """The data driver's page reads since `since_ms`: (unix ms, state, row
    key, valid ms)."""
    reads = []
    for line in driver_file("drive-data", "/tmp/page.log").splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[0] == "PAGE" and parts[1].isdigit() and int(parts[1]) >= since_ms:
            row = parts[3] if parts[2] == "ok" and len(parts) == 5 else ""
            valid = int(parts[4]) if parts[2] == "ok" and len(parts) == 5 and parts[4].isdigit() else 0
            reads.append((int(parts[1]), parts[2], row, valid))
    return reads


def lake_newest(since_ms, until_ms):
    """Each row of the driver's dataset the lake recorded in the window and
    a little before: (recorded ms, valid ms), in recorded order."""
    said = psql("select recorded_at_ns / 1000000, valid_from_ns / 1000000 from lake_row where dataset = "
                f"'drive-data:drive' and recorded_at_ns >= {(since_ms - 60_000) * 1_000_000} and "
                f"recorded_at_ns <= {until_ms * 1_000_000} order by recorded_at_ns", quiet=True)
    rows = []
    for line in said.splitlines():
        parts = line.replace("|", " ").split()
        if len(parts) == 2 and all(p.lstrip("-").isdigit() for p in parts):
            rows.append((int(parts[0]), int(parts[1])))
    return rows


def trust_targets(s, failure, began_ms):
    """The buy-side trust targets in a class (contract v30, stage C; the
    resilience spec's section 15, approved 2026-11-10 and 2026-11-11), as a
    live page drawn from the driver's reads would show its price:

    - stale on time: the page ages its value every second from the row's own
      valid time; it holds the newest row its reads answered. Each second of
      the class where the lake's newest row (by what it had recorded then)
      was past the threshold, the page must have flagged it by the threshold
      plus 2 s, and never shown it live where the lake held nothing newer;
    - never blank: every read answered drew a row -- the last good one at
      least -- never an empty answer. A failed read leaves the page drawn.

    What it cannot see: the kit's own cells (the kit's step), a page's
    render, a browser's clock, deliveries a page hears between reads."""
    if not DRIVEN:
        return
    # The page reads about once a second: a class shorter than that (the
    # street's network-policy, healed in under one) is judged once the page
    # has read in it, waited for at most 5 s.
    reads = page_reads(began_ms)
    waited = time.time()
    while not reads and time.time() - waited < 5:
        time.sleep(0.5)
        reads = page_reads(began_ms)
    ended_ms = int(time.time() * 1000)
    if not reads:
        s.check(False, f"{failure}: trust targets: the page read nothing in the class")
        return
    empty = sum(1 for r in reads if r[1] == "empty")
    failed = sum(1 for r in reads if r[1] == "failed")
    answered = [r for r in reads if r[1] == "ok"]
    longest = 0
    last = began_ms
    for at, state, _, _ in reads:
        if state == "ok":
            longest = max(longest, at - last)
            last = at
    longest = max(longest, ended_ms - last)
    s.check(empty == 0, f"{failure}: never blank: every read the page answered drew a row ({len(reads)} reads, "
            f"{empty} empty, {failed} failed and left the page as drawn)")
    figure(f"{failure}: the page's longest stretch without a fresh answer", longest_s=round(longest / 1000, 2))
    newest = lake_newest(began_ms, ended_ms)
    late, live_while_stale, flagged = 0.0, 0, 0
    held_valid = None
    i, j = 0, 0
    lake_valid = None
    for tick in range(began_ms + 1000, ended_ms, 1000):
        while i < len(answered) and answered[i][0] <= tick:
            held_valid = answered[i][3] if held_valid is None else max(held_valid, answered[i][3])
            i += 1
        while j < len(newest) and newest[j][0] <= tick:
            lake_valid = newest[j][1] if lake_valid is None else max(lake_valid, newest[j][1])
            j += 1
        if lake_valid is None or held_valid is None:
            continue
        due = lake_valid + STALE_AFTER_S * 1000
        page_stale = tick - held_valid > STALE_AFTER_S * 1000
        if tick > due:
            if page_stale:
                flagged += 1
            else:
                live_while_stale += 1
                late = max(late, (tick - due) / 1000)
    s.check(live_while_stale == 0 or late <= STALE_FLAG_S,
            f"{failure}: stale on time: the page flagged its price stale within {STALE_AFTER_S} s + "
            f"{STALE_FLAG_S} s of it turning stale ({flagged} stale second(s) flagged, "
            f"{live_while_stale} shown live)")
    claim(s, f"{failure}", "page", late, STALE_FLAG_S,
          {"what": "the buy-side trust targets: the page's stale flag past its threshold, and blank reads",
           "stale_after_s": STALE_AFTER_S, "stale_seconds_flagged": flagged,
           "shown_live_while_stale": live_while_stale, "reads": len(reads), "blank_reads": empty,
           "failed_reads": failed, "longest_without_fresh_s": round(longest / 1000, 1)})


def rows_read_are_kept(s, failure, began_ms):
    """Every row the page read in the class is one the lake's record holds:
    no member -- a reader's hot layer above all -- answered a change the
    record does not (contract v30, stage C; the resilience spec's section 3:
    after a database failover, a reader holds exactly what the record
    holds)."""
    if not DRIVEN:
        return
    keys = sorted({r[2] for r in page_reads(began_ms) if r[1] == "ok" and r[2]})
    if not keys:
        return
    held = set(psql("select row_key from lake_row where dataset = 'drive-data:drive' and row_key = any("
                    "array[" + ",".join(f"'{k}'" for k in keys) + "])", quiet=True).split())
    missing = [k for k in keys if k not in held]
    s.check(not missing, f"{failure}: every row the page read is in the lake's record ({len(keys)} read, "
            f"{len(missing)} not held{': ' + ', '.join(missing[:5]) if missing else ''})")


def entitled(dataset, instance, why):
    """An instance entitled to read a dataset, as a deployment admin
    entitles a reader (W10.1): its DatasetEntitlement recorded where the
    conductor keeps it, which the conductor publishes whole within a
    minute."""
    now = time.time_ns()
    entitlement = (text_field(1, dataset) + text_field(2, instance) + varint(3 << 3) + varint(1)
                   + text_field(5, ADMIN) + varint(6 << 3) + varint(now) + text_field(9, why))
    psql("INSERT INTO config_dataset_entitlement_change (dataset, instance, entitlement, changed_at_ns) "
         f"VALUES ('{dataset}', '{instance}', decode('{entitlement.hex()}', 'hex'), {now})")


def page_entitled(s):
    """The data driver entitled to read its own dataset back (entitled),
    and reading it. Once a run."""
    entitled("drive-data:drive", "drive-data", "the buy-side trust targets: the page reads the driver's price back")
    reading = happens(lambda: any(r[1] == "ok" for r in page_reads(0)), 240, every=5)
    s.check(reading, "the page reads the driver's price back")


# ── Rebalance (step 3d; the resilience spec's sections 6.8 and 15) ──────────
#
# After the two writer groups (writer_groups), under the driver's load, as a
# deployment admin does it: signed in at the dashboard, a lake range, a book
# partition and an account moved by the dashboard's own tools
# (`dashboard__move_partition`, `dashboard__move_account`) and its Move
# dialog, within one database and then across two (`lake.databases` and
# `bor.databases` set to 2 by an upgrade). Each move holds the writes to what
# it moves for at most moves.holdSeconds: the driver's p99 write time while
# it ran within it, and none of its writes refused; it switches only with the
# digests equal (its PartitionMoved, no PartitionMoveAborted); and what was
# committed before it reads the same after it, from where it is written now.
# An injected lag and an injected digest mismatch each abort a move, the
# source still owning and writing, with PartitionMoveAborted (and the
# digest's IntegrityFinding).


def admin_password_hash():
    """The deployment admin's password, drawn and kept in ADMIN_PASSWORD,
    and its Argon2id hash at the dashboard's defaults in the PHC form it
    reads, made as the harness makes its own (deploy/harness/compose.yaml):
    by OpenSSL 3.3's ARGON2ID, the password on its standard input alone."""
    global ADMIN_PASSWORD
    ADMIN_PASSWORD = "h" + os.urandom(24).hex()
    salt = os.urandom(16)
    done = run("docker", "run", "--rm", "-i", "--entrypoint", "sh", "alpine/openssl:3.3.2", "-c",
               'openssl kdf -keylen 32 -kdfopt pass:"$(cat)" -kdfopt hexsalt:' + salt.hex()
               + " -kdfopt iter:2 -kdfopt memcost:19456 -kdfopt lanes:1 ARGON2ID",
               input=ADMIN_PASSWORD.encode(), quiet=True)
    key = bytes.fromhex(done.stdout.decode().strip().replace(":", ""))

    def b64(raw):
        return base64.b64encode(raw).decode().rstrip("=")
    return f"$argon2id$v=19$m=19456,t=2,p=1${b64(salt)}${b64(key)}"


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class Dashboard:
    """The deployment admin at the dashboard, over a port-forward to its
    Service, as the host it is configured at: her browser's cookies, and an
    MCP client she authorised through its own OAuth (as the harness's `mcp
    connect` does)."""

    HOST = "failover-e2e.localhost"
    CALLBACK = "http://127.0.0.1:53682/callback"
    # How many times a request whose forward ended is sent again, each on a
    # forward made anew.
    RESENDS = 2

    def __init__(self):
        import socket
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        self.port = port
        self.base = f"http://127.0.0.1:{port}"
        self.cookies = {}
        self.access = None
        self.forward = None
        if not self.forwarded():
            raise SystemExit("the dashboard did not answer over its port-forward")

    def forwarded(self):
        """A port-forward to the dashboard's Service, made anew: one lands
        on the pod the Service chose when it was made, and ends with it.
        Whether the dashboard answers over it."""
        self.close()

        def answers():
            if self.forward is None or self.forward.poll() is not None:
                self.forward = subprocess.Popen(
                    ["kubectl", "--namespace", NAMESPACE, "port-forward", f"svc/{FULL}-dashboard",
                     f"{self.port}:80"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                time.sleep(1)
            try:
                return self.sent("GET", "/sign-in")[0] == 200
            except (OSError, http.client.HTTPException):
                return False
        if happens(answers, 120, every=1):
            return True
        self.close()
        return False

    def close(self):
        if self.forward is not None:
            self.forward.terminate()
            self.forward.wait()
            self.forward = None

    @staticmethod
    def ended(error):
        """Whether `error` is the forward ending under a request: the pod it
        landed on stopped (a rollout's, PR CI 44604574060), the connection
        closed with no response, reset or refused."""
        if isinstance(error, urllib.error.HTTPError):
            return False
        if isinstance(error, urllib.error.URLError):
            error = error.reason
        return isinstance(error, (ConnectionError, http.client.RemoteDisconnected))

    def made_anew(self, method, path, error, attempt):
        """The forward made anew after it ended under `method path`, said."""
        print(f"R: the dashboard's forward ended under {method} {path} ({error!r}); made anew "
              f"({attempt} of {self.RESENDS})", flush=True)
        if not self.forwarded():
            raise SystemExit(f"{method} {path}: the dashboard's forward ended under it, and it did not "
                             f"answer over a forward made anew")

    def send(self, method, path, fields=None, body=None, bearer=None, again=None):
        """The request sent; its status, body and Location. Where the forward
        ends under it, the forward is made anew and a GET sent again, up to
        RESENDS times; any other request only where `again()` says what it
        would start has not begun, and otherwise it stops here: it may
        have acted."""
        for attempt in range(self.RESENDS + 1):
            try:
                return self.sent(method, path, fields=fields, body=body, bearer=bearer)
            except (OSError, http.client.HTTPException) as error:
                if not self.ended(error):
                    raise
                if attempt == self.RESENDS:
                    raise SystemExit(f"{method} {path}: the dashboard's forward ended under it "
                                     f"{attempt + 1} times ({error!r})")
                self.made_anew(method, path, error, attempt + 1)
                if method != "GET" and not (again and again()):
                    raise SystemExit(f"{method} {path}: the dashboard's forward ended under it ({error!r}); "
                                     f"made anew, the request not sent again: it may have acted")
        raise AssertionError("unreachable")

    def sent(self, method, path, fields=None, body=None, bearer=None):
        """The request sent once, over the forward as it is."""
        data, kind = None, None
        if body is not None:
            data, kind = json.dumps(body).encode(), "application/json"
        elif fields is not None:
            data, kind = urllib.parse.urlencode(fields).encode(), "application/x-www-form-urlencoded"
        request = urllib.request.Request(self.base + path, data=data, method=method)
        request.add_header("Host", self.HOST)
        if kind:
            request.add_header("Content-Type", kind)
        if bearer:
            request.add_header("Authorization", f"Bearer {bearer}")
        elif self.cookies:
            request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in self.cookies.items()))
        try:
            response = urllib.request.build_opener(NoRedirect).open(request, timeout=60)
            status, raw, headers = response.status, response.read(), response.headers
        except urllib.error.HTTPError as refused:
            status, raw, headers = refused.code, refused.read(), refused.headers
        if not bearer:
            for value in headers.get_all("Set-Cookie") or []:
                name, _, rest = value.partition("=")
                self.cookies[name] = rest.split(";")[0]
        return status, raw.decode("utf-8", errors="replace"), headers.get("Location")

    def connect(self):
        """Her MCP client, authorised for everything she holds."""
        import hashlib
        import secrets as drawn
        resource = f"http://{self.HOST}/mcp"
        status, said, _ = self.send("POST", "/oauth/register",
                                    body={"client_name": "failover rebalance", "redirect_uris": [self.CALLBACK]})
        if status != 201:
            raise SystemExit(f"the MCP client was not registered: {status} {said[:300]}")
        client_id = json.loads(said)["client_id"]
        verifier = drawn.token_urlsafe(48)
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        query = urllib.parse.urlencode({
            "response_type": "code", "client_id": client_id, "redirect_uri": self.CALLBACK,
            "code_challenge": challenge, "code_challenge_method": "S256", "state": "rebalance",
            "resource": resource})
        status, page, _ = self.send("GET", f"/oauth/authorize?{query}")
        found = re.search(r'name="authorize" value="([^"]+)"', page)
        if status != 200 or not found:
            raise SystemExit(f"the authorisation did not ask her to sign in: {status}")
        status, page, _ = self.send("POST", "/sign-in", fields={
            "name": ADMIN.split("|", 1)[1], "password": ADMIN_PASSWORD, "authorize": found.group(1)})
        request = re.search(r'name="request" value="([^"]+)"', page)
        confirm = re.search(r'name="confirm" value="([^"]+)"', page)
        if status != 200 or not request or not confirm:
            raise SystemExit(f"she was not shown the consent page: {status}")
        fields = [("request", request.group(1)), ("confirm", confirm.group(1)), ("decision", "allow"),
                  ("covers", "everything"), ("days", "30")]
        status, _, location = self.send("POST", "/oauth/authorize", fields=fields)
        code = urllib.parse.parse_qs(urllib.parse.urlsplit(location or "").query).get("code", [""])[0]
        if status != 302 or not code:
            raise SystemExit(f"the consent sent back no code: {status}")
        status, said, _ = self.send("POST", "/oauth/token", fields={
            "grant_type": "authorization_code", "code": code, "code_verifier": verifier,
            "redirect_uri": self.CALLBACK, "client_id": client_id, "resource": resource})
        if status != 200:
            raise SystemExit(f"the code was not exchanged: {status}")
        granted = json.loads(said)
        self.access = granted["access_token"]
        self.refresh, self.client_id, self.resource = granted.get("refresh_token"), client_id, resource

    def refreshed(self):
        """Her client's access token refreshed, as a client does when it is
        told its token has expired: by its refresh token, or, given none or
        refused, authorised again. The ha run outlasts an access token's
        life (PR CI 43669650813: "/mcp answered 401 ... the access token has
        expired; refresh it", 29 minutes in)."""
        if getattr(self, "refresh", None):
            status, said, _ = self.send("POST", "/oauth/token", fields={
                "grant_type": "refresh_token", "refresh_token": self.refresh,
                "client_id": self.client_id, "resource": self.resource})
            if status == 200:
                granted = json.loads(said)
                self.access = granted["access_token"]
                self.refresh = granted.get("refresh_token") or self.refresh
                return
        self.connect()

    def call(self, tool, arguments, again=None):
        """A tool called as her client: its structured answer. `again`, as
        `send` takes it: whether the call may be sent again where the
        forward ended under it."""
        asked = {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                 "params": {"name": tool, "arguments": arguments}}
        status, said, _ = self.send("POST", "/mcp", body=asked, bearer=self.access, again=again)
        if status == 401 and "expired" in said:
            self.refreshed()
            status, said, _ = self.send("POST", "/mcp", body=asked, bearer=self.access, again=again)
        if status != 200:
            raise SystemExit(f"{tool}: /mcp answered {status}: {said[:400]}")
        return (json.loads(said).get("result") or {}).get("structuredContent") or {}

    def move_dialog(self, partition, group, note, unbegun):
        """The Resilience page's Move dialog posted, as she posts it signed in:
        its status and where it sent her. Where the forward ends under her
        post, it is made anew and the dialog posted again from the page and
        a token shown again, only once `unbegun()` says, from the store, that
        no move of it began: a post again never starts a second move. Up to
        RESENDS times, each said."""
        path = "/admin/resilience/move-partition"
        for attempt in range(self.RESENDS + 1):
            status, page, _ = self.send("GET", "/admin/resilience")
            token = re.search(r'action="/admin/resilience/move-partition"><input type="hidden" '
                              r'name="form_token" value="([^"]+)"', page)
            if status != 200 or not token:
                raise SystemExit(f"the Resilience page with its Move dialog was not shown: {status}")
            try:
                status, _, location = self.sent("POST", path, fields={
                    "form_token": token.group(1), "partition": partition, "target_group": str(group),
                    "note": note})
                return status, location or ""
            except (OSError, http.client.HTTPException) as error:
                if not self.ended(error):
                    raise
                if attempt == self.RESENDS:
                    raise SystemExit(f"her Move dialog: the dashboard's forward ended under her post "
                                     f"{attempt + 1} times ({error!r})")
                self.made_anew("POST", path, error, attempt + 1)
                if not unbegun():
                    raise SystemExit(f"her Move dialog: the dashboard's forward ended under her post ({error!r}), "
                                     f"and the store shows a move of {partition} begun: not posted again")
                print(f"R: her Move dialog: no move of {partition} begun; posted again", flush=True)
        raise AssertionError("unreachable")

    def sign_in(self):
        status, _, _ = self.send("POST", "/sign-in", fields={
            "name": ADMIN.split("|", 1)[1], "password": ADMIN_PASSWORD})
        if status != 303:
            raise SystemExit(f"she was not signed in: {status}")


def database_pod(store, d):
    """Database `d` of `store`'s primary: the first's, or the one the chart
    brings past it (`<release>-<store>-database-<d>`)."""
    if d == 0:
        return primary()
    component = f"{ {'book': 'bor'}.get(store, store)}-database-{d}"
    said = json.loads(out("get", "pods", "-l",
                          f"app.kubernetes.io/instance={RELEASE},meridian.dev/component={component},"
                          "meridian.dev/database-role=primary", "-o", "json"))["items"]
    live = [p["metadata"]["name"] for p in said if not p["metadata"].get("deletionTimestamp")]
    return live[0] if live else f"{FULL}-{component}-0"


def driver_lines(path):
    """The data and oms instances' lines of `path` in their pods' scratch."""
    return [line.split() for instance in ("drive-data", "drive-oms")
            for line in driver_file(instance, path).splitlines()]


# Each move's window as window_held read it, (began ms, until ms), in turn:
# what refused_between reads the stretches between moves from.
WINDOWS = []

# The lake's and the book's members, whose logs say why a write was refused:
# kept before each of the rebalance class's upgrades rolls them, and at a
# class's first refused write, in one file the job keeps when it fails
# (.e2e-*.log; never from a stalled runner, whose check run is its channel)
# and the next run clears (.e2e-said-*.log). PR CI 45225932982: the class
# rolled every member for two databases before any log was taken, and the
# book's logs of a refused order were lost with them.
STORE_MEMBERS = GROUP_COMPONENTS + ("lake-reader", "bor-reader")
STORE_LOGS = ".e2e-said-stores.log"
# Each kubectl call's own bound: an API server that does not answer costs
# the run this per call, never the job's limit.
STORE_LOGS_TIMEOUT = "20s"
# The classes whose first refused write's logs are kept.
STORE_LOGS_KEPT = set()


def keep_store_logs(when):
    """Every lake and book member's logs (`STORE_MEMBERS`, all containers,
    timestamped, the last SAID_LINES of each), appended to STORE_LOGS under
    `when`. Each call bounded by STORE_LOGS_TIMEOUT; what is not answered is
    said in the file."""
    bound = f"--request-timeout={STORE_LOGS_TIMEOUT}"
    selector = f"app.kubernetes.io/instance={RELEASE},meridian.dev/component in ({','.join(STORE_MEMBERS)})"
    listed = kubectl("get", "pods", "-l", selector, "-o", "name", bound, check=False)
    parts = [f"== {when}, at {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}"]
    if listed.returncode != 0:
        parts.append(f"(the members were not listed: {listed.stderr.decode(errors='replace').strip()})")
    for name in listed.stdout.decode(errors="replace").split():
        said = kubectl("logs", name, "--all-containers", "--prefix", "--timestamps", f"--tail={SAID_LINES}",
                       bound, check=False)
        parts.append(f"== {name}")
        parts.append((said.stdout + said.stderr).decode(errors="replace"))
    with open(STORE_LOGS, "a", encoding="utf-8") as kept:
        kept.write("\n".join(parts) + "\n")
    print(f"R: {when}: the lake's and the book's members' logs, kept in {STORE_LOGS}", flush=True)


def store_logs_at_first_refusal(failure, what):
    """The members' logs kept at `failure`'s first refused write, once."""
    if failure not in STORE_LOGS_KEPT:
        STORE_LOGS_KEPT.add(failure)
        keep_store_logs(f"{failure}: the first refused write, {what}")


def upgrade_kept(*sets):
    """An upgrade (`upgrade`), the members' logs kept before it rolls them."""
    keep_store_logs(f"before the upgrade to {' '.join(sets)}")
    return upgrade(*sets)


def window_held(s, what, began_ms, ended_ms):
    """The driver's writes while a move ran, and through the hold after it
    began: their p99 time within the hold, and none refused. A move shorter
    than the driver's pace began no write of its own (CI 43626539705, ha:
    "None ms of 0 writes"), so the window is at least the hold."""
    until_ms = max(ended_ms, began_ms + HOLD_SECONDS * 1000) + 1000
    WINDOWS.append((began_ms, until_ms))
    # Each write by when it began (its answer's time less its own), never
    # by its answer alone, refused or answered alike (PR CI 45225932982: an
    # order begun before a move's window, timed out 5 s later inside it,
    # was charged to it): a write begun in the window and answered after it
    # is counted with its whole time (CI 43636763392, ha: "None ms of 0
    # writes" through a range's move -- writes answered past the window's
    # end were not counted at all). Read once the window's writes had the
    # hold to answer.
    settle = (until_ms + HOLD_SECONDS * 1000) / 1000 - time.time()
    if settle > 0:
        time.sleep(settle)
    took = sorted(int(p[3]) for p in driver_lines("/tmp/took")
                  if len(p) == 4 and p[2].isdigit() and p[3].isdigit()
                  and began_ms <= int(p[2]) - int(p[3]) <= until_ms)
    refused = [p for p in driver_lines("/tmp/refused")
               if refusal(p) and began_ms <= refusal(p)[0] <= until_ms]
    p99 = took[min(len(took) - 1, int(len(took) * 0.99))] if took else None
    s.check(bool(took) and p99 <= HOLD_SECONDS * 1000,
            f"rebalance: {what}: the driver's p99 write time while it moved within moves.holdSeconds "
            f"({p99} ms of {len(took)} writes, the longest {took[-1] if took else None} ms)")
    # Each refusal's whole words, in the job's log and the run's artifact
    # (.e2e-refused.log): the next red names its cause (CI 43626539705).
    if refused:
        store_logs_at_first_refusal("rebalance", what)
        with open(".e2e-refused.log", "a") as kept:
            kept.write(f"== {what}\n")
            for p in refused:
                kept.write(" ".join(p) + "\n")
        for p in refused:
            print(f"      refused: {' '.join(p)}", flush=True)
    s.check(not refused, f"rebalance: {what}: no write refused while it moved ({len(refused)}: "
                         f"{[' '.join(p[:3]) for p in refused[:3]]}; each in full above and in "
                         f".e2e-refused.log)")
    if p99 is not None:
        claim(s, "rebalance", what, p99 / 1000, HOLD_SECONDS,
              {"move_s": round((ended_ms - began_ms) / 1000, 2), "writes": len(took)})


def refused_between(s, what, windows):
    """The driver's writes refused between moves: begun after one of a
    phase's move windows ended and before the next began (`windows`, the
    phase's in turn), with nothing moved, upgraded or aborted meanwhile. A
    write refused there is refused in steady state, which no write may be,
    as none may within a move's window: the class fails, each named, and
    kept with the window's in .e2e-refused.log."""
    found = []
    if windows:
        first, last = windows[0][0], max(until for _, until in windows)
        for p in driver_lines("/tmp/refused"):
            said = refusal(p)
            if said and first <= said[0] <= last and not any(b <= said[0] <= u for b, u in windows):
                found.append(p)
    if found:
        store_logs_at_first_refusal("rebalance", f"refused between moves, {what}")
        with open(".e2e-refused.log", "a") as kept:
            kept.write(f"== refused between moves, {what}\n")
            for p in found:
                kept.write(" ".join(p) + "\n")
        for p in found:
            print(f"      refused between moves: {' '.join(p)}", flush=True)
    s.check(not found, f"rebalance: {what}: no write refused between moves ({len(found)}: "
                       f"{[' '.join(p[:3]) for p in found[:3]]}; each in full above and in .e2e-refused.log)")


def committed_cut(store, d, kind, name):
    """Where a thing about to move stands in database `d`, as a condition on
    its rows: a range's rows to its last sequence, a partition's entries
    (or an account's, in each partition it has been in) to its last. Each
    is written by one writer in turn under its head's lock, so every row to
    there is committed and none is added to them later.

    Never a time: `committed_at_ns` and `recorded_at_ns` are stamped as a
    write is received, before it waits its turn, and merge-queue run
    44726786526 (ha) had an order received 0.2 s before a move's check
    began and committed 4.3 s later, after the check's first read -- "7869
    and 7870", nothing changed."""
    if kind == "range":
        said = psql(f"select coalesce(max(sequence), 0) from lake_row where dataset = '{name.split('/')[0]}' "
                    f"and coalesce(partition, dataset) = '{name}'", pod=database_pod(store, d))
        return f"sequence <= {int(said)}"
    whose = f"partition = '{name}'" if kind == "partition" else f"account_id = '{name}'"
    said = psql(f"select partition, max(last_sequence) from book_entry where {whose} "
                f"group by partition order by partition", pod=database_pod(store, d))
    stood = [line.rsplit("|", 1) for line in said.splitlines()]
    return "(" + " or ".join(f"(partition = '{partition}' and first_sequence <= {int(last)})"
                             for partition, last in stood) + ")" if stood else "false"


def committed_before(store, d, kind, name, cut):
    """What was committed of a moved thing to `cut` (committed_cut, read
    before its move), in database `d`: how many, and a digest of them, each
    by its key and place."""
    if kind == "range":
        dataset = name.split("/")[0]
        sql = (f"select count(*), coalesce(md5(string_agg(row_key || ':' || sequence || ':' || version, ',' "
               f"order by sequence, row_key)), '') from lake_row where dataset = '{dataset}' "
               f"and coalesce(partition, dataset) = '{name}' and {cut}")
    elif kind == "partition":
        sql = (f"select count(*), coalesce(md5(string_agg(entry_id || ':' || encode(digest, 'hex'), ',' "
               f"order by first_sequence)), '') from book_entry where partition = '{name}' "
               f"and {cut}")
    else:
        sql = (f"select count(*), coalesce(md5(string_agg(entry_id || ':' || encode(digest, 'hex'), ',' "
               f"order by partition, first_sequence)), '') from book_entry where account_id = '{name}' "
               f"and {cut}")
    return psql(sql, pod=database_pod(store, d))


def no_gap(store, d, kind, name):
    """The moved thing's change log whole where it is written now: each
    sequence once, none missing."""
    if kind == "range":
        sql = (f"select count(distinct sequence) - (max(sequence) - min(sequence) + 1) from lake_row "
               f"where dataset = '{name.split('/')[0]}' and coalesce(partition, dataset) = '{name}'")
    else:
        sql = (f"with c as (select first_sequence f, last_sequence l, lag(last_sequence) over "
               f"(order by first_sequence) p from book_entry where partition = '{name}') "
               f"select count(*) filter (where p is not null and f <> p + 1) from c")
    said = psql(sql, pod=database_pod(store, d))
    return said in ("0", "")


def varint_field(record, number):
    """A protobuf message's varint field, by number: the first, or None."""
    at = 0

    def varint():
        nonlocal at
        shift = value = 0
        while True:
            byte = record[at]
            at += 1
            value |= (byte & 0x7F) << shift
            if not byte & 0x80:
                return value
            shift += 7
    while at < len(record):
        key = varint()
        field, wire = key >> 3, key & 7
        if wire == 0:
            value = varint()
            if field == number:
                return value
        elif wire == 1:
            at += 8
        elif wire == 5:
            at += 4
        elif wire == 2:
            # The length read first: `at += varint()` would add it to
            # `at` as it was before the length's own bytes were read.
            length = varint()
            at += length
        else:
            return None
    return None


def lake_records(kind, partition, since_ns):
    """The lake's control records of `kind` naming `partition`, kept since:
    each record's bytes."""
    said = psql(f"select encode(body, 'hex') from lake_control_change where kind = '{kind}' "
                f"and committed_at_ns >= {since_ns} "
                f"and position(convert_to('{partition}', 'UTF8') in body) > 0 order by sequence")
    return [bytes.fromhex(line) for line in said.split()]


def book_records(key, since_ns):
    """The book's `~control` entries whose key begins `key`, kept since."""
    said = psql(f"select idempotency_key from book_entry where account_id = '~control' "
                f"and idempotency_key like '{key}%' and committed_at_ns >= {since_ns}")
    return said.split()


def partition_moved(store, partition, since_ns, seconds):
    """Whether `partition`'s move ended within `seconds`: 'moved' with its
    PartitionMoved, 'aborted' with its PartitionMoveAborted, else None."""
    def ended():
        if store == "lake":
            if lake_records("meridian.v1.PartitionMoved", partition, since_ns):
                return "moved"
            if lake_records("meridian.v1.PartitionMoveAborted", partition, since_ns):
                return "aborted"
        else:
            if book_records(f"control-record:partition-moved:{partition}:", since_ns):
                return "moved"
            if book_records(f"control-record:partition-move-aborted:{partition}:", since_ns):
                return "aborted"
        return None
    started = time.time()
    while time.time() - started < seconds:
        try:
            said = ended()
        except SystemExit:
            said = None
        if said:
            return said
        time.sleep(1)
    return None


def rendezvous(partition, count=2):
    """The group of `count` a partition is its own by, unassigned
    (meridian_changes::groups::owner): the highest weight, the first eight
    bytes of SHA-256 over `<partition>/<group>`, the lower group on a tie."""
    import hashlib
    return max(range(count), key=lambda g: (
        int.from_bytes(hashlib.sha256(f"{partition}/{g}".encode()).digest()[:8], "big"), -g))


def book_owner(partition):
    """The group its head names, else its own by rendezvous."""
    said = psql(f"select coalesce(writer_group, -1) from book_partition_head where partition = '{partition}'")
    named = int(said) if said.lstrip("-").isdigit() else -1
    return named if named >= 0 else rendezvous(partition)


def lake_owner(partition):
    """The group its row names, else its dataset's by rendezvous."""
    said = psql(f"select coalesce(writer_group, -1) from lake_partition where dataset = '{partition}'")
    named = int(said) if said.lstrip("-").isdigit() else -1
    return named if named >= 0 else rendezvous(partition.split("/")[0])


def book_partitions():
    """The book's partitions after the class's upgrade, each whether or not
    it has been written yet: a partition's head is made by its first write,
    and in ha the heads named P0 alone (CI 43621875619)."""
    return [f"P{i}" for i in range(BOOK_PARTITIONS)]


# How long a move is given to end, moved or aborted, from its request.
MOVE_ENDS_S = 120


def a_move(s, dash, store, kind, name, target, where, by_dialog=False):
    """One move under the driver's load, by her tool (or her Move dialog):
    answered at once, ended moved within switchSeconds, the driver's writes
    held to the hold, nothing committed before it changed, its change log
    whole where it is written now. `where` is (source database, target
    database). Says whether it moved."""
    what = f"{store} {kind} {name} to {'group ' + str(target) if kind != 'account' else target}, {where_said(where)}"
    since_ns = time.time_ns()
    time.sleep(1)
    cut = committed_cut(store, where[0], kind, name)
    before = committed_before(store, where[0], kind, name, cut)
    began_ms = int(time.time() * 1000)
    if kind == "account":
        answer = dash.call("dashboard__move_account", {
            "account_id": name, "target_partition": target, "note": "rebalance: an account moved under load"})
        moved = answer.get("outcome") == "made"
        s.check(moved, f"rebalance: {what}: dashboard__move_account answered with its move ({answer})")
        ended = "moved" if moved else None
    else:
        note = f"rebalance: {kind} {name} moved under load"

        def unbegun():
            """No move of it begun by a request whose answer the dashboard's
            forward lost: none ended, moved or aborted, within the time a
            move is given to end, and the target not its owner."""
            owner = lake_owner if store == "lake" else book_owner
            return partition_moved(store, name, since_ns, MOVE_ENDS_S) is None and owner(name) != target
        if by_dialog:
            status, location = dash.move_dialog(name, target, note, unbegun)
            s.check(status == 303 and location.endswith("#partitions"),
                    f"rebalance: {what}: her Move dialog answered by the store, the move begun "
                    f"({status} to {location})")
        else:
            answer = dash.call("dashboard__move_partition",
                               {"partition": name, "target_group": target, "note": note}, again=unbegun)
            s.check(answer.get("outcome") == "made",
                    f"rebalance: {what}: dashboard__move_partition answered, the move begun ({answer})")
        ended = partition_moved(store, name, since_ns, MOVE_ENDS_S)
        s.check(ended == "moved", f"rebalance: {what}: switched, its digests equal ({ended})")
    ended_ms = int(time.time() * 1000)
    window_held(s, what, began_ms, ended_ms)
    after = committed_before(store, where[1], kind, name, cut)
    s.check(before == after and not before.startswith("0|"),
            f"rebalance: {what}: what was committed before it reads the same after it ({before} and {after})")
    if kind != "account":
        s.check(no_gap(store, where[1], kind, name), f"rebalance: {what}: its change log whole where it is written")
    return ended == "moved"


def where_said(where):
    return "within one database" if where[0] == where[1] else f"from database {where[0]} to database {where[1]}"


def injected(component, path, text):
    """A file made (text) or removed (None) in each of the component's
    group-0 members' scratch."""
    for pod in pods(component):
        name = pod["metadata"]["name"]
        if text is None:
            kubectl("exec", name, "-c", component, "--", "rm", "-f", path, check=False)
        else:
            kubectl("exec", name, "-c", component, "--", "sh", "-c", f"printf %s {text} > {path}")


def an_abort(s, dash, store, name, target, cause):
    """A move aborted by an injected lag or digest mismatch: the source still
    owning, its PartitionMoveAborted naming the cause (and a digest's
    IntegrityFinding), and the driver's writes going on."""
    component = "lake" if store == "lake" else "bor"
    path = MOVE_LAG_FILE if cause == "lag" else MOVE_DIGEST_FILE
    owner = (lake_owner if store == "lake" else book_owner)(name)
    since_ns = time.time_ns()
    injected(component, path, "60000" if cause == "lag" else "differs")
    try:
        answer = dash.call("dashboard__move_partition", {
            "partition": name, "target_group": target, "note": f"rebalance: an injected {cause}"})
        s.check(answer.get("outcome") == "made", f"rebalance: {store} {name}: the move begun ({answer})")
        ended = partition_moved(store, name, since_ns, 120)
    finally:
        injected(component, path, None)
    s.check(ended == "aborted", f"rebalance: {store} {name}: an injected {cause} aborts its move ({ended})")
    still = (lake_owner if store == "lake" else book_owner)(name)
    s.check(still == owner, f"rebalance: {store} {name}: its source still owns it after the abort "
                            f"(group {still}, before {owner})")
    code = {"lag": 1, "digest": 3}[cause]
    if store == "lake":
        records = lake_records("meridian.v1.PartitionMoveAborted", name, since_ns)
        causes = [varint_field(r, 6) for r in records]
        s.check(code in causes, f"rebalance: {store} {name}: its PartitionMoveAborted names {cause} ({causes})")
        if cause == "digest":
            found = lake_records("meridian.v1.IntegrityFinding", name, since_ns)
            s.check(bool(found), f"rebalance: {store} {name}: the digest's IntegrityFinding kept ({len(found)})")
    else:
        aborted = book_records(f"control-record:partition-move-aborted:{name}:", since_ns)
        found = book_records(f"control-record:finding:move:{name}:", since_ns)
        s.check(bool(aborted), f"rebalance: {store} {name}: its PartitionMoveAborted journalled on ~control")
        s.check(bool(found) == (cause == "digest"),
                f"rebalance: {store} {name}: an IntegrityFinding for a digest alone ({len(found)} for {cause})")
    resumed_ms = int(time.time() * 1000)
    writing = happens(lambda: sum(1 for p in driver_lines("/tmp/took")
                                  if len(p) == 4 and p[2].isdigit() and int(p[2]) > resumed_ms) >= 4, 60, every=3)
    s.check(writing, f"rebalance: {store} {name}: the driver writes on after the abort")


BOOK_PARTITIONS = 4


def rebalance(s):
    """The rebalance class (step 3d; section 15), after writer_groups."""
    print("R: rebalance: a range, a partition and an account moved under load", flush=True)
    began = time.time()
    upgraded = upgrade_kept(f"bor.partitions={BOOK_PARTITIONS}")
    s.check(upgraded.returncode == 0, "rebalance: the book at four partitions")
    # Every workload rolled out before her forward is made: the dashboard's
    # among them, handed the chart's values, which replaces its two copies
    # one at a time and is Available all through (PR CI 44604574060: her
    # forward landed on the last old pod, which the rollout then ended
    # under her Move dialog's post).
    upgrade_rolled_out(s, "rebalance: the book at four partitions")
    wait_for("every component ready", all_ready, seconds=600)
    dash = Dashboard()
    try:
        dash.connect()
        dash.sign_in()
        moves(s, dash)
    finally:
        dash.close()
    print("R: rebalance held", flush=True)
    figure("rebalance: the class, start to end", took_s=round(time.time() - began))


def moves(s, dash):
    dataset = "drive-data:drive"
    # The driver's dataset halved, its rows (one instrument's) in one half:
    # that half is what moves under load.
    answer = dash.call("dashboard__split_partition", {"partition": dataset, "note": "rebalance: halves to move"})
    s.check(answer.get("outcome") == "made", f"rebalance: {dataset} split by dashboard__split_partition ({answer})")
    halves = [f"{dataset}/0000-7fff", f"{dataset}/8000-ffff"]
    loaded = ""

    def newest_half():
        nonlocal loaded
        loaded = psql(f"select partition from lake_row where dataset = '{dataset}' and partition is not null "
                      "order by sequence desc limit 1")
        return loaded in halves
    s.check(happens(newest_half, 120), f"rebalance: the driver writes into a half ({loaded})")
    if loaded not in halves:
        return
    accounts = [a for a in DRIVER_ACCOUNTS if psql(
        f"select partition from book_account where account_id = '{a}'") == "P0"]
    owners = {p: str(book_owner(p)) for p in book_partitions()}
    # Within one database: the driver's half and the book's P0 to the other
    # group; an account to a partition of the group P0 left.
    half_from = lake_owner(loaded)
    p0_from = int(owners.get("P0", "-1"))
    s.check(half_from in (0, 1) and p0_from in (0, 1),
            f"rebalance: the driver's half and P0 each held by a group ({half_from} and {p0_from})")
    if half_from not in (0, 1) or p0_from not in (0, 1) or len(accounts) < 2:
        return
    phase = len(WINDOWS)
    a_move(s, dash, "lake", "range", loaded, 1 - half_from, (0, 0))
    a_move(s, dash, "book", "partition", "P0", 1 - p0_from, (0, 0), by_dialog=True)
    others = sorted(p for p, g in owners.items() if p != "P0" and int(g) == p0_from) or \
        sorted(p for p in owners if p != "P0")
    a_move(s, dash, "book", "account", accounts[0], others[0], (0, 0))
    refused_between(s, "within one database", WINDOWS[phase:])
    # Across two databases: each store's second database brought by an
    # upgrade, group 1 writing to it; its partitions moved there by the
    # count's change; then the driver's half, P0 and an account moved from
    # group 0's database to group 1's.
    upgraded = upgrade_kept("lake.databases=2", "bor.databases=2")
    s.check(upgraded.returncode == 0, "rebalance: the lake and the book at two databases")
    if upgraded.returncode != 0:
        return
    for component in ("lake", "bor"):
        rolled = run("kubectl", "--namespace", NAMESPACE, "rollout", "status",
                     f"statefulset/{FULL}-{component}-database-1", "--timeout=420s", check=False)
        s.check(rolled.returncode == 0, f"rebalance: {component}'s database 1 brought and ready")
    # Each store's members rolled to the new count before anything is timed:
    # a partition is moved home only once every member seen runs the
    # sweep's revision (`lake_member`, `book_member`), so the moves home
    # begin when the rollout ends, and the rollout's own writer changes are
    # an upgrade's, not a move's. Timed from "every component ready" alone,
    # the window to the moves home took in the rest of the roll (PR CI
    # 43675491409, ha: four orders timed out at 04:43:28 and 04:43:51, as
    # group 1's and then group 0's book writer changed hands; the first
    # partition moved home at 04:44:52).
    for component in ("lake", "bor"):
        for deployment in (f"{FULL}-{component}", f"{FULL}-{component}-group-1"):
            rolled = run("kubectl", "--namespace", NAMESPACE, "rollout", "status", f"deployment/{deployment}",
                         "--timeout=420s", check=False)
            s.check(rolled.returncode == 0, f"rebalance: {deployment} rolled out at two databases")
        # Its readers too, where the profile has any.
        run("kubectl", "--namespace", NAMESPACE, "rollout", "status", f"deployment/{FULL}-{component}-reader",
            "--timeout=420s", check=False)
    ready = happens(all_ready, 600, every=5)
    s.check(ready, "rebalance: every component ready at two databases")
    upgrade_rolled_out(s, "rebalance: the lake and the book at two databases")
    # The dashboard's pod was replaced too: it is handed the chart's
    # resilience values (item 34), which the databases' count changed, and
    # her port-forward ended with the old pod (PR CI 43783060111: the next
    # tool call, "Remote end closed connection without response"). Her
    # client's grant is the dashboard's record, not the pod's.
    s.check(dash.forwarded(), "rebalance: the dashboard, restarted with the chart's new values, answers her again")
    since_ms = int(time.time() * 1000)
    writing = happens(lambda: all(sum(1 for at in driver_acked().get(kind, {}).values() if at > since_ms) >= 10
                                  for kind in ("price", "order")), 300, every=3)
    s.check(writing, "rebalance: the driver's prices and orders acknowledged again at two databases")
    # The moves home are timed from here: the upgrade's own rollout (the
    # chart restarts the lake's and the book's members for the new count)
    # left both stores' public topics unserved for about 2 s after every
    # pod was ready (CI 43636763392, 20:36:43-45, the lake's and the book's
    # at once, before any move began) -- the restart's, not a move's.
    since_ms = int(time.time() * 1000)
    phase = len(WINDOWS)
    in_one = happens(lambda: int(psql("select count(*) from book_entry where account_id <> '~control'",
                                      pod=database_pod("bor", 1)) or 0) > 0, 300, every=5)
    s.check(in_one, "rebalance: group 1's book partitions moved to database 1 by the count's change")
    # A range is home once its row is in database 1, named group 1's: the
    # driver's rows are in a half group 0 holds by then, and group 1's
    # other half holds none to count (CI 43626539705).
    lake_in_one = happens(lambda: int(psql(
        "select count(*) from lake_partition where writer_group = 1 and closed_at is null",
        pod=database_pod("lake", 1)) or 0) > 0, 300, every=5)
    s.check(lake_in_one, "rebalance: group 1's lake ranges moved to database 1 by the count's change")
    # Written where each head is until moved home (ruled 2026-11-14): from
    # the driver acknowledged again at two databases until the moves home
    # are done, the driver's writes within the hold, none refused.
    window_held(s, "the count's moves home", since_ms, int(time.time() * 1000))
    if not (ready and writing):
        return
    owners = {p: str(book_owner(p)) for p in book_partitions()}
    zeros = sorted(p for p, g in owners.items() if int(g) == 0)
    ones = sorted(p for p, g in owners.items() if int(g) == 1)
    # An account of group 0's database to group 1's.
    held = [a for a in accounts[1:] if psql(f"select partition from book_account where account_id = '{a}'") in zeros]
    if held and ones:
        a_move(s, dash, "book", "account", held[0], ones[0], (0, 1))
    else:
        s.check(False, f"rebalance: an account in group 0's database to move ({held}, {ones})")
    if lake_owner(loaded) == 0:
        a_move(s, dash, "lake", "range", loaded, 1, (0, 1))
    else:
        s.check(False, f"rebalance: the driver's half in group 0's database to move ({lake_owner(loaded)})")
    moved_p0 = "P0" in zeros and a_move(s, dash, "book", "partition", "P0", 1, (0, 1))
    if "P0" not in zeros:
        s.check(False, f"rebalance: P0 in group 0's database to move ({owners})")
    refused_between(s, "across two databases", WINDOWS[phase:])
    # The aborts, from group 1's database back towards group 0's: the
    # lake's range by an injected digest mismatch, the book's P0 by an
    # injected lag (the gate held shut for moves.switchSeconds).
    if lake_owner(loaded) == 1:
        an_abort(s, dash, "lake", loaded, 0, "digest")
    if moved_p0:
        an_abort(s, dash, "bor", "P0", 0, "lag")


FAILURES = {
    "lake-writer": lambda s: kill_writer(s, "lake-writer", "lake"),
    "bor-writer": lambda s: kill_writer(s, "bor-writer", "bor"),
    "street-writer": lambda s: kill_writer(s, "street-writer", "street"),
    "instrument-writer": lambda s: kill_writer(s, "instrument-writer", "instrument"),
    "conductor-writer": lambda s: kill_writer(s, "conductor-writer", "conductor"),
    "launcher-writer": lambda s: kill_writer(s, "launcher-writer", "launcher"),
    "node-freeze": node_freeze,
    "split-brain": split_brain,
    "node": node_lost,
    "network-policy": network_policy,
    "db-primary": db_primary,
    "broker": broker_lost,
    "bit-rot": bit_rot,
    "torn-write": torn_write,
    "volume-lost": volume_lost,
    "zone": zone_lost,
    "replica-rot": replica_rot,
    "order-plugin": order_plugin,
    "memory-pressure": lambda s: flood(s, "memory-pressure", "pressure", "overloaded", "lake"),
    "noisy-plugin": lambda s: flood(s, "noisy-plugin", "noisy", "rate_limited", "sidecar"),
    "standing-coordinator": standing_coordinator,
    "apiserver": apiserver_lost,
    "write-ahead-bound": write_ahead_bound,
}


def verdict(s):
    print(flush=True)
    if s.failures:
        print(f"e2e-cluster-failover FAILED ({PROFILE}): {len(s.failures)}", flush=True)
        for failure in s.failures:
            print(f"  - {failure}", flush=True)
        return 1
    print(f"e2e-cluster-failover OK ({PROFILE})", flush=True)
    return 0


# ── write-scale (step 3d, stage T; the resilience spec's section 15) ──────
#
# A step of e2e-cluster-scaled: the lake and the book each written at
# saturation, group commit on, in three shapes -- one writer group on one
# database (as installed), two groups on one database, two groups on two --
# and what each commits a second read from the stores' own rows. It holds
# two groups on two databases to at least SCALE_AT_LEAST times one on one,
# each change log whole and in order after each shape. It prints whether
# that held and the threshold, never a figure: those are kept in
# SCALE_FIGURES for a private run to read into the claims ledger.

def scale_keys():
    """The lake's datasets' keys for the step: the first SCALE_DATASETS / 2
    of `scale<n>` that are each writer group's own of two, by the same
    rendezvous the lake places a dataset by."""
    mine = {0: [], 1: []}
    n = 0
    while any(len(keys) < SCALE_DATASETS // 2 for keys in mine.values()):
        key = f"scale{n}"
        group = rendezvous(f"drive-data:{key}")
        if len(mine[group]) < SCALE_DATASETS // 2:
            mine[group].append(key)
        n += 1
    return sorted(mine[0] + mine[1])


def scale_databases(component):
    """The pods to read a store's rows in: its first database's primary and
    each one's the chart brings past it."""
    return [primary()] + [database_pod(component, d) for c, d, _, _ in extra_databases() if c == component]


def scale_committed(component, since_ns, until_ns):
    """What the step's load committed to a store between two times, by the
    store's own clock on each row: in each of its databases, by partition.

    A move between databases copies a partition's rows as they are, their
    times kept, and leaves the source's in place: summed over databases, a
    partition moved since `since_ns` was counted in both (stage T's review,
    finding 2). So each partition is counted once -- where it is held most
    whole -- and each partition counted in two databases is named: a move
    ran in the window. Answers (commits counted once, each database's own
    count, the partitions counted in more than one)."""
    if component == "lake":
        sql = ("select dataset || '|' || count(*) from lake_row where dataset like 'drive-data:scale%' "
               f"and recorded_at_ns >= {since_ns} and recorded_at_ns < {until_ns} group by dataset")
    else:
        sql = ("select partition || '|' || count(*) from book_entry where account_id like 'ACC-DRIVE-%' "
               f"and committed_at_ns >= {since_ns} and committed_at_ns < {until_ns} group by partition")
    by_database = []
    for pod in scale_databases(component):
        by_database.append({line.rsplit("|", 1)[0]: int(line.rsplit("|", 1)[1])
                            for line in psql(sql, pod=pod).splitlines()})
    return counted_once(by_database)


def counted_once(by_database):
    """Each partition's commits counted once over a store's databases:
    (the sum of each partition's greatest count in any database, each
    database's own sum, the partitions counted in more than one)."""
    once, twice = {}, set()
    for counts in by_database:
        for name, count in counts.items():
            if name in once:
                twice.add(name)
            once[name] = max(once.get(name, 0), count)
    return sum(once.values()), [sum(counts.values()) for counts in by_database], sorted(twice)


def scale_told(writing, accounts=()):
    """The step's instances told to write -- the oms instance into
    `accounts` where any are named -- or to stop."""
    for instance in SCALE_DRIVERS:
        pod = driver_pod(instance)
        if not pod:
            raise SystemExit(f"{instance} has no pod")
        kubectl("exec", pod, "-c", "plugin", "--", "sh", "-c",
                f"printf %s '{','.join(accounts)}' > {SCALE_FILE}" if writing else f"rm -f {SCALE_FILE}")


def scale_accounts(s):
    """As many of the driver's accounts of each writer group, of two: each
    account placed by a first order, its partition read from the book, and
    its partition's group by the rendezvous the book places a partition by.
    Written evenly over them, two groups each take half the load; over all
    of them the busier group would bound the whole."""
    scale_told(True)
    try:
        placed = {}

        def all_placed():
            said = psql("select account_id || ' ' || partition from book_account "
                        "where account_id like 'ACC-DRIVE-%'")
            placed.update(line.split() for line in said.splitlines() if len(line.split()) == 2)
            return len(placed) >= len(DRIVER_ACCOUNTS)
        happens(all_placed, 120, every=3)
    finally:
        scale_told(False)
    time.sleep(12)
    by_group = {0: [], 1: []}
    for account, partition in sorted(placed.items()):
        by_group[rendezvous(partition)].append(account)
    even = min(len(by_group[0]), len(by_group[1]))
    s.check(even > 0, f"write-scale: the driver's accounts fall to both writer groups of two "
            f"({len(by_group[0])} and {len(by_group[1])} of {len(placed)} placed)")
    return by_group[0][:even] + by_group[1][:even]


def logs_whole(s, what):
    """Each store's change log whole in each database past the first too
    (properties reads the first): no sequence of a partition missing or
    taken twice."""
    properties(s, what)
    for component, table, partition, first, last in (("bor", "book_entry", "partition", "first_sequence",
                                                      "last_sequence"),
                                                     ("lake", "lake_row", "dataset", "sequence", "sequence")):
        for pod in scale_databases(component)[1:]:
            said = psql(
                f"with d as (select distinct {partition} as p, {first} as f, {last} as l from {table}), "
                f"c as (select p, f, lag(l) over (partition by p order by f, l) as prev_l from d) "
                f"select count(*) filter (where prev_l is not null and f <> prev_l + 1) from c", pod=pod)
            s.check(said in ("0", ""), f"{what}: {component}'s change logs whole in {pod} "
                    f"({said or 0} sequence(s) missing or twice)")


def scale_shape(s, shape, figures, accounts):
    """One shape written at saturation: warmed, then measured for
    SCALE_MEASURED_S, each store's commits a second kept in `figures` and
    never said; its instances stopped, and each change log whole after."""
    print(f"W: write-scale: {shape}", flush=True)
    scale_told(True, accounts)
    try:
        time.sleep(SCALE_WARM_S)
        since_ns = time.time_ns()
        time.sleep(SCALE_MEASURED_S)
        until_ns = time.time_ns()
    finally:
        scale_told(False)
    # Every write in flight answered before the rows are counted.
    time.sleep(12)
    seconds = (until_ns - since_ns) / 1e9
    for component in ("lake", "bor"):
        name = "lake" if component == "lake" else "book"
        committed, by_database, twice = scale_committed(component, since_ns, until_ns)
        figures[shape][component] = {"per_s": committed / seconds, "by_database": by_database,
                                     "counted_in_two_databases": twice, "seconds": round(seconds, 2)}
        s.check(committed > 0, f"write-scale: {shape}: the {name} was written through the measured window")
        # No move ran in the window: a partition's rows of the window are in
        # one database alone, and none of group 1's is on its way after it.
        s.check(not twice, f"write-scale: {shape}: no partition of the {name} moved between databases while it "
                f"was measured ({len(twice)} counted in two databases)")
        if len(by_database) > 1:
            away = away_from_home(component)
            s.check(away == 0, f"write-scale: {shape}: every one of group 1's partitions of the {name} still "
                    f"home in its own database after the measured window ({away} in the first)")
    for instance in SCALE_DRIVERS:
        figures[shape][instance] = {"last_refusal": driver_file(instance, "/tmp/scale-refused").strip()[:400],
                                    "counted": len(driver_file(instance, "/tmp/scale.log").splitlines())}
    logs_whole(s, f"write-scale, {shape}")


def scale_upgraded(s, what, *sets):
    """The scaled install moved to the next shape by an upgrade, every
    workload rolled out and ready; says whether it was."""
    upgraded = upgrade(*sets)
    s.check(upgraded.returncode == 0, f"write-scale: {what}: the upgrade applied"
            + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
    if upgraded.returncode != 0:
        return False
    migrated(s, f"write-scale: {what}")
    # But the sharded instance's: its standby pod is never ready, by design.
    settled = happens(lambda: rolled_out() and all_ready() and group_members_ready(2), 900, every=5)
    s.check(settled, f"write-scale: {what}: every workload rolled out and ready"
            + ("" if settled else f" ({'; '.join(rollout_left()[:4])})"))
    return settled


def write_scale(s):
    print("W: write-scale: each store written at saturation in three shapes", flush=True)
    shapes = ["one group on one database", "two groups on one database", "two groups on two databases"]
    figures = {shape: {} for shape in shapes}
    registered = happens(lambda: all(driver_file(i, "/tmp/registered") for i in SCALE_DRIVERS), 300, every=3)
    s.check(registered, "write-scale: its two instances registered, writing nothing until told")
    if not registered:
        for instance in SCALE_DRIVERS:
            said = kubectl("logs", driver_pod(instance), "-c", "plugin", "--tail=15", check=False)
            s.note(f"{instance} said: {said.stdout.decode(errors='replace')[-1500:]}")
        return
    accounts = scale_accounts(s)
    if not accounts:
        return
    try:
        scale_shape(s, shapes[0], figures, accounts)
        if not scale_upgraded(s, "two writer groups", "lake.writerGroups=2", "bor.writerGroups=2"):
            return
        scale_shape(s, shapes[1], figures, accounts)
        if not scale_upgraded(s, "two databases", "lake.databases=2", "bor.databases=2"):
            return
        for component in ("lake", "bor"):
            rolled = run("kubectl", "--namespace", NAMESPACE, "rollout", "status",
                         f"statefulset/{FULL}-{component}-database-1", "--timeout=420s", check=False)
            s.check(rolled.returncode == 0, f"write-scale: {component}'s database 1 brought and ready")
        # Group 1's partitions home in its own database before anything is
        # measured: until then both groups write to the first.
        # Every one of them (stage T's review, finding 2): one partition home
        # left the rest moving through the measured window, each move's hold
        # refusing writes and its copy counted in both databases.
        home = happens(lambda: moved_home("bor") and moved_home("lake"), 900, every=5)
        s.check(home, "write-scale: every one of group 1's partitions of each store moved to its database 1 "
                f"(still in the first: {away_from_home('bor')} of the book's, {away_from_home('lake')} of the lake's)")
        if not home:
            return
        scale_shape(s, shapes[2], figures, accounts)
        for component in ("lake", "bor"):
            name = "lake" if component == "lake" else "book"
            one = figures[shapes[0]][component]["per_s"]
            two = figures[shapes[2]][component]
            # Both databases written in the window: else the figure is one
            # database's, whatever it is.
            s.check(len(two["by_database"]) == 2 and all(n > 0 for n in two["by_database"]),
                    f"write-scale: the {name}'s two groups each committed to their own database")
            # The ratio, measured off CI (PO, item 39): computed and kept,
            # held or not, for the private cloud run; never asserted here.
            figures[f"{name}_held"] = one > 0 and two["per_s"] >= SCALE_AT_LEAST * one
            s.note(f"write-scale: the {name}'s ratio of two writer groups on two databases to one on one is "
                   f"measured off CI, on the private benchmark machine, against at least {SCALE_AT_LEAST}; "
                   f"this run's is kept in {SCALE_FIGURES}, never printed")
        # Every shape ran to its end: each store measured in each.
        s.check(all(component in figures[shape] for shape in shapes for component in ("lake", "bor")),
                "write-scale: the run completed, each store measured in all three shapes")
    finally:
        figures["threshold"] = SCALE_AT_LEAST
        figures["run"] = os.environ.get("GITHUB_RUN_ID", "local")
        figures["kubernetes"] = kubernetes_version()
        with open(SCALE_FIGURES, "w", encoding="utf-8") as kept:
            json.dump(figures, kept, indent=1)


# ── Two writer groups (step 3d, stage G) ──────────────────────────────────

# The internal topics a write is handed to its owning group on (the
# resilience spec's section 6.7).
WRITES_HANDED_ON = ("platform.lake.internal.record", "platform.book.internal.command")


def handed_on_counts():
    """Each internal hand-off topic's messages, by its subscription's queue
    group, as the broker's own monitoring counts them (`/connz`, on the
    server's loopback)."""
    counts = {}
    for server in broker_servers():
        said = monitored(server, ["/connz?subs=detail&limit=4096"])
        status, text = said.get("/connz?subs=detail&limit=4096", (0, ""))
        if status != 200:
            continue
        for connection in json.loads(text).get("connections", []):
            for sub in connection.get("subscriptions_list_detail", []) or []:
                if sub.get("subject") in WRITES_HANDED_ON:
                    key = (sub["subject"], sub.get("qgroup", ""))
                    counts[key] = counts.get(key, 0) + int(sub.get("msgs", 0))
    return counts


def writer_groups(s):
    """The lake and the book at two writer groups (step 3d, stage G; the
    resilience spec's sections 6.1, 6.3 and 6.7), last, under the driver's
    load: `lake.writerGroups` and `bor.writerGroups` set to 2 by an upgrade,
    as `meridian upgrade-deployment --set` sets them; each group's writer
    serving; the driver's prices and orders acknowledged again and kept; the
    driver's dataset (`drive-data:drive`, group 1's) and its accounts' P0
    (group 1's) written by group 1, under group 1's own epoch, the flood's
    dataset (group 0's) and each store's `control` under group 0's; the
    lake's assignment recorded in its control partition; and writes handed
    to their owners on the internal topics, as the broker counts them."""
    print("G: the lake and the book at two writer groups", flush=True)
    began_ms = int(time.time() * 1000)
    upgraded = upgrade("lake.writerGroups=2", "bor.writerGroups=2")
    s.check(upgraded.returncode == 0, "writer groups: helm upgrade to two writer groups applied"
            + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
    if upgraded.returncode != 0:
        return
    for component in ("lake", "bor"):
        for deployment in (f"{FULL}-{component}", f"{FULL}-{component}-group-1"):
            rolled = run("kubectl", "--namespace", NAMESPACE, "rollout", "status", f"deployment/{deployment}",
                         "--timeout=420s", check=False)
            s.check(rolled.returncode == 0, f"writer groups: {deployment} rolled out and ready")
    upgrade_rolled_out(s, "writer groups: at two")
    groups_ok = happens(lambda: all(any(ready(p) for p in pods(c)) for c in
                                    ("lake", "lake-group-1", "bor", "bor-group-1")), 300)
    s.check(groups_ok, "writer groups: each store's group 0 and group 1 writer ready")
    since_ms = int(time.time() * 1000)
    writing = happens(lambda: all(sum(1 for at in driver_acked().get(kind, {}).values() if at > since_ms) >= 20
                                  for kind in ("price", "order")), 300, every=3)
    s.check(writing, "writer groups: the driver's prices and orders acknowledged again at two groups")
    since_ns = since_ms * 1_000_000
    # The lake: each group's datasets under its own epoch. Read in one
    # statement, and once group 1 has settled: the upgrade rolls the
    # database's members too, each new timeline has group 0's writer raise
    # every group's epoch, and group 1's writer, fenced, takes over again.
    # Between the two, group 1's row names an epoch nothing was ever written
    # under, and three reads apart straddled that takeover's commit (PR CI
    # 43684440785: "group 1 19, the dataset 20, control 19", 0 rows under
    # 19, six seconds after the roll's third timeline).
    def lake_groups():
        own = "(select epoch from lake_writer_group where writer_group = 1)"
        said = psql(
            f"select coalesce({own}::text, ''), "
            "coalesce((select epoch from lake_change_head where partition = 'control')::text, ''), "
            "coalesce((select epoch from lake_partition where dataset = 'drive-data:drive')::text, ''), "
            "coalesce((select epoch from lake_partition where dataset = 'drive-flood:drive')::text, ''), "
            "(select count(*) from lake_row where dataset = 'drive-data:drive' "
            f"and recorded_at_ns >= {since_ns} and epoch = {own})")
        return (said.strip().split("|") + [""] * 5)[:5]

    def lake_settled(said):
        lake_one, control, drive, _, by_one = said
        return bool(lake_one) and drive == lake_one and control != lake_one and by_one.isdigit() and int(by_one) > 0

    # The read that settled is the one checked, never a read after it: a
    # new timeline between the two raised every epoch again (PR CI
    # 43810170103: the book's P0 settled, then read under its next epoch,
    # 0 entries, after the database's timeline moved at 05:54:20).
    lake_one, control, drive, flood, by_one = settled_read(lake_groups, lake_settled, 60)
    s.check(lake_one and drive == lake_one and control != lake_one,
            f"writer groups: the lake's group 1 owns drive-data:drive under its own epoch "
            f"(group 1 {lake_one or 'none'}, the dataset {drive}, control {control})")
    s.check(flood == control, f"writer groups: the lake's group 0 keeps drive-flood:drive under control's epoch "
            f"({flood} and {control})")
    s.check(by_one.isdigit() and int(by_one) > 0,
            f"writer groups: the driver's prices committed by the lake's group 1 since ({by_one} rows)")
    owners = psql("select count(*) from lake_control_change where kind = 'meridian.v1.PartitionOwner'").strip()
    s.check(owners.isdigit() and int(owners) > 0,
            f"writer groups: the lake's datasets' groups recorded in its control partition ({owners} records)")
    # The book: P0 group 1's, control group 0's, each its own epoch. Read
    # in one statement and once settled, as the lake's.
    def book_groups():
        p0 = "(select epoch from book_partition_head where partition = 'P0')"
        said = psql(
            f"select coalesce({p0}::text, ''), "
            "coalesce((select epoch from book_partition_head where partition = 'control')::text, ''), "
            "(select count(*) from book_entry where account_id like 'ACC-DRIVE-%' "
            f"and committed_at_ns >= {since_ns} and partition = 'P0' and epoch = {p0})")
        return (said.strip().split("|") + [""] * 3)[:3]

    def book_settled(said):
        p0, book_control, acted = said
        return bool(p0) and bool(book_control) and p0 != book_control and acted.isdigit() and int(acted) > 0

    p0, book_control, acted = settled_read(book_groups, book_settled, 60)
    s.check(p0 and book_control and p0 != book_control,
            f"writer groups: the book's P0 (group 1's) and control (group 0's) under their own epochs "
            f"({p0} and {book_control})")
    s.check(acted.isdigit() and int(acted) > 0,
            f"writer groups: the driver's orders committed in P0 by the book's group 1 since ({acted} entries)")
    # Handed on: each internal topic's messages to its group-1 queue group.
    # In `ha` a data plugin's lake writes reach every group by the lake's
    # write-ahead stream, never by a hand-off (section 6.1): there, group
    # 1's own place in the stream, moved since, says it took them.
    counts = handed_on_counts()
    for topic in WRITES_HANDED_ON:
        if topic == "platform.lake.internal.record" and PROFILE == "ha":
            # Group 1's place, kept under the stream's identity (item 191).
            taken = psql("select count(*) from lake_write_ahead where stream like 'LAKE_WRITE_AHEAD.%.group-1' "
                         f"and committed_at_ns >= {since_ns}").strip()
            s.check(taken == "1", "writer groups: the lake's group 1 took its writes from the write-ahead stream, "
                    f"its own place in it moved since ({taken or 'none'})")
            continue
        to_one = sum(n for (subject, group), n in counts.items() if subject == topic and group.endswith(".group-1"))
        s.check(to_one > 0, f"writer groups: writes handed to group 1 on {topic} ({to_one} as the broker counts)")
    s.note(f"writer groups: the hand-offs by topic and queue group: {counts}")
    driver_rpo_held(s, "writer groups")
    print("G: two writer groups held", flush=True)
    figure("writer groups: the class, start to end", took_s=round((time.time() * 1000 - began_ms) / 1000))


def main():
    s = Scorecard()
    asked = [f for f in os.environ.get("FAILURE", "").split(",") if f]
    failures = asked or DEFAULT_FAILURES[PROFILE]
    # Rebalance (step 3d) runs after the two writer groups, last: asked for
    # alone, the writer groups first.
    rebalancing = "rebalance" in failures or (not asked and REBALANCED)
    failures = [f for f in failures if f != "rebalance"]
    unknown = [f for f in failures if f not in FAILURES]
    if unknown:
        raise SystemExit(f"no such failure class: {', '.join(unknown)}; one of {', '.join(FAILURES)}")
    if PROFILE in ("ha", "scaled", "reconfigure"):
        agents = [n for n in nodes() if "topology.kubernetes.io/zone" in n["metadata"].get("labels", {})]
        if len(agents) < 3:
            raise SystemExit("the HA profile needs three agents labelled into zones: "
                             "make e2e-cluster-k3d E2E_K3D_AGENTS=3 E2E_K3D_ZONES='zone-a zone-b zone-c'")
    print(f"A: installed in the {PROFILE} profile", flush=True)
    install(s)
    first_run(s)
    if DRIVEN:
        driver_fixtures(s)
        try:
            page_entitled(s)
        except SystemExit as stopped:
            s.check(False, f"the page's entitlement: {stopped}")
    if PROFILE == "scaled":
        # The instruments every driver names, and the shards' subjects, and
        # the shard reader entitled to the shards' dataset: written before
        # the shape is read, so the reader's standing want is the lake's
        # by the time the shards are (item 33).
        try:
            driver_fixtures(s)
            entitled(SHARD_DATASET, SHARD_READER, "each subject served once: the reader's want shared by the shards")
        except SystemExit as stopped:
            s.check(False, f"the drivers' fixtures: {stopped}")
    if TLS:
        internal_tls(s)
    if PROFILE in ("ha", "scaled"):
        shape(s)
    if PROFILE == "scaled":
        try:
            shards(s)
        except SystemExit as stopped:
            s.check(False, f"shards: {stopped}")
        try:
            served_whole(s)
        except SystemExit as stopped:
            s.check(False, f"served once: the record could not be read: {stopped}")
    properties(s, "the install")
    if PROFILE == "scaled":
        # Last: it changes the install's shape (two writer groups, then two
        # databases), which every reading above holds as installed.
        try:
            write_scale(s)
        except SystemExit as stopped:
            s.check(False, f"write-scale: {stopped}")
    if PROFILE == "ha" and DRIVEN:
        try:
            reader_held(s)
        except SystemExit as stopped:
            s.check(False, f"reader held: {stopped}")
        try:
            reader_hands_on(s)
        except SystemExit as stopped:
            s.check(False, f"reader hands on: {stopped}")
    if PROFILE == "reconfigure":
        try:
            reconfigure(s)
        except SystemExit as stopped:
            s.check(False, f"reconfigure: {stopped}")
    for failure in failures:
        began_ms = int(time.time() * 1000)
        failed_before = len(s.failures)
        try:
            FAILURES[failure](s)
        except SystemExit as stopped:
            s.check(False, f"{failure}: {stopped}")
        # What the cluster and the broker said as the class failed, before
        # the next class rolls the pods it was about.
        if len(s.failures) > failed_before:
            cluster_said(failure)
        try:
            trust_targets(s, failure, began_ms)
            rows_read_are_kept(s, failure, began_ms)
        except SystemExit as stopped:
            s.check(False, f"{failure}: the trust targets could not be read: {stopped}")
        try:
            properties(s, failure)
        except SystemExit as stopped:
            s.check(False, f"properties after {failure}: the change logs could not be read: {stopped}")
        if DRIVEN and failure not in RPO_IN_CLASS:
            try:
                driver_rpo_held(s, failure)
            except SystemExit as stopped:
                s.check(False, f"{failure}: the stores' rows could not be read against the driver's: {stopped}")
    if DRIVEN:
        try:
            driver_rpo_held(s, "the whole run", lake_may_lose=PROFILE == "ha")
        except SystemExit as stopped:
            s.check(False, f"the write driver's record could not be read: {stopped}")
        # Two writer groups (step 3d, stage G), last of the tolerant run's
        # with every class run, the driver still writing: its stores' groups
        # then stay as this leaves them.
        if (PROFILE == "tolerant" and not asked) or rebalancing:
            try:
                writer_groups(s)
            except SystemExit as stopped:
                s.check(False, f"writer groups: {stopped}")
        # Rebalance (step 3d), the two groups' partitions moved under load
        # within one database and across two: in the tolerant and ha runs.
        if rebalancing and REBALANCED:
            try:
                rebalance(s)
            except SystemExit as stopped:
                s.check(False, f"rebalance: {stopped}")
        try:
            driver_history(s)
        except SystemExit as stopped:
            s.check(False, f"the write driver's record could not be read: {stopped}")
    if PROFILE == "reconfigure":
        # Last: a restore to a time takes back what the driver wrote after
        # it, which every reading above holds kept.
        try:
            restored_to_a_time(s)
        except SystemExit as stopped:
            s.check(False, f"restore: {stopped}")
    return verdict(s)


if __name__ == "__main__":
    sys.exit(main())
