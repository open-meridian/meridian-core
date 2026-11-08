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
once; every store answers again from another zone within the zone tier).

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
TLS = PROFILE == "ha"

# The live install moved between the profiles, in order (e2e-cluster-
# reconfigure): up to ha, and back.
RECONFIGURE = ["ha", "tolerant"]

DEFAULT_FAILURES = {
    "tolerant": ["lake-writer", "bor-writer", "network-policy", "db-primary", "broker",
                 "bit-rot", "torn-write"],
    "ha": ["lake-writer", "bor-writer", "street-writer", "instrument-writer",
           "split-brain", "node-freeze", "node", "zone", "network-policy", "db-primary", "broker",
           "bit-rot", "torn-write", "volume-lost", "replica-rot"],
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
    return 3 if SHAPE in ("ha", "scaled") else 1


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
        if TLS:
            values += ["--set", "tls.internal.enabled=true"]
        values += ["--set", "zones={" + ",".join(ZONES) + "}"]
        values += ["--set", f"database.brought.patroniImage={POSTGRES_IMAGE}"]
    run("helm", "install", RELEASE, CHART, "--namespace", NAMESPACE, "--create-namespace", *values)
    s.check(True, f"helm install, profile {PROFILE}")
    if PROFILE == "ha":
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


def internal_tls(s):
    """Internal TLS on (ruling 1084): every store's connection to the
    database over TLS, and a plaintext one refused, by the database and by
    the broker; a TLS one verified against the chart's CA by the name the
    stores use."""
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
    tolerated = all(
        any(t.get("key") == "node.kubernetes.io/unreachable" and t.get("tolerationSeconds") == 30
            for t in p["spec"].get("tolerations", []))
        for c in STORES + ["dashboard"] for p in pods(c))
    s.check(tolerated, "every stateless pod tolerates an unreachable node for 30 s")
    # The sets the preset turns up beside the stores (step 3b part 2): the
    # database's three members under Patroni, the broker's three servers,
    # the dashboard's two copies and analytics' two of each, every set spread
    # over three zones (or two, for two), with a budget over it.
    for component, want in (("database", 3), ("broker", 3), ("dashboard", 2),
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
            s.check(len(zones) == want and "" not in zones,
                    f"{component}: one member per zone ({', '.join(sorted(zones))})")
        budget = kubectl("get", "pdb", f"{FULL}-{component}", check=False).returncode == 0
        s.check(budget, f"{component}: a disruption budget over its members")
    roles = sorted(p["metadata"]["labels"].get("meridian.dev/database-role", "") for p in pods("database"))
    s.check(roles == ["primary", "replica", "replica"], f"database: one primary, two standbys ({roles})")
    sync = psql("select count(*) from pg_stat_replication where sync_state = 'sync'", database="postgres")
    s.check(sync == "1", f"database: one standby synchronous ({sync})")
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
    if failure == "db-primary":
        print("F: db-primary: the database's primary killed outright (SIGKILL), under commits", flush=True)
    for component in ("bor", "lake"):
        wait_for(f"{component} settled", lambda c=component: members_settled(c), seconds=300)
    marks_table()
    victim = primary()
    members = [p["metadata"]["name"] for p in pods("database")]
    # Commits made from a member that is not killed. The tolerant profile has
    # one member, killed with its writer: there the commits before the kill
    # are what is held.
    writing_in = next((m for m in members if m != victim), None)
    writer = write_marks(writing_in, 45, pause=0 if failure == "torn-write" else 0.05) if writing_in else None
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
    acked = acknowledged(writer) if writer else set()
    acked.add(acked_before)
    kept = {int(i) for i in psql("select id from failover_marks", pod=new_primary).split()}
    lost = sorted(acked - kept)
    if PROFILE != "tolerant":
        s.check(new_primary != victim, f"{failure}: a standby promoted ({new_primary})")
    s.check(not lost, f"{failure}: every acknowledged commit kept ({len(acked)} acknowledged, "
            f"{len(lost)} lost{': ' + ', '.join(map(str, lost[:5])) if lost else ''})")
    for component in ("bor", "lake"):
        if component not in took:
            s.check(False, f"{failure}: {component} never answered again within {bound[component] + 30}s")
            continue
        claim(s, failure, component, took[component], bound[component],
              {"acknowledged_commits": len(acked), "rpo_commits_lost": len(lost)})
    healed = happens(lambda: len(pods("database")) == database_members()
                     and all(ready(p) for p in pods("database")), 300)
    s.check(healed, f"{failure}: the killed member back, as a standby where there are standbys")


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
            'curl -fsS -o /dev/null -u "patroni:$PATRONI_RESTAPI_PASSWORD" -X POST '
            "-H 'Content-Type: application/json' -d '{}' http://127.0.0.1:8008/restart", quiet=True)
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
    for component in STORES:
        if component not in took:
            s.check(False, f"zone: {component} never answered again from outside zone {zone} within {HA_ZONE + 30}s")
            explain(s, "zone", component)
            continue
        claim(s, "zone", component, took[component], HA_ZONE,
              {"zone": zone, "nodes": len(lost), "writer_in_zone": component in there})
    wait_for("the zone's nodes back", lambda: all(
        any(c["type"] == "Ready" and c["status"] == "True" for c in n["status"]["conditions"])
        for n in nodes()), seconds=300)
    healed = all(happens(lambda c=component: members_settled(c), 300) for component in STORES)
    s.check(healed, f"zone: zone {zone}'s members back, as standbys")


# ── e2e-cluster-reconfigure: a live install moved between the profiles ────

# Commits through the database's Service for the whole move, from a pod of
# its own (every member the move restarts is not one to write from), each
# acknowledged id printed, the connection made again after a refusal.
LONG_WRITER = r"""
export PGPASSWORD="$POSTGRES_PASSWORD"
while [ ! -e /tmp/stop ]; do
  id=$(psql -X -q -At -h HOST -U postgres -d meridian -v ON_ERROR_STOP=1 \
       -c "insert into failover_marks (at) values (clock_timestamp()) returning id" 2>/dev/null) \
    && echo "ACKED $id"
  sleep 0.2
done
echo DONE
"""


def long_writer():
    host = f"{FULL}-database.{NAMESPACE}.svc.cluster.local"
    pod = {
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "e2e-writer", "namespace": NAMESPACE,
                     "labels": {"app.kubernetes.io/instance": RELEASE}},
        "spec": {
            "restartPolicy": "Never",
            "nodeSelector": {"kubernetes.io/hostname": server_node()},
            "containers": [{
                "name": "writer", "image": POSTGRES_IMAGE, "imagePullPolicy": "IfNotPresent",
                "command": ["sh", "-c", LONG_WRITER.replace("HOST", host)],
                "env": [{"name": "POSTGRES_PASSWORD", "valueFrom": {"secretKeyRef": {
                    "name": f"{FULL}-database-passwords", "key": "superuser"}}}],
            }],
        },
    }
    kubectl("create", "-f", "-", input=json.dumps(pod).encode())
    wait_for("the writer to commit", lambda: "ACKED" in out("logs", "e2e-writer", check=False), seconds=120)


