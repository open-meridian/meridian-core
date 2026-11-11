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
  per set, every stateless pod tolerating a lost node for 30 s.

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
cluster's Kubernetes. No platform: the conductor waits for an enrolment
nobody gives. No value of a secret, a key or a password is ever printed.
"""

import base64
import json
import os
import subprocess
import sys
import tempfile
import time

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
DRIVER_SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "driver.py")
DRIVER_FIXTURES = ["e2e/book/instruments.sql", "e2e/failover/driver.sql"]
DRIVER_ACCOUNTS = [f"ACC-DRIVE-{i:02d}" for i in range(12)]
DESK_ACCOUNT = "ACC-DRIVE-DESK"
DRIVER_INSTRUMENT = "LCL-01J8XQ4M7K00000000E2EAAPL"
DRIVER_CASH = "LCL-01J8XQ4M7K000000000E2EUSD"
# The run's signer, which vouches for the driver's person (a recorded order
# is a user's act): its seed in a Secret only the oms driver's container
# reads, its public half beside the dashboard's keys.
DRIVER_SIGNER = "e2e-drive-signer"
DRIVER_SIGNER_KEY_ID = "e2e-drive"
INTEROP_IMAGE = os.environ.get("E2E_INTEROP_IMAGE", "meridian-python-interop")
# The classes that read RPO through the stores themselves, and say it in
# their claims; after every other, the driver's rows are held all the same.
RPO_IN_CLASS = {"db-primary", "torn-write", "zone"}
# The oms driver's history (Jepsen's invoke/ok/fail/info), kept for Elle.
HISTORY = os.environ.get("E2E_FAILOVER_HISTORY", ".e2e-failover-history.edn")

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
    print(f"CLAIM {json.dumps(line)}", flush=True)
    with open(CLAIMS, "a", encoding="utf-8") as kept:
        kept.write(json.dumps(line) + "\n")
    if bound is None:
        s.check(True, f"{failure}: {component} in {seconds:.1f}s (no bound is set for it yet)")
    else:
        s.check(line["held"], f"{failure}: {component} answered again in {seconds:.1f}s, within {bound}s")


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
    run("helm", "install", RELEASE, CHART, "--namespace", NAMESPACE, "--create-namespace", *values)
    s.check(True, f"helm install, profile {PROFILE}")
    if PROFILE == "ha":
        # The book's reader may be held behind on purpose (reader_held).
        kubectl("set", "env", f"deployment/{FULL}-bor-reader", f"MERIDIAN_READER_HOLD_FILE={HOLD_FILE}")
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
        pin = json.dumps({"spec": {"template": {"spec": {
            "nodeSelector": {"kubernetes.io/hostname": server_node()},
            "affinity": None, "topologySpreadConstraints": None}}}})
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
        cluster_services_on_server()


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
    }
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
    wait_for("every component ready", all_ready, seconds=900)
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


# What analytics says once it has its stream, and where it could not make one.
QUEUE_MADE = "the units go by JetStream's work queue"
QUEUE_NOT_THERE = "the units' work queue is not there"


def jetstream_rolled(s):
    """The broker's servers restarted one at a time, as an upgrade rolls
    them, and JetStream still there after: each analytics copy restarted
    makes or finds its stream at three, rather than going by the queue
    group. CI 42294258528: with the stream in memory, a roll left the three
    answering "no suitable peers for placement" for twelve minutes."""
    kubectl("rollout", "restart", f"statefulset/{broker_set()}")
    rolled = kubectl("rollout", "status", f"statefulset/{broker_set()}", "--timeout=300s",
                     check=False).returncode == 0
    s.check(rolled, "broker: its servers restarted one at a time")
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
                    "DRIVER_ACCOUNTS": DESK_ACCOUNT if role == "desk" else ",".join(DRIVER_ACCOUNTS)},
        }})
        if role in ("oms", "desk"):
            sidecars[-1]["plugin"]["existingSecret"] = DRIVER_SIGNER
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
    are written), then the driver writing both kinds."""
    for path in DRIVER_FIXTURES:
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
    writing = happens(lambda: all(driver_acked().get(kind) for kind in ("price", "order")), 300, every=3)
    s.check(writing, "the write driver writing through the stores: prices to the lake, orders to the book "
            f"({', '.join(f'{k} {len(v)}' for k, v in sorted(driver_acked().items())) or 'nothing acknowledged'})")
    if not writing:
        for instance in DRIVERS:
            said = kubectl("logs", driver_pod(instance), "-c", "plugin", "--tail=15", check=False)
            s.note(f"{instance} said: {said.stdout.decode(errors='replace')[-1500:]}")


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


