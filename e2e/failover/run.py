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
probe names the path and its address, and it heals with no restart).

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
CHART = os.path.abspath(os.environ.get("E2E_CHART", "deploy/chart"))
PROFILE = os.environ.get("PROFILE", "tolerant")
CLAIMS = os.environ.get("E2E_FAILOVER_CLAIMS", ".e2e-failover-claims.jsonl")
CLUSTER = os.environ.get("E2E_K3D_CLUSTER", "meridian-e2e")
ADMIN = "local|alice"
ZONES = ["zone-a", "zone-b", "zone-c"]

STORES = ["lake", "bor", "instrument", "street", "conductor", "launcher"]
BINARY = {c: f"meridian-{c}" for c in STORES}

# The bounds each profile is held to (rulings 1048 and 1056), in seconds.
TOLERANT_POD = 120
HA = {"bor": 10, "street": 10, "instrument": 10, "conductor": 10, "launcher": 10, "lake": 15}

DEFAULT_FAILURES = {
    "tolerant": ["lake-writer", "bor-writer", "network-policy"],
    "ha": ["lake-writer", "bor-writer", "street-writer", "instrument-writer",
           "split-brain", "node-freeze", "node", "network-policy"],
    "scaled": [],
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
    started = time.time()
    while time.time() - started < seconds:
        try:
            if ready():
                return True
        except Exception:
            pass
        time.sleep(every)
    return False


def wait_for(what, ready, seconds=600):
    if not happens(ready, seconds):
        raise SystemExit(f"{what} never happened, in {seconds}s")


def secret(name):
    data = json.loads(out("get", "secret", name, "-o", "json")).get("data") or {}
    return {key: base64.b64decode(value) for key, value in data.items()}


def psql(sql, database="meridian", quiet=False):
    return kubectl(
        "exec", "-i", f"{FULL}-database-0", "--",
        "psql", "-X", "-q", "-At", "-v", "ON_ERROR_STOP=1", "-U", "postgres", "-d", database,
        input=sql.encode(), quiet=quiet,
    ).stdout.decode().strip()


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
    done = kubectl("exec", pod_name, "-c", component, "--", BINARY[component], "probe", check=False)
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


def epoch_now(component):
    said = answering(component)
    return ((said or {}).get("answers") or {}).get("epoch", 0)


def claim(s, failure, component, seconds, bound, extra=None):
    line = {
        "profile": PROFILE, "failure": failure, "component": component,
        "recovered_s": round(seconds, 2), "bound_s": bound,
        "held": seconds <= bound,
        "command": f"make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-failover PROFILE={PROFILE} FAILURE={failure}",
        "kubernetes": kubernetes_version(),
        "run": os.environ.get("GITHUB_RUN_ID", "local"),
    }
    line.update(extra or {})
    print(f"CLAIM {json.dumps(line)}", flush=True)
    with open(CLAIMS, "a", encoding="utf-8") as kept:
        kept.write(json.dumps(line) + "\n")
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
    if PROFILE in ("ha", "scaled"):
        # The conductor's standby needs the key as a Secret: the generated
        # one's volume attaches to one node.
        workdir = tempfile.mkdtemp(prefix="failover-key-")
        key = os.path.join(workdir, "key.pem")
        run("openssl", "genpkey", "-algorithm", "ed25519", "-out", key)
        run("kubectl", "create", "namespace", NAMESPACE, check=False)
        kubectl("create", "secret", "generic", "failover-key", f"--from-file=key.pem={key}", quiet=True)
        os.remove(key)
        values += ["--set", "key.generate=false", "--set", "key.existingSecret=failover-key"]
        for component in STORES:
            values += ["--set", f"{component}.standby=1"]
        values += ["--set", "zones={" + ",".join(ZONES) + "}"]
    run("helm", "install", RELEASE, CHART, "--namespace", NAMESPACE, "--create-namespace", *values)
    s.check(True, f"helm install, profile {PROFILE}")
    if PROFILE in ("ha", "scaled"):
        # The database and the broker kept on the server node, so a test
        # that stops or pauses an agent never stops them (theirs are 3b's
        # database and broker classes, injected apart).
        pin = json.dumps({"spec": {"template": {"spec": {"nodeSelector": {
            "kubernetes.io/hostname": server_node()}}}}})
        kubectl("patch", "statefulset", f"{FULL}-database", "--type", "merge", "-p", pin)
        kubectl("patch", "deployment", f"{FULL}-broker", "--type", "merge", "-p", pin, check=False)


def first_run(s):
    """What the wizard's Job writes, written directly, then what it does:
    the database, its roles, the URLs; the conductor and the dashboard
    restarted; the Job ended and its rights given up (as e2e/backup)."""
    wait_for("the database's StatefulSet", lambda: kubectl(
        "get", "statefulset", f"{FULL}-database", check=False).returncode == 0)
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=1")
    wait_for("the database", lambda: kubectl(
        "exec", f"{FULL}-database-0", "--", "pg_isready", "-h", "127.0.0.1", "-U", "postgres",
        check=False).returncode == 0)
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


def all_ready():
    names = out("get", "deployments", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "name").split()
    if not names:
        return False
    return kubectl("wait", "--for=condition=Available", "--timeout=5s", *names, check=False).returncode == 0


def members_settled(component):
    """Every member running and ready: two with a standby."""
    want = 2 if PROFILE in ("ha", "scaled") else 1
    live = pods(component)
    return len(live) == want and all(ready(p) for p in live)


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
        modes = sorted((probe(p["metadata"]["name"], component) or {}).get("member", {}).get("mode", "")
                       for p in live)
        s.check(modes == ["standby", "writer"], f"{component}: one writer, one standby ({modes})")
        budget = kubectl("get", "pdb", f"{FULL}-{component}", check=False).returncode == 0
        s.check(budget, f"{component}: a disruption budget over its members")
    tolerated = all(
        any(t.get("key") == "node.kubernetes.io/unreachable" and t.get("tolerationSeconds") == 30
            for t in p["spec"].get("tolerations", []))
        for c in STORES + ["dashboard"] for p in pods(c))
    s.check(tolerated, "every stateless pod tolerates an unreachable node for 30 s")


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
        modes = sorted((probe(p["metadata"]["name"], component) or {}).get("member", {}).get("mode", "")
                       for p in pods(component))
        s.check(modes == ["standby", "writer"], f"{failure}: the killed writer came back as the standby ({modes})")


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
    (kube-router's, as most) lets carry on."""
    node, ip = node_of(pod_name), pod_ip(pod_name)
    for target in targets:
        for rule in (["-s", ip, "-d", target], ["-s", target, "-d", ip]):
            run("docker", "exec", node, "iptables", verb, "FORWARD", *(["1"] if verb == "-I" else []),
                *rule, "-j", "DROP", check=verb == "-I")


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
        # The cut writer stepped down before its lease ended, and cannot
        # come back as anything but an unready standby while cut.
        s.check(happens(lambda: not ready(json.loads(out("get", "pod", writer, "-o", "json"))), 60),
                "split-brain: the cut member is no longer ready")
        s.check(holder(component) != writer, "split-brain: the lease is the standby's: one writer")
    finally:
        cut_rules(writer, targets, "-D")
    healed = happens(lambda: members_settled(component), 180)
    s.check(healed, "split-brain: healed once the cut is lifted, the cut member back as the standby")


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
        modes = sorted((probe(p["metadata"]["name"], component) or {}).get("member", {}).get("mode", "")
                       for p in pods(component))
        s.check(settled and modes == ["standby", "writer"] and holder(component) in
                {p["metadata"]["name"] for p in pods(component)},
                f"network-policy: one writer and its standby once healed ({modes})")
    if healed:
        claim(s, "network-policy", component, took, 60)


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
    if PROFILE in ("ha", "scaled"):
        agents = [n for n in nodes() if "topology.kubernetes.io/zone" in n["metadata"].get("labels", {})]
        if len(agents) < 3:
            raise SystemExit("the HA profile needs three agents labelled into zones: "
                             "make e2e-cluster-k3d E2E_K3D_AGENTS=3 E2E_K3D_ZONES='zone-a zone-b zone-c'")
    print(f"A: installed in the {PROFILE} profile", flush=True)
    install(s)
    first_run(s)
    if PROFILE in ("ha", "scaled"):
        shape(s)
    for failure in failures:
        try:
            FAILURES[failure](s)
        except SystemExit as stopped:
            s.check(False, f"{failure}: {stopped}")
    return verdict(s)


if __name__ == "__main__":
    sys.exit(main())