def long_writer_acknowledged():
    kubectl("exec", "e2e-writer", "--", "touch", "/tmp/stop")
    wait_for("the writer to stop", lambda: "DONE" in out("logs", "e2e-writer", check=False), seconds=60)
    said = out("logs", "e2e-writer")
    kubectl("delete", "pod", "e2e-writer", "--wait")
    return {int(line.split()[1]) for line in said.splitlines()
            if line.startswith("ACKED ") and line.split()[1].isdigit()}


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


def reconfigure(s):
    global SHAPE
    print("R: the live install moved between the profiles, under commits", flush=True)
    marks_table()
    long_writer()
    for target in RECONFIGURE:
        before = record_now()
        print(f"R: {SHAPE} -> {target}, by the value `meridian upgrade-deployment --set` gives", flush=True)
        began = time.time()
        upgraded = run("helm", "upgrade", RELEASE, CHART, "--namespace", NAMESPACE, "--reuse-values",
                       *helm_takes_counts(), "--set", f"resilience.profile={target}", check=False)
        s.check(upgraded.returncode == 0, f"reconfigure: helm upgrade to {target} applied"
                + ("" if upgraded.returncode == 0 else f": {upgraded.stderr.decode(errors='replace')[-400:]}"))
        if upgraded.returncode != 0:
            return
        SHAPE = target
        if target == "ha":
            # A database alone moves under Patroni first, alone, and is given
            # its standbys by a second upgrade with the same values, as
            # `meridian upgrade-deployment` runs it when the chart says so.
            said = upgraded.stdout.decode(errors="replace")
            moved = "upgrade again with" in said
            s.check(moved, "reconfigure: the chart says the database moves under Patroni first, alone")
            if moved:
                wait_for("the database under Patroni, member 0 its leader", lambda: kubectl(
                    "exec", f"{FULL}-database-0", "-c", "postgres", "--", "pg_isready", "-h", "127.0.0.1",
                    "-U", "postgres", check=False).returncode == 0 and psql(
                    "select not pg_is_in_recovery()", database="postgres", pod=f"{FULL}-database-0") == "t",
                    seconds=600)
                again = run("helm", "upgrade", RELEASE, CHART, "--namespace", NAMESPACE, "--reuse-values",
                            *helm_takes_counts(), "--set", f"resilience.profile={target}", check=False)
                s.check(again.returncode == 0, "reconfigure: the second upgrade adds the database's standbys")
        settled = happens(settled_everywhere, 900, every=5)
        took = time.time() - began
        s.check(settled, f"reconfigure: every member in the {target} shape and ready, {took:.0f}s after the upgrade")
        if not settled:
            for component in STORES:
                explain(s, f"reconfigure to {target}", component)
            return
        claim(s, f"reconfigure-{target}", "deployment", took, None, {"from": "tolerant" if target == "ha" else "ha"})
        after = record_now()
        kept = all(set(before[store].splitlines()) <= set(after[store].splitlines()) for store in before)
        s.check(kept, f"reconfigure: each store's change log as it was before the move, added to and nothing else")
        properties(s, f"reconfigure to {target}")
    acked = long_writer_acknowledged()
    kept = {int(i) for i in psql("select id from failover_marks").split()}
    lost = sorted(acked - kept)
    s.check(acked and not lost, f"reconfigure: every acknowledged commit kept across both moves "
            f"({len(acked)} acknowledged, {len(lost)} lost{': ' + ', '.join(map(str, lost[:5])) if lost else ''})")


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
    if TLS:
        internal_tls(s)
    if PROFILE in ("ha", "scaled"):
        shape(s)
    properties(s, "the install")
    if PROFILE == "reconfigure":
        try:
            reconfigure(s)
        except SystemExit as stopped:
            s.check(False, f"reconfigure: {stopped}")
    for failure in failures:
        try:
            FAILURES[failure](s)
        except SystemExit as stopped:
            s.check(False, f"{failure}: {stopped}")
        try:
            properties(s, failure)
        except SystemExit as stopped:
            s.check(False, f"properties after {failure}: the change logs could not be read: {stopped}")
    return verdict(s)


if __name__ == "__main__":
    sys.exit(main())