def driver_kept(pod=None):
    """What the stores hold of it: the book's orders in the driver's
    accounts, the lake's rows in its dataset."""
    orders = psql("select order_id from book_order where account_id like 'ACC-DRIVE-%'", pod=pod)
    placements = psql("select placement_id from book_placement where account_id like 'ACC-DRIVE-%'", pod=pod)
    prices = psql("select row_key from lake_row where dataset = 'drive-data:drive'", pod=pod)
    return {"order": set(orders.split()), "placement": set(placements.split()), "price": set(prices.split())}


# What a class lost, kept as it is found, so a later class is not charged
# with it again.
DRIVER_LOST = {"order": set(), "placement": set(), "price": set()}


def store_rpo(since_ms=0, pod=None):
    """RPO through the stores: every acknowledged order and price looked
    for in the book's and the lake's own rows (read after what was
    acknowledged, so nothing acknowledged after is counted). Says the
    acknowledgements since `since_ms`, and what is newly lost."""
    acked = driver_acked()
    kept = driver_kept(pod)
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
    s.check(not book["lost"], f"{after}: every order and placement the book acknowledged is in it "
            f"({len(book['lost'])} lost{': ' + ', '.join(book['lost'][:5]) if book['lost'] else ''})")
    if lake_may_lose:
        s.note(f"{after}: the lake lost {len(lake['lost'])} acknowledged row(s), within {lake['lost_s']}s "
               "(its commits are local)")
    else:
        s.check(not lake["lost"], f"{after}: every price the lake acknowledged is in it "
                f"({len(lake['lost'])} lost{': ' + ', '.join(lake['lost'][:5]) if lake['lost'] else ''})")
    return rpo


def acks_again(began_ms, seconds=90):
    """The driver acknowledged by both stores again after `began_ms`."""
    return happens(lambda: all(any(at > began_ms for at in driver_acked().get(kind, {}).values())
                               for kind in ("price", "order")), seconds, every=2)


def driver_history(s):
    """The driver stopped, within its calls' bound, and the oms instance's
    history kept for Elle (the ha failover CI job checks it)."""
    for instance in DRIVERS:
        pod = driver_pod(instance)
        if pod:
            kubectl("exec", pod, "-c", "plugin", "--", "touch", "/tmp/stop", check=False)
    stopped = happens(lambda: all("DONE" in driver_file(i, "/tmp/done") for i in DRIVERS), 60, every=2)
    s.check(stopped, "the write driver stopped when told, within a minute")
    history = driver_file("drive-oms", "/tmp/history.edn")
    with open(HISTORY, "w", encoding="utf-8") as kept:
        kept.write(history)
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
    s.check(detected, f"order-plugin: the pod not ready, found in {detect_s:.1f}s")
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
            f"order-plugin: detected within {ORDER_PLUGIN_DETECT}s ({detect_s:.1f}s)")
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


def cut_rules(pod_name, targets, verb):
    """Drop every packet between a pod and each target, on the pod's node,
    first in FORWARD: established connections too, which a NetworkPolicy
    (kube-router's, as most) lets carry on.

    Every rule in one exec on the node, so the cut starts (and ends) at
    once: a docker exec per rule, some thirty for the broker class, spent
    four to six seconds of the measured bound putting the cut in place and
    taking it out (broker 9.46 s of 10 in a local run)."""
    node, ip = node_of(pod_name), pod_ip(pod_name)
    at = "1 " if verb == "-I" else ""
    rules = [f"iptables {verb} FORWARD {at}-s {a} -d {b} -j DROP"
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
       -c "insert into failover_marks (at) values (clock_timestamp()) returning id" 2>/dev/null) \
    && echo "ACKED $id"
  sleep 0.05
done
"""


def marks_table():
    psql("create table if not exists failover_marks (id bigserial primary key, at timestamptz not null)")


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
        psql("insert into failover_marks (at) values (clock_timestamp())", pod=primary(avoid))
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
        psql("insert into failover_marks (at) values (clock_timestamp())")
        acked_before = int(psql("select max(id) from failover_marks") or 0)
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
        kept = {int(i) for i in psql("select id from failover_marks", pod=new_primary).split()}
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
    OUTPUT), the API server and the kubelet included."""
    node, ip = node_of(pod_name), pod_ip(pod_name)
    for chain, rule in (("FORWARD", ["-s", ip]), ("FORWARD", ["-d", ip]),
                        ("INPUT", ["-s", ip]), ("OUTPUT", ["-d", ip])):
        run("docker", "exec", node, "iptables", verb, chain, *(["1"] if verb == "-I" else []),
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


def settled_everywhere():
    if not all_ready():
        return False
    if not all(members_settled(c) for c in STORES):
        return False
    database = pods("database")
    if len(database) != database_members() or not all(ready(p) for p in database):
        return False
    servers = [p for p in pods("broker") if p["metadata"]["labels"].get("meridian.dev/broker-set") == broker_set()]
    return len(servers) == (3 if SHAPE == "ha" else 1) and all(ready(p) for p in servers) \
        and len(pods("broker")) == len(servers)


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
    return run("helm", "upgrade", RELEASE, CHART, "--namespace", NAMESPACE, "--reset-then-reuse-values",
               "--timeout", "15m", *helm_takes_counts(), *values, check=False)


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
    s.check(settled, f"internal TLS on: every member ready, {time.time() - began:.0f}s after the upgrade")
    if not settled:
        raise SystemExit("the deployment did not settle with internal TLS on")
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
    before it says an upgrade is done (meridian-cli, upgrade_deployment.rs):
    its template seen by its controller, every replica on it and ready,
    and nothing old left -- a StatefulSet's current revision its update's."""
    left = []
    for kind in ("deployments", "statefulsets"):
        items = json.loads(out("get", kind, "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]
        for item in items:
            name, spec, status = item["metadata"]["name"], item["spec"], item.get("status", {})
            wanted = spec.get("replicas", 1)
            if item["metadata"].get("generation", 0) > status.get("observedGeneration", 0):
                left.append(f"{name}: its new template not yet seen")
            elif status.get("updatedReplicas", 0) < wanted:
                left.append(f"{name}: {status.get('updatedReplicas', 0)} of {wanted} on the new template")
            elif status.get("readyReplicas", 0) < wanted:
                left.append(f"{name}: {status.get('readyReplicas', 0)} of {wanted} ready")
            elif kind == "deployments" and (status.get("replicas", 0) > wanted
                                            or status.get("availableReplicas", 0) < wanted):
                left.append(f"{name}: old pods still being replaced")
            elif kind == "statefulsets" and spec.get("updateStrategy", {}).get("type") != "OnDelete" \
                    and status.get("currentRevision") != status.get("updateRevision"):
                left.append(f"{name}: its update not finished")
    return left


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


# How much of each container's log a failed move keeps: the Makefile's
# E2E_K3D_LOG_LINES, which .e2e-cluster.log keeps at a run's end.
SAID_LINES = os.environ.get("E2E_K3D_LOG_LINES", "400")
SAID_KEPT = []


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


def reconfigure(s):
    global SHAPE
    failed_before = len(s.failures)
    tls_turned_on(s)
    archived_in_tolerant(s)
    if len(s.failures) > failed_before:
        cluster_said("reconfigure's internal TLS and archive")
    print("R: the live install moved between the profiles, under the write driver's load", flush=True)
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
            began = time.time()
            upgraded = upgrade(f"resilience.profile={target}")
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
                    print(f"R: the first upgrade rolled out, {time.time() - began:.0f}s after it; the second upgrade",
                          flush=True)
                    member_now("before the second upgrade")
                    with HookLog("preflight") as preflight:
                        again = upgrade(f"resilience.profile={target}")
                    print(f"R: the second upgrade's preflight said, {time.time() - began:.0f}s after the first "
                          f"upgrade:\n{preflight.said or '(nothing read)'}", flush=True)
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
            s.check(settled, f"reconfigure: every member in the {target} shape and ready, {took:.0f}s after the upgrade")
            if not settled:
                for component in STORES:
                    explain(s, f"reconfigure to {target}", component)
                return
            connections_held(s, f"reconfigure to {target}")
            extra = {"from": "tolerant" if target == "ha" else "ha"}
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
            stopped = False
        finally:
            # What every pod and the namespace said at the move a check
            # failed in, kept now: the moves after it replace every member,
            # so the run's own record at its end no longer holds them (PR CI
            # 43022080248: the first move to ha failed once, its pods' logs
            # gone by then).
            if stopped or len(s.failures) > failed_before:
                cluster_said(f"reconfigure to {target}")


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


def stopped_everything():
    """Every component and the database stopped, as `meridian restore
    --to-time` stops them: their counts, to start them again."""
    running = {d["metadata"]["name"]: d["spec"].get("replicas", 1) for d in json.loads(out(
        "get", "deployments", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]}
    for name in running:
        kubectl("scale", "deployment", name, "--replicas=0")
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=0")
    # Their pods gone: each Deployment's (a ReplicaSet's, by its template
    # hash) and the database's members.
    wait_for("every component and the database to stop", lambda: not [
        p for p in json.loads(out("get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]
        if "pod-template-hash" in p["metadata"].get("labels", {})
        or p["metadata"].get("labels", {}).get("meridian.dev/component") == "database"], seconds=300)
    return running


def offline_act(act, **env):
    """A Job from the database's offline CronJob with the act named, as the
    CLI makes it (meridian-cli, offline.rs), run to its end: whether it
    succeeded, and what it said."""
    cron = json.loads(out("get", "cronjob", f"{FULL}-database-offline", "-o", "json"))
    template = cron["spec"]["jobTemplate"]
    name = f"{FULL}-database-{act}"[:63]
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
    s.check(settled, f"restore: every member ready with the archive on, {time.time() - began:.0f}s after the upgrades")
    if not settled:
        raise SystemExit("the deployment did not settle with the archive on")
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
    in the act's own words and Postgres's, the volume left restorable; the
    restore run; the standbys' volumes deleted and Patroni's record of the
    old primary cleared; everything started; and the rows as they stood at
    the time."""
    print("R: a restore to a time in ha from a base backup taken in tolerant, under Patroni with internal TLS on",
          flush=True)
    s.check(SHAPE == "ha" and under_patroni(), f"restore: the database under Patroni in ha ({SHAPE})")
    members = database_members()
    psql(f"create database {POINT}", database="postgres")
    psql("create table point (n int primary key, said text not null)", database=POINT)
    for n in (1, 2, 3):
        psql(f"insert into point values ({n}, 'before')", database=POINT)
    time.sleep(1.5)
    to = psql("select to_char(clock_timestamp() at time zone 'UTC', 'YYYY-MM-DD HH24:MI:SS.US') || '+00'",
              database="postgres")
    time.sleep(1.5)
    for n in (4, 5, 6):
        psql(f"insert into point values ({n}, 'after')", database=POINT)
    past = psql("select to_char((clock_timestamp() + interval '1 day') at time zone 'UTC', "
                "'YYYY-MM-DD HH24:MI:SS.US') || '+00'", database="postgres")
    wal = psql("select pg_walfile_name(pg_switch_wal())", database="postgres")
    wait_for("the last segment to be archived", lambda: psql(
        "select last_archived_wal from pg_stat_archiver", database="postgres") >= wal, seconds=180)
    running = stopped_everything()
    # Past the archive's end: the act fails in its own words, with
    # Postgres's last lines, and leaves the volume restorable.
    ok, log = offline_act("restore-to-time", MERIDIAN_RESTORE_TO_TIME=past,
                          MERIDIAN_DATABASE_START_SECONDS=RESTORE_START_SECONDS)
    s.check(not ok and "is outside the archive" in log and "Postgres's log, its last lines:" in log,
            f"restore: a time past the archive, {past}, refused as outside it, with Postgres's last lines")
    ok, log = offline_act("restore-to-time", MERIDIAN_RESTORE_TO_TIME=to,
                          MERIDIAN_DATABASE_START_SECONDS=RESTORE_START_SECONDS)
    s.check(ok and "promoted at" in log, f"restore: the offline Job restored the database to {to}, replaying "
            "ha's raise of its settings past the base backup's, and starting it without the certificates "
            "Patroni's configuration names")
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
    wait_for("the restored database's primary", lambda: psql(
        "select not pg_is_in_recovery()", database="postgres") == "t", seconds=300)
    for name, replicas in running.items():
        kubectl("scale", "deployment", name, f"--replicas={replicas}")
    s.check(happens(settled_everywhere, 900, every=5),
            f"restore: every component ready again, and the database's {members} members")
    rows = psql("select string_agg(n || ':' || said, ',' order by n) from point", database=POINT)
    s.check(rows == "1:before,2:before,3:before", f"restore: the rows as they stood at the time ({rows})")
    timeline = psql("select timeline_id from pg_control_checkpoint()", database="postgres")
    s.check(timeline not in ("", "1"), f"restore: on a timeline of its own ({timeline})")
    internal_tls(s)


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
    # Let go: the next page heard is a gap, read from the writer; the reads
    # after it are its own again.
    time.sleep(5)
    settled = reader_log(reader, FROM_PRIMARY, since="1m")
    time.sleep(15)
    s.check(reader_log(reader, FROM_PRIMARY, since="1m") <= settled,
            "reader held: let go, it caught up and answers from its own record again")
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
    group), hands each to the writer on platform.lake.internal.read-from-
    writer and answers its reply, so every read is answered the price served
    for it, whichever member took it (the spec's section 5, approved
    2026-11-10)."""
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


def cut_api(verb):
    """Every pod of the release cut from the API server, both ways, on its
    node: routed through the node (FORWARD), and to the node itself where
    the API server runs there (INPUT). One exec per node, so the cut starts
    and ends at once."""
    api = api_addresses()
    by_node = {}
    for pod in json.loads(out("get", "pods", "-o", "json"))["items"]:
        ip, node = pod["status"].get("podIP"), pod["spec"].get("nodeName")
        if ip and node and not pod["metadata"].get("deletionTimestamp"):
            by_node.setdefault(node, set()).add(ip)
    at = "1 " if verb == "-I" else ""
    for node, ips in by_node.items():
        rules = []
        for ip in sorted(ips):
            for target in api:
                rules += [f"iptables {verb} FORWARD {at}-s {ip} -d {target} -j DROP",
                          f"iptables {verb} FORWARD {at}-s {target} -d {ip} -j DROP",
                          f"iptables {verb} INPUT {at}-s {ip} -d {target} -j DROP"]
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
        parts = line.split("|")
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
            f"yet known ({len(first)} gap(s)"
            + (f", recorded {at_once:.1f}s after the writer was let go)" if at_once is not None else ")"))
    found = happens(lambda: gaps_of(dataset, since), 180, every=3)
    took = time.time() - let_go
    gaps = gaps_of(dataset, since)
    s.check(found, f"write-ahead-bound: the gap the stream left in {dataset} recorded "
            f"({len(gaps)} gap(s), {took:.1f}s after the writer was let go)")
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
    ended_ms = int(time.time() * 1000)
    reads = page_reads(began_ms)
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
            f"{empty} empty, {failed} failed and left the page as drawn; longest without a fresh answer "
            f"{longest / 1000:.1f} s)")
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
            f"{live_while_stale} shown live, at most {late:.1f} s late)")
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


def page_entitled(s):
    """The data driver entitled to read its own dataset back, as a deployment
    admin entitles a reader (W10.1): its DatasetEntitlement recorded where
    the conductor keeps it, published whole within a minute. Once a run."""
    now = time.time_ns()
    dataset, instance = "drive-data:drive", "drive-data"
    why = "the buy-side trust targets: the page reads the driver's price back"
    entitlement = (text_field(1, dataset) + text_field(2, instance) + varint(3 << 3) + varint(1)
                   + text_field(5, ADMIN) + varint(6 << 3) + varint(now) + text_field(9, why))
    psql("INSERT INTO config_dataset_entitlement_change (dataset, instance, entitlement, changed_at_ns) "
         f"VALUES ('{dataset}', '{instance}', decode('{entitlement.hex()}', 'hex'), {now})")
    reading = happens(lambda: any(r[1] == "ok" for r in page_reads(0)), 240, every=5)
    s.check(reading, "the page reads the driver's price back")


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


def main():
    s = Scorecard()
    asked = [f for f in os.environ.get("FAILURE", "").split(",") if f]
    failures = asked or DEFAULT_FAILURES[PROFILE]
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
    if TLS:
        internal_tls(s)
    if PROFILE in ("ha", "scaled"):
        shape(s)
    properties(s, "the install")
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
        try:
            FAILURES[failure](s)
        except SystemExit as stopped:
            s.check(False, f"{failure}: {stopped}")
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
