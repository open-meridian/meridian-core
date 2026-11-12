"""Backup and restore, proved on a scratch cluster.

Task kernel/a-deployment-is-backed-up-and-restored: a deployment holding
people, their access, plugin settings with a secret one sealed, and a book,
backed up by the chart's CronJob on its schedule and by `meridian backup
--to`; then wiped, as a reset of Rancher Desktop wipes a laptop's cluster,
leaving only the directory the backups were written to; then restored by
`meridian restore`, and held to what it was: the same people and access, the
same settings, the secret opening with the restored settings key to the
value it was sealed from, the same book line for line, the same keys, an edge
plugin's storage file for file (ruling 828), every component ready, and a
backup taken again after.

It needs a cluster that is nobody's: `make e2e-cluster-backup` refuses any
kube context that is not a k3d or kind one, and `make e2e-cluster-k3d
E2E_CLUSTER_TARGET=e2e-cluster-backup` makes one and removes it. No platform:
the conductor waits for an enrolment nobody gives, and nothing here needs it.

What the wizard's Job writes (the database, its roles, the URLs) is written
here directly, as first run writes it, and what it does then is done as it
does it: the conductor and the dashboard restarted, which read the URL and the
administrator only when they start, and its rights given up. The rows the
backup carries are put in with SQL, every one in the shape its store keeps.
The secret is sealed here with the deployment's own settings key, in the
conductor's format (meridian_config::sealing: a version byte, a 24-byte
nonce, XChaCha20-Poly1305 with the instance and the setting's name as
associated data), and opened after the restore with the key the restore put
back. No value of a
secret, a key or a password is ever printed: each is compared by its hash.
"""

import base64
import hashlib
import json
import os
import re
import struct
import subprocess
import sys
import tempfile
import time

NAMESPACE = os.environ.get("E2E_NAMESPACE", "meridian-backup-e2e")
RELEASE = os.environ.get("E2E_RELEASE", "backup-e2e")
FULL = f"{RELEASE}-meridian-runtime"
IMAGE = os.environ["E2E_IMAGE"]
BACKUP_IMAGE = os.environ["E2E_BACKUP_IMAGE"]
CLI_IMAGE = os.environ["E2E_CLI_IMAGE"]
# The brought database's image under Patroni, which holds pgBackRest: the
# database archiving its WAL (backup.walArchive) runs from it.
POSTGRES_IMAGE = os.environ["E2E_POSTGRES_IMAGE"]
CHART = os.path.abspath(os.environ.get("E2E_CHART", "deploy/chart"))
BOOK_SQL = os.path.abspath("deploy/harness/book.sql")
# A directory on the node, as a laptop's would be under the Mac's home: what
# survives the wipe.
BACKUP_PATH = os.environ.get("E2E_BACKUP_PATH", "/var/lib/meridian-backup-e2e")
PLUGIN = "e2e-plugin"
# An edge plugin's storage (decisions/028), as the launcher makes its claim:
# backed up by `meridian backup` and made again, filled, by `meridian restore`
# (ruling 828). No plugin runs on it; what is proved is the claim and every
# file in it, its owner and mode.
STORE = "e2e-store"
STORE_CLAIM = f"{FULL}-storage-{STORE}"
SECRET_VALUE = "a-vendor-key-for-the-e2e-only"
ADMIN = "local|alice"


class Scorecard:
    def __init__(self):
        self.failures = []

    def check(self, held, said):
        print(f"    {'ok' if held else 'FAILED'}: {said}", flush=True)
        if not held:
            self.failures.append(said)


def run(*command, input=None, check=True, quiet=False):
    done = subprocess.run(command, capture_output=True, input=input)
    if check and done.returncode != 0:
        # Never the command's input, which may hold a Secret; and with
        # `quiet`, not its output either, where psql can quote a statement.
        said = "(its output is not shown: it can quote a password)" if quiet else (
            f"{done.stdout.decode(errors='replace')}\n{done.stderr.decode(errors='replace')}")
        raise SystemExit(f"{' '.join(command)} failed ({done.returncode})\n{said}")
    return done


def kubectl(*args, input=None, check=True, quiet=False):
    return run("kubectl", "--namespace", NAMESPACE, *args, input=input, check=check, quiet=quiet)


def out(*args):
    return kubectl(*args).stdout.decode().strip()


def happens(ready, seconds):
    """Whether `ready` answers within `seconds`, asked every two."""
    started = time.time()
    while time.time() - started < seconds:
        try:
            if ready():
                return True
        except Exception:
            pass
        time.sleep(2)
    return False


def wait_for(what, ready, seconds=420):
    if not happens(ready, seconds):
        raise SystemExit(f"{what} never happened, in {seconds}s")


def secret(name):
    """A Secret's data, decoded, in memory only."""
    data = json.loads(out("get", "secret", name, "-o", "json")).get("data") or {}
    return {key: base64.b64decode(value) for key, value in data.items()}


def digest(data):
    """A Secret's or ConfigMap's contents as one hash: what is compared."""
    h = hashlib.sha256()
    for key in sorted(data):
        value = data[key] if isinstance(data[key], bytes) else data[key].encode()
        h.update(key.encode() + b"\0" + hashlib.sha256(value).digest())
    return h.hexdigest()


def psql(sql, database="meridian", quiet=False):
    return kubectl(
        "exec", "-i", f"{FULL}-database-0", "-c", "postgres", "--",
        "psql", "-X", "-q", "-At", "-v", "ON_ERROR_STOP=1", "-U", "postgres", "-d", database,
        input=sql.encode(), quiet=quiet,
    ).stdout.decode().strip()


def cli(workdir, *args):
    """The real `meridian`, in the image meridian-cli's e2e builds, with this
    machine's kubeconfig and the run's directory."""
    kubeconfig = os.environ.get("KUBECONFIG") or os.path.expanduser("~/.kube/config")
    done = subprocess.run(
        ["docker", "run", "--rm", "--network", "host",
         "-v", f"{kubeconfig}:/kube/config:ro", "-e", "KUBECONFIG=/kube/config",
         "-v", f"{workdir}:/work", "-w", "/work",
         "-v", f"{CHART}:/chart:ro",
         CLI_IMAGE, *args],
        capture_output=True, text=True,
    )
    for line in (done.stdout + done.stderr).splitlines():
        print(f"    | {line}", flush=True)
    return done


# ── The conductor's sealing, to seal a secret as it would and open it after ──

M32 = 0xFFFFFFFF
SIGMA = struct.unpack("<4I", b"expand 32-byte k")


def _rotl(v, c):
    return ((v << c) & M32) | (v >> (32 - c))


def _rounds(s):
    def qr(a, b, c, d):
        s[a] = (s[a] + s[b]) & M32; s[d] = _rotl(s[d] ^ s[a], 16)
        s[c] = (s[c] + s[d]) & M32; s[b] = _rotl(s[b] ^ s[c], 12)
        s[a] = (s[a] + s[b]) & M32; s[d] = _rotl(s[d] ^ s[a], 8)
        s[c] = (s[c] + s[d]) & M32; s[b] = _rotl(s[b] ^ s[c], 7)
    for _ in range(10):
        qr(0, 4, 8, 12); qr(1, 5, 9, 13); qr(2, 6, 10, 14); qr(3, 7, 11, 15)
        qr(0, 5, 10, 15); qr(1, 6, 11, 12); qr(2, 7, 8, 13); qr(3, 4, 9, 14)


def _block(key, counter, nonce12):
    s = list(SIGMA) + list(struct.unpack("<8I", key)) + [counter] + list(struct.unpack("<3I", nonce12))
    w = s[:]
    _rounds(w)
    return struct.pack("<16I", *[(w[i] + s[i]) & M32 for i in range(16)])


def _hchacha(key, nonce16):
    s = list(SIGMA) + list(struct.unpack("<8I", key)) + list(struct.unpack("<4I", nonce16))
    _rounds(s)
    return struct.pack("<8I", *(s[0:4] + s[12:16]))


def _xor(key, nonce12, data):
    stream = b"".join(_block(key, 1 + i // 64, nonce12) for i in range(0, len(data), 64))
    return bytes(a ^ b for a, b in zip(data, stream))


def _poly1305(key, message):
    r = int.from_bytes(key[:16], "little") & 0x0FFFFFFC0FFFFFFC0FFFFFFC0FFFFFFF
    s = int.from_bytes(key[16:], "little")
    p, acc = (1 << 130) - 5, 0
    for i in range(0, len(message), 16):
        acc = (acc + int.from_bytes(message[i:i + 16] + b"\x01", "little")) * r % p
    return ((acc + s) % (1 << 128)).to_bytes(16, "little")


def _tag(subkey, nonce12, aad, ciphertext):
    pad = lambda b: b + b"\0" * (-len(b) % 16)
    one_time = _block(subkey, 0, nonce12)[:32]
    return _poly1305(one_time, pad(aad) + pad(ciphertext) + struct.pack("<QQ", len(aad), len(ciphertext)))


def _aad(plugin, name):
    return b"meridian.plugin-setting.v1\0" + plugin.encode() + b"\0" + name.encode()


def seal(key, plugin, name, value):
    nonce = os.urandom(24)
    subkey, nonce12 = _hchacha(key, nonce[:16]), b"\0" * 4 + nonce[16:]
    ciphertext = _xor(subkey, nonce12, value.encode())
    return b"\x01" + nonce + ciphertext + _tag(subkey, nonce12, _aad(plugin, name), ciphertext)


def opened(key, plugin, name, sealed):
    """The value, or None where it does not open with this key."""
    if len(sealed) < 41 or sealed[0] != 1:
        return None
    nonce, body, tag = sealed[1:25], sealed[25:-16], sealed[-16:]
    subkey, nonce12 = _hchacha(key, nonce[:16]), b"\0" * 4 + nonce[16:]
    if _tag(subkey, nonce12, _aad(plugin, name), body) != tag:
        return None
    return _xor(subkey, nonce12, body).decode()


# ── What the deployment holds, read the same way before and after ────────────

def held():
    """Everything the run compares, as hashes and stable lines."""
    sealed_hex = psql(
        f"select encode(sealed, 'hex') from config_plugin_setting "
        f"where plugin_instance_id = '{PLUGIN}' and name = 'api_key'"
    )
    key = secret(f"{FULL}-settings-key").get("key", b"")
    value = opened(key, PLUGIN, "api_key", bytes.fromhex(sealed_hex)) if sealed_hex else None
    keys = json.loads(out("get", "configmap", f"{FULL}-dashboard-keys", "-o", "json")).get("data") or {}
    return {
        "people": psql(
            "select g.name || '|' || array_to_string(g.logins, ',') || '|' || "
            "coalesce(string_agg(p.access_group_id, ',' order by p.access_group_id), '') "
            "from config_user_group g left join config_permission p using (user_group_id) "
            "group by g.user_group_id, g.name, g.logins order by g.name collate \"C\""
        ),
        "accounts": psql("select account_id || '|' || name from config_account order by account_id collate \"C\""),
        "settings": psql(
            "select plugin_instance_id || '|' || name || '|' || coalesce(value, '<sealed>') "
            "from config_plugin_setting order by plugin_instance_id collate \"C\", name collate \"C\""
        ),
        "secret opens to its value": value is not None
        and hashlib.sha256(value.encode()).hexdigest() == hashlib.sha256(SECRET_VALUE.encode()).hexdigest(),
        "book": psql(open(BOOK_SQL).read()),
        "settings key": digest(secret(f"{FULL}-settings-key")),
        "dashboard signing key": digest(secret(f"{FULL}-dashboard-signing")),
        "dashboard public keys": digest(keys),
        "database passwords": digest(secret(f"{FULL}-database-passwords")),
        "broker credentials": digest(secret(f"{FULL}-broker")),
    }


def deployment_key():
    """The conductor's key, by its hash, wherever the chart mounted it."""
    for path in ("/var/lib/meridian/key.pem", "/etc/meridian/key.pem"):
        done = kubectl("exec", f"deploy/{FULL}-conductor", "--", "sha256sum", path, check=False)
        if done.returncode == 0:
            return done.stdout.decode().split()[0]
    return ""


def node_backups():
    """The backup files on the node's directory, which the wipe leaves."""
    node = run("kubectl", "get", "nodes", "-o", "jsonpath={.items[0].metadata.name}").stdout.decode()
    done = run("docker", "exec", node, "sh", "-c", f"ls -1 {BACKUP_PATH} 2>/dev/null || true")
    return sorted(line for line in done.stdout.decode().split() if line.endswith(".meridian-backup"))


def scheduled_runs():
    """The CronJob's own Jobs, oldest first: not the ones `meridian backup` makes."""
    jobs = json.loads(out("get", "jobs", "-l", "meridian.dev/component=backup", "-o", "json"))["items"]
    jobs = [j for j in jobs
            if not j["metadata"].get("annotations", {}).get("cronjob.kubernetes.io/instantiate")]
    jobs.sort(key=lambda j: j["metadata"]["creationTimestamp"])
    return jobs


def finished(job):
    return any(c["type"] in ("Complete", "Failed") and c["status"] == "True"
               for c in job["status"].get("conditions", []))


def first_run_job():
    """The first-run Job's manifest, as the chart made it."""
    jobs = json.loads(out("get", "jobs", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))["items"]
    return next(j for j in jobs
                if j["spec"]["template"]["metadata"]["labels"].get("meridian.dev/component") == "first-run")


def first_run_env(job):
    container = job["spec"]["template"]["spec"]["containers"][0]
    return {e["name"]: e.get("value", "") for e in container.get("env", [])}


def first_run_finishes():
    """The rest of what the Job's apply does once the Secrets are written
    (meridian_first_run::FirstRun::apply): the Deployments the chart names
    restarted, since the conductor and the dashboard read the database's URL
    and the administrator only when they start; its exit; and its own
    RoleBinding gone. The apply itself is reached only through the wizard,
    over the bus and sealed to a key the Job holds in memory, which
    e2e-cluster drives; here its pod is stopped, as it stops once applied.

    The binding is deleted after the Job is done, never before: a pod stopped
    while running may be counted failed and made again, and the new one finds
    the URL and gives up the rights itself (stand_down), which it can do only
    while it holds them. Counted done instead, it leaves them, and they are
    deleted here."""
    job = first_run_job()
    env = first_run_env(job)
    restarted = [name.strip() for name in env["MERIDIAN_FIRST_RUN_RESTART"].split(",") if name.strip()]
    for name in restarted:
        kubectl("rollout", "restart", f"deployment/{name}")
    kubectl("delete", "pod", "-l", f"job-name={job['metadata']['name']}", "--wait")
    wait_for("the first-run Job to finish", lambda: json.loads(out(
        "get", "job", job["metadata"]["name"], "-o", "json"))["status"].get("succeeded"), seconds=120)
    kubectl("delete", "rolebinding", env["MERIDIAN_FIRST_RUN_BINDING"], "--ignore-not-found")
    for name in restarted:
        kubectl("rollout", "status", f"deployment/{name}", "--timeout=600s")


def first_run_finished():
    """The Job done and its rights gone: what a finished first run leaves."""
    try:
        job = first_run_job()
    except StopIteration:
        return False
    status = json.loads(out("get", "job", job["metadata"]["name"], "-o", "json"))["status"]
    binding = first_run_env(job)["MERIDIAN_FIRST_RUN_BINDING"]
    return bool(status.get("succeeded")) and kubectl("get", "rolebinding", binding, check=False).returncode != 0


def started_with_the_database():
    """Whether the conductor and the dashboard each started with the
    database's URL: read in the running container, never printed. Without it
    the conductor keeps its configuration in memory."""
    for component, variable in (("conductor", "MERIDIAN_CONFIG_DATABASE_URL"),
                                ("dashboard", "MERIDIAN_LOCAL_ACCOUNTS_DATABASE_URL")):
        pods = json.loads(out(
            "get", "pods", "-l", f"app.kubernetes.io/instance={RELEASE},meridian.dev/component={component}",
            "--field-selector=status.phase=Running", "-o", "json"))["items"]
        serving = [p["metadata"]["name"] for p in pods if not p["metadata"].get("deletionTimestamp")]
        if not serving:
            return False
        for pod in serving:
            done = kubectl("exec", pod, "--", "sh", "-c", f'test -n "${variable}"', check=False)
            if done.returncode != 0:
                return False
    return True


def store_pod(name, command, read_only):
    """A pod of the backup image mounting the store's claim, run to its end;
    what it printed. The image is in the node already (image-load)."""
    pod = {
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": NAMESPACE},
        "spec": {
            "restartPolicy": "Never",
            "containers": [{"name": "store", "image": BACKUP_IMAGE, "imagePullPolicy": "IfNotPresent",
                            "command": ["sh", "-c", command],
                            "volumeMounts": [{"name": "store", "mountPath": "/store", "readOnly": read_only}]}],
            "volumes": [{"name": "store", "persistentVolumeClaim": {"claimName": STORE_CLAIM, "readOnly": read_only}}],
        },
    }
    kubectl("delete", "pod", name, "--ignore-not-found", "--wait")
    kubectl("create", "-f", "-", input=json.dumps(pod).encode())
    wait_for(f"the pod {name} to finish", lambda: out(
        "get", "pod", name, "-o", "jsonpath={.status.phase}") in ("Succeeded", "Failed"), seconds=180)
    said = out("logs", name)
    phase = out("get", "pod", name, "-o", "jsonpath={.status.phase}")
    kubectl("delete", "pod", name, "--wait")
    if phase != "Succeeded":
        raise SystemExit(f"the pod {name} failed:\n{said}")
    return said


def fill_store():
    """The claim as the launcher makes one for an edge plugin, and records in
    it as a plugin keeps them: a file of its own, one owned by another user
    and readable by its group alone, and a directory."""
    claim = {
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {"name": STORE_CLAIM, "namespace": NAMESPACE, "labels": {
            "meridian.dev/component": "plugin-storage", "meridian.dev/instance": STORE,
            "meridian.dev/launched": "true", "meridian.dev/plugin": STORE,
            "app.kubernetes.io/instance": RELEASE, "app.kubernetes.io/name": "meridian-runtime"}},
        "spec": {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "64Mi"}}},
    }
    kubectl("create", "-f", "-", input=json.dumps(claim).encode())
    store_pod("e2e-store-fill",
              "set -e; mkdir -p /store/session /store/statements; "
              "head -c 300000 /dev/urandom >/store/session/messages.log; "
              "echo 'seq 4127' >/store/session/sequence; "
              "head -c 5000 /dev/urandom >/store/statements/2026-10-29.xml; "
              "chown 65532:65532 /store/statements/2026-10-29.xml; chmod 0640 /store/statements/2026-10-29.xml",
              read_only=False)


def store_held():
    """Every file in the store: its hash, owner, group and mode."""
    return store_pod("e2e-store-read",
                     "cd /store && find . -type f | LC_ALL=C sort | while read -r f; do "
                     "echo \"$(sha256sum \"$f\" | cut -d' ' -f1) $(stat -c '%u:%g %a' \"$f\") $f\"; done",
                     read_only=True)


def all_ready():
    names = out("get", "deployments", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "name").split()
    if not names:
        return False
    return kubectl(
        "wait", "--for=condition=Available", "--timeout=5s", *names, check=False
    ).returncode == 0


def main():
    s = Scorecard()
    workdir = tempfile.mkdtemp(prefix="meridian-backup-e2e-", dir=os.environ.get("E2E_WORKDIR"))
    os.chmod(workdir, 0o777)

    # Whether this CLI backs up plugins' storage (ruling 828), which core's
    # CI meets at meridian-cli's main: until that lands there, the store is
    # left out of this run, and said so.
    stores = "plugin's storage" in cli(workdir, "backup", "--help").stdout
    if not stores:
        print("    skipped: this meridian does not back up plugins' storage yet", flush=True)

    print("A: an identity to seal backups to, by `meridian backup key`", flush=True)
    done = cli(workdir, "backup", "key", "--to", "identity.txt")
    s.check(done.returncode == 0, f"meridian backup key: exit {done.returncode}")
    recipient = next((w for w in done.stdout.split() if w.startswith("age1")), "")
    s.check(bool(recipient), "and it printed the public half")
    # For the container's user, where this machine owns what the container wrote (a Mac's
    # shared folder). On Linux the container's user owns it already, and reads it back.
    identity = os.path.join(workdir, "identity.txt")
    if os.path.exists(identity) and os.stat(identity).st_uid == os.getuid():
        os.chmod(identity, 0o644)
    again = cli(workdir, "backup", "key", "--to", "identity.txt")
    s.check(again.returncode != 0 and "never written over" in again.stderr,
            "a second one is never written over the first")
    if not recipient:
        return verdict(s)

    print("B: installed, with a backup every minute to a directory on the node", flush=True)
    repository, _, tag = IMAGE.rpartition(":")
    run("helm", "install", RELEASE, CHART, "--namespace", NAMESPACE, "--create-namespace",
        "--set", "deployment.id=DEP-backup-e2e",
        "--set", "platform.address=http://127.0.0.1:9",
        "--set", f"image.repository={repository}", "--set", f"image.tag={tag}",
        "--set", "registry.enabled=false",
        "--set", f"backup.image={BACKUP_IMAGE}",
        "--set", f"backup.recipient={recipient}",
        "--set", f"backup.path={BACKUP_PATH}",
        "--set", "backup.schedule=*/1 * * * *",
        "--set", "backup.keep=1",
        # The WAL archived beside the backups, for a restore to a time (J).
        "--set", "backup.walArchive.enabled=true",
        "--set", f"database.brought.patroniImage={POSTGRES_IMAGE}")
    s.check(True, "helm install")

    print("C: first run, as the wizard's Job writes it", flush=True)
    wait_for("the database's StatefulSet", lambda: kubectl(
        "get", "statefulset", f"{FULL}-database", check=False).returncode == 0)
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=1")
    wait_for("the database", lambda: kubectl(
        "exec", f"{FULL}-database-0", "--", "pg_isready", "-h", "127.0.0.1", "-U", "postgres",
        check=False).returncode == 0)
    passwords = secret(f"{FULL}-database-passwords")
    serving, migrating = passwords["serving"].decode(), passwords["migrating"].decode()
    psql(
        f"create role meridian_app login password '{serving}';\n"
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
    kubectl("replace", "-f", "-", input=json.dumps(database).encode())
    # A scheduled run made before this has no database to back up, and says
    # so: "no database yet ... the keys and values alone", no counts. With a
    # run every minute, one can start in the seconds between the install and
    # here (CI runs 40235294905, 40448007851 and 40614116769, each where C
    # began 2-6s before a minute). E reads a run made after.
    before_the_url = {j["metadata"]["name"] for j in scheduled_runs()}
    addresses = json.loads(out("get", "secret", "meridian-addresses", "-o", "json"))
    addresses["data"] = {
        "dashboard-url": base64.b64encode(b"http://backup-e2e.localhost").decode(),
        "administrator-login": base64.b64encode(ADMIN.encode()).decode(),
    }
    kubectl("replace", "-f", "-", input=json.dumps(addresses).encode())
    del serving, migrating, passwords
    first_run_finishes()
    wait_for("every component ready", all_ready, seconds=600)
    s.check(True, "every component is ready")
    s.check(started_with_the_database(), "the conductor and the dashboard started with the database's URL")
    s.check(first_run_finished(), "first run finished, and holds no rights")
    wait_for("the administrator's permission, which the conductor writes", lambda: ADMIN in psql(
        "select array_to_string(logins, ',') from config_user_group"), seconds=300)
    s.check(True, "the conductor wrote the administrator's permission")

    print("D: people, an account, settings with a secret sealed, and a book", flush=True)
    key = secret(f"{FULL}-settings-key")["key"]
    sealed = seal(key, PLUGIN, "api_key", SECRET_VALUE)
    del key
    psql(
        "insert into config_user_group (user_group_id, name, logins) "
        "values ('ug-traders', 'Traders', '{local|bob}');\n"
        "insert into config_account (account_id, name, state, created_at_ns) "
        "values ('ACC-e2e', 'Fund A', 1, 1760000000000000000);\n"
        f"insert into config_plugin_setting (plugin_instance_id, name, value, sealed, set_by, set_at_ns) values "
        f"('{PLUGIN}', 'region', 'us-east', null, '{ADMIN}', 1760000000000000000), "
        f"('{PLUGIN}', 'api_key', null, decode('{sealed.hex()}', 'hex'), '{ADMIN}', 1760000000000000001);\n"
        "update book_partition_head set sequence = 2 where partition = 'P0';\n"
        "insert into book_account (account_id, partition) values ('ACC-e2e', 'P0');\n"
        "insert into book_attributes (account_id, partition, sequence, base_currency_code, "
        "lot_relief_default, opening_as_of, record) values ('ACC-e2e', 'P0', 1, 'USD', 1, '', ''::bytea);\n"
        "insert into book_entry (entry_id, account_id, partition, first_sequence, last_sequence, kind, "
        "effective_date, actor, committed_at_ns, meta, cause, body, reply) values "
        f"('ENT-1', 'ACC-e2e', 'P0', 1, 1, 'attributes', '2026-10-01', '{ADMIN}', 1, '', '', '', ''), "
        f"('ENT-2', 'ACC-e2e', 'P0', 2, 2, 'trade', '2026-10-02', '{ADMIN}', 2, '', '', '', '');\n"
        "insert into book_position (account_id, instrument_id, side, partition, sequence, removed, "
        "trade_date_quantity, settled_quantity, not_stated_quantity, effective_date, record) values "
        "('ACC-e2e', 'INS-E2E', 1, 'P0', 2, false, 100.25, null, 0, '2026-10-02', ''::bytea);\n"
        "insert into book_lot (account_id, instrument_id, side, lot_id, ordinal, open_quantity, "
        "original_quantity, cost, cost_currency, acquired_date, source) values "
        "('ACC-e2e', 'INS-E2E', 1, 'LOT-1', 1, 100.25, 100.25, 1234.5600, 'USD', '2026-10-02', 1);\n"
    )
    if stores:
        fill_store()
    before = held()
    key_before = deployment_key()
    # The administrator holds what first run grants (meridian_config's
    # install_named_administrator): deployment admin and every plugin's admin.
    s.check("Traders|local|bob|" in before["people"]
            and f"|{ADMIN}|all-plugins-admin,deployment-admin" in before["people"],
            "two people: the administrator with their access, and a trader")
    s.check(before["secret opens to its value"], "the secret setting opens with the settings key")
    s.check("position|Fund A|INS-E2E|" in before["book"] and "lot|Fund A|INS-E2E|" in before["book"],
            f"the book holds Fund A's position and lot ({len(before['book'].splitlines())} lines)")
    s.check(bool(key_before), "the conductor holds the deployment's key")

    print("E: backed up on the schedule, then by `meridian backup --to`", flush=True)
    def succeeded_with_the_url():
        """The scheduled runs made after the database's URL was written that succeeded, oldest first."""
        return [j["metadata"]["name"] for j in scheduled_runs()
                if j["status"].get("succeeded") and j["metadata"]["name"] not in before_the_url]
    wait_for("a scheduled backup made after the database's URL was written",
             lambda: bool(succeeded_with_the_url()), seconds=200)
    s.check(True, "the CronJob wrote a backup on its schedule")
    # Suspended, and every run it made finished, before a log is read: the
    # CronJob keeps one succeeded Job (successfulJobsHistoryLimit), and a
    # later run finishing deletes the earlier one, its pod with it.
    kubectl("patch", "cronjob", f"{FULL}-backup", "--type=merge", "-p", '{"spec":{"suspend":true}}')
    wait_for("the scheduled runs to finish", lambda: all(finished(j) for j in scheduled_runs()), seconds=120)
    newest = succeeded_with_the_url()[-1]
    pods = out("get", "pods", "-l", f"job-name={newest}",
               "--field-selector=status.phase=Succeeded", "-o", "name").split()
    log = out("logs", pods[0], "--tail=-1") if pods else ""
    s.check("backup: wrote " in log and "tables" in log, "and said what it wrote, names and counts only")
    s.check(bool(log) and SECRET_VALUE not in log and recipient not in log, "and printed no secret")
    # What the backup below holds, read just before it.
    before = held()
    store_before = store_held() if stores else ""
    if stores:
        s.check(len(store_before.splitlines()) == 3, f"the store holds three files: {store_before.splitlines()}")
    done = cli(workdir, "backup", "--release", RELEASE, "-n", NAMESPACE, "--to", "/work")
    s.check(done.returncode == 0, f"meridian backup --to: exit {done.returncode}")
    if stores:
        s.check(f"plugin storage: {STORE_CLAIM}" in done.stdout, "and it backed up the plugin's storage")
    files = [f for f in os.listdir(workdir) if f.endswith(".meridian-backup")]
    s.check(len(files) == 1, f"and the file is here: {files}")
    on_node = node_backups()
    s.check(len(on_node) == 1 and files and on_node[0] == files[0],
            f"the node keeps the newest one alone (backup.keep=1): {on_node}")
    if not files:
        return verdict(s)
    with open(os.path.join(workdir, files[0]), "rb") as sealed_file:
        head = sealed_file.read(64)
    s.check(head.startswith(b"age-encryption.org/v1"), "the file is sealed with age")

    restored_to_a_time(s, before)
    checksums_enabled(s, before)

    print("F: wiped, as a reset of the cluster wipes it; the node's directory is left", flush=True)
    run("helm", "uninstall", RELEASE, "--namespace", NAMESPACE, "--wait")
    run("kubectl", "delete", "namespace", NAMESPACE, "--wait", "--timeout=300s")
    volume = f"{NAMESPACE}-{FULL}-backup"
    held_pv = run("kubectl", "get", "pv", volume, check=False).returncode == 0
    if held_pv:
        # From meridian 0.1.47 restore deletes a backup volume a deleted
        # namespace left, within its own confirmation (the CLI's audit C4):
        # the plan names it, and nothing changes until the plan is approved.
        left = cli(workdir, "restore", files[0], "--identity", "identity.txt",
                   "-n", NAMESPACE, "--chart", "/chart")
        s.check(left.returncode == 1 and f"First it deletes the backup volume {volume}" in left.stderr
                and "not approved, so nothing was changed" in left.stderr,
                "a backup volume the deleted namespace left is named in the plan before anything changes")
        s.check(run("kubectl", "get", "namespace", NAMESPACE, check=False).returncode != 0,
                "and nothing was made")
        s.check(run("kubectl", "get", "pv", volume, check=False).returncode == 0,
                "and the volume is kept until the plan is approved")
    s.check(run("kubectl", "get", "namespace", NAMESPACE, check=False).returncode != 0,
            "the namespace and everything in it are gone")
    s.check(node_backups() == on_node, "the backup is still in the node's directory")

    print("G: restored by `meridian restore`", flush=True)
    wrong = cli(workdir, "restore", files[0], "-n", NAMESPACE, "--chart", "/chart")
    s.check(wrong.returncode == 2 and "--identity" in wrong.stderr, "without an identity it asks for one")
    asked = cli(workdir, "restore", files[0], "--identity", "identity.txt",
                "-n", NAMESPACE, "--chart", "/chart")
    s.check(asked.returncode == 1 and "not approved, so nothing was changed" in asked.stderr
            and "Restoring " in asked.stderr,
            "without a terminal and without --yes it shows the plan and changes nothing")
    s.check(run("kubectl", "get", "namespace", NAMESPACE, check=False).returncode != 0,
            "and the namespace was not made")
    done = cli(workdir, "restore", files[0], "--identity", "identity.txt",
               "-n", NAMESPACE, "--chart", "/chart", "--timeout", "10m", "--yes")
    s.check(done.returncode == 0, f"meridian restore: exit {done.returncode}")
    if held_pv:
        s.check(f"Deleted the backup volume {volume}" in done.stdout + done.stderr,
                "the volume the deleted namespace left is deleted first, as approved")
    s.check("every count as backed up" in done.stdout, "every table's row count as backed up")
    s.check("    config: " in done.stdout and "    book: " in done.stdout, "the counts by store")
    s.check(SECRET_VALUE not in done.stdout + done.stderr, "and it printed no secret")
    again = cli(workdir, "restore", files[0], "--identity", "identity.txt",
                "-n", NAMESPACE, "--chart", "/chart")
    s.check(again.returncode == 1 and "holds a release" in again.stderr,
            "restoring over the restored deployment is refused, nothing changed")

    print("H: what it holds now is what it held", flush=True)
    wait_for("every component ready", all_ready, seconds=300)
    # What the components read only when they start, as after first run: a
    # conductor still on the store it keeps in memory until it has a database
    # would answer for nobody, whatever the database holds.
    s.check(started_with_the_database(), "the conductor and the dashboard started with the database's URL")
    s.check(first_run_finished(), "first run finished, and holds no rights")
    after = held()
    for what in before:
        s.check(after[what] == before[what], f"the same {what}")
    s.check(deployment_key() == key_before, "the same deployment key, now from a Secret")
    if stores:
        claim = json.loads(out("get", "pvc", STORE_CLAIM, "-o", "json"))
        labels = claim["metadata"].get("labels", {})
        s.check(labels.get("meridian.dev/launched") == "true" and labels.get("meridian.dev/plugin") == STORE,
                "the plugin's storage is its claim again, the launcher's, for its plugin")
        s.check(store_held() == store_before, "the same plugin storage, file for file, owner and mode")
    # Waited for: the Job starts with the URL, and the components do not wait
    # for it here, since the restored schema is already the one they verify.
    def migrated():
        jobs = out("get", "jobs", "-o", "jsonpath={range .items[*]}{.metadata.name} {.status.succeeded}{\"\\n\"}{end}")
        return any(line.startswith(f"{FULL}-migrate-") and line.endswith(" 1") for line in jobs.splitlines())
    s.check(happens(migrated, 300), "the migration ran, once the database's URL was given")

    print("I: and the restored deployment backs up again", flush=True)
    for name in files:
        os.unlink(os.path.join(workdir, name))
    done = cli(workdir, "backup", "--release", RELEASE, "-n", NAMESPACE, "--to", "/work")
    s.check(done.returncode == 0, f"meridian backup after the restore: exit {done.returncode}")
    s.check(len(node_backups()) == 1, "the node keeps the newest one alone")
    # The restored database is a new one to the archive (its system
    # identifier is new): the stanza moved to it, and a base backup of it.
    s.check(happens(lambda: archive_said("the stanza moved to this database"), 120),
            "the archive moved to the restored database, a new one to it, keeping the one before as history")
    s.check(happens(lambda: "a full base backup taken" in newest_base_backup_log(), 180),
            "and a backup took a base backup of it into the archive")
    return verdict(s)


# ── J and K: what the core side of `meridian restore --to-time`, of
# `meridian restore --to-archive-end` and of `meridian upgrade-deployment
# --enable-checksums` does, driven here as the CLI drives it: every
# component and the database stopped, a Job made from the database's offline
# CronJob with the act named, and everything started again
# (templates/database-offline.yaml). ──


def archive_said(words):
    """Whether the archive's container beside the database said `words`."""
    done = kubectl("logs", f"{FULL}-database-0", "-c", "archive", "--tail=-1", check=False)
    return words in done.stdout.decode()


def newest_base_backup_log():
    """What the newest backup Job's archive step said."""
    jobs = json.loads(out("get", "jobs", "-l", "meridian.dev/component=backup", "-o", "json"))["items"]
    jobs.sort(key=lambda j: j["metadata"]["creationTimestamp"])
    for job in reversed(jobs):
        pods = out("get", "pods", "-l", f"job-name={job['metadata']['name']}", "-o", "name").split()
        for pod in pods:
            done = kubectl("logs", pod, "-c", "archive", check=False)
            if done.returncode == 0:
                return done.stdout.decode()
    return ""


def deployments():
    """Each of the release's Deployments, and the replicas it runs."""
    listed = json.loads(out("get", "deployments", "-l", f"app.kubernetes.io/instance={RELEASE}", "-o", "json"))
    return {d["metadata"]["name"]: d["spec"].get("replicas", 1) for d in listed["items"]}


def stopped(running):
    """Every component and the database stopped, as the CLI stops them, and
    their pods gone. The broker uses no database and keeps running."""
    for name in running:
        kubectl("scale", "deployment", name, "--replicas=0")
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=0")

    def gone():
        listed = json.loads(out("get", "deployments,statefulsets", "-l", f"app.kubernetes.io/instance={RELEASE}",
                                "-o", "json"))["items"]
        counted = [i for i in listed if i["kind"] == "Deployment" or i["metadata"]["name"] == f"{FULL}-database"]
        return all(not i["status"].get("replicas") for i in counted) and kubectl(
            "get", "pod", f"{FULL}-database-0", check=False).returncode != 0
    wait_for("every component and the database to stop", gone, seconds=300)


def started(running):
    """The database, then every component, started again and ready."""
    kubectl("scale", "statefulset", f"{FULL}-database", "--replicas=1")
    wait_for("the database", lambda: kubectl(
        "exec", f"{FULL}-database-0", "-c", "postgres", "--", "pg_isready", "-h", "127.0.0.1", "-U", "postgres",
        check=False).returncode == 0, seconds=300)
    for name, replicas in running.items():
        kubectl("scale", "deployment", name, f"--replicas={replicas}")
    wait_for("every component ready", all_ready, seconds=600)


def offline(act, **env):
    """A Job from the offline CronJob with the act named, as the CLI makes
    it, run to its end: whether it succeeded, and what it said."""
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
    log = kubectl("logs", f"job/{name}", "--tail=-1", check=False).stdout.decode()
    for line in log.splitlines():
        print(f"    | {line}", flush=True)
    return bool(status.get("succeeded")), log


def claim(s, failure, component, seconds, bound, extra=None):
    """A measurement, kept as the failover run keeps its own (366): the
    claims ledger is written from these lines."""
    line = {
        "profile": "tolerant", "failure": failure, "component": component,
        "recovered_s": round(seconds, 2), "bound_s": bound,
        "held": bound is None or seconds <= bound,
        "command": "make e2e-cluster-k3d E2E_CLUSTER_TARGET=e2e-cluster-backup",
        "run": os.environ.get("GITHUB_RUN_ID", "local"),
    }
    line.update(extra or {})
    print(f"CLAIM {json.dumps(line)}", flush=True)
    with open(os.environ.get("E2E_BACKUP_CLAIMS", ".e2e-backup-claims.jsonl"), "a", encoding="utf-8") as kept:
        kept.write(json.dumps(line) + "\n")
    if bound is not None:
        s.check(line["held"], f"{failure}: {component} in {seconds:.1f}s, within {bound}s")


PROBE = "e2e_point_in_time"


def probe_rows():
    return psql(f"select n || '|' || at::text || '|' || said from {PROBE} order by n")


def archived_now():
    """The segment holding the last commit archived, as archive_timeout would
    within backup.walArchive.timeoutSeconds."""
    wal = psql("select pg_walfile_name(pg_switch_wal())")
    wait_for("the last segment to be archived", lambda: psql(
        "select last_archived_wal from pg_stat_archiver") >= wal, seconds=120)
    return wal


def now_at_the_database():
    """The database's clock, as the act takes a time."""
    return psql("select to_char(clock_timestamp() at time zone 'UTC', 'YYYY-MM-DD HH24:MI:SS.US') || '+00'")


def timeline_now():
    return psql("select timeline_id from pg_control_checkpoint()")


def holds(log):
    """The archive's first and last times, as a refusal names them."""
    found = re.search(r"which holds (\d{4}-\d\d-\d\d [\d:.]+\+00) to (\d{4}-\d\d-\d\d [\d:.]+\+00)", log)
    return found.groups() if found else None


def restored_to_a_time(s, before):
    """`meridian restore --to-time` twice across timelines, then `--to-archive-
    end`, as the CLI drives them (step 3c; spec section 9: the timeline is
    chosen, never asked): to a time T1; written to on T1's new timeline; to
    a time T0 before T1, which only the earlier timeline held; written to
    again; refused for times outside the archive, naming its first and last
    times; and to the archive's end, the newest timeline's last commit. Rows
    exact each time."""
    print("J: restored to a time between two commits, from the WAL archive", flush=True)
    s.check(archive_said("the stanza holds this database"), "the archive's stanza holds the database")
    s.check("a full base backup taken" in newest_base_backup_log(),
            "the backup took a full base backup into the archive")
    psql(f"create table {PROBE} (n int primary key, at timestamptz not null default clock_timestamp(), "
         "said text not null)")
    for n in (1, 2, 3):
        psql(f"insert into {PROBE} (n, said) values ({n}, 'before')")
    time.sleep(1.5)
    t0 = now_at_the_database()
    time.sleep(1.5)
    held_at_t0 = probe_rows()
    for n in (4, 5, 6):
        psql(f"insert into {PROBE} (n, said) values ({n}, 'between')")
    time.sleep(1.5)
    to = now_at_the_database()
    time.sleep(1.5)
    held_at_the_time = probe_rows()
    for n in (7, 8, 9):
        psql(f"insert into {PROBE} (n, said) values ({n}, 'after')")
    s.check(len(probe_rows().splitlines()) == 9, "nine rows committed, three either side of each of two times")
    s.check(True, f"the WAL through {archived_now()} is archived")
    running = deployments()
    began = time.time()
    stopped(running)
    ok, log = offline("restore-to-time", MERIDIAN_RESTORE_TO_TIME=to)
    s.check(ok, f"the offline Job restored the database to {to}")
    s.check("promoted at" in log and "on timeline" in log, "and said where it was promoted")
    s.check("along timeline 1, the one that held it" in log, "along timeline 1, the one that held the time")
    started(running)
    took = time.time() - began
    s.check(probe_rows() == held_at_the_time and len(held_at_the_time.splitlines()) == 6,
            "row for row as it stood at the time: the six before it, none after")
    s.check(held()["book"] == before["book"], "the book as it was")
    # The region row (ruling 1048: a region lost, back within 2 h); what a
    # restore into a new cluster can lose is what the archive had not yet
    # taken, at most backup.walArchive.timeoutSeconds (60 s here, 5 min asked).
    claim(s, "restore-to-time", "database", took, 7200, {"rpo_s": 60, "rpo_bound_s": 300})
    first_restore = timeline_now()
    s.check(first_restore not in ("", "1"), f"on a timeline of its own ({first_restore})")
    wait_for("the archive to take the new timeline", lambda: archive_said("the stanza holds this database"),
             seconds=60)

    print("J2: written to on the new timeline, then restored to an earlier time the first timeline held",
          flush=True)
    for n in (10, 11, 12):
        psql(f"insert into {PROBE} (n, said) values ({n}, 'on the restored timeline')")
    s.check(True, f"three rows on timeline {first_restore}, the WAL through {archived_now()} archived")
    running = deployments()
    stopped(running)
    ok, log = offline("restore-to-time", MERIDIAN_RESTORE_TO_TIME=t0)
    s.check(ok, f"the offline Job restored the database to {t0}, before the first restore's time")
    s.check("along timeline 1, the one that held it" in log,
            f"along timeline 1, which held {t0}, not the newest ({first_restore}), with no option asked")
    started(running)
    s.check(probe_rows() == held_at_t0 and len(held_at_t0.splitlines()) == 3,
            "row for row as it stood at the earlier time: the three before it")
    s.check(held()["book"] == before["book"], "the book as it was")
    second_restore = timeline_now()
    s.check(second_restore not in ("", "1", first_restore), f"on a timeline of its own again ({second_restore})")
    wait_for("the archive to take the new timeline", lambda: archive_said("the stanza holds this database"),
             seconds=60)

    print("J3: times outside the archive refused, naming its first and last; then restored to its end",
          flush=True)
    psql(f"insert into {PROBE} (n, said) values (13, 'after the second restore')")
    at_the_end = probe_rows()
    s.check(True, f"one row on timeline {second_restore}, the WAL through {archived_now()} archived")
    # Read while the database still answers.
    past = psql("select to_char((clock_timestamp() + interval '1 day') at time zone 'UTC', "
                "'YYYY-MM-DD HH24:MI:SS.US') || '+00'")
    running = deployments()
    stopped(running)
    ok, log = offline("restore-to-time", MERIDIAN_RESTORE_TO_TIME=past)
    named = holds(log)
    s.check(not ok and "is outside the archive" in log and named is not None,
            f"a time past the archive, {past}, refused as outside it, naming its first and last times ({named})")
    s.check(named is not None and named[0] < t0 and to < named[1] < past,
            "its first time before the earliest time restored to, its last after the latest")
    s.check("the database's volume is as it was" in log, "refused before the volume was touched")
    ok, log = offline("restore-to-time", MERIDIAN_RESTORE_TO_TIME="2000-01-01 00:00:00+00")
    s.check(not ok and "is before its oldest base backup ended" in log and holds(log) == named,
            "a time before the oldest base backup refused the same, naming the same first and last times")
    ok, log = offline("restore-to-archive-end")
    s.check(ok and "promoted at the archive's end" in log, "the offline Job restored the database to the archive's end")
    s.check(f"along timeline {second_restore}" in log, f"along the newest timeline, {second_restore}")
    started(running)
    s.check(probe_rows() == at_the_end, "row for row as it stood when it stopped, the second restore's row with it")
    s.check(held()["book"] == before["book"], "the book as it was")
    s.check(timeline_now() not in ("", "1", first_restore, second_restore), "on a timeline of its own")
    wait_for("the archive to take the new timeline", lambda: archive_said("the stanza holds this database"),
             seconds=60)


def checksums_enabled(s, before):
    print("K: page checksums turned on in a database made without them", flush=True)
    s.check(psql("show data_checksums") == "on", "a database this chart makes has page checksums")
    rows = probe_rows()
    running = deployments()
    stopped(running)
    # A database made before step 3b, as `meridian upgrade-deployment` finds
    # one: its checksums turned off here, on the stopped volume.
    pod = {
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": "e2e-checksums-off", "namespace": NAMESPACE},
        "spec": {
            "restartPolicy": "Never",
            "containers": [{"name": "off", "image": POSTGRES_IMAGE, "imagePullPolicy": "IfNotPresent",
                            "command": ["su-exec", "postgres", "pg_checksums", "--disable",
                                        "-D", "/var/lib/postgresql/data/pgdata"],
                            "volumeMounts": [{"name": "data", "mountPath": "/var/lib/postgresql/data"}]}],
            "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": f"data-{FULL}-database-0"}}],
        },
    }
    kubectl("create", "-f", "-", input=json.dumps(pod).encode())
    wait_for("the checksums to be turned off", lambda: out(
        "get", "pod", "e2e-checksums-off", "-o", "jsonpath={.status.phase}") in ("Succeeded", "Failed"), seconds=180)
    s.check(out("get", "pod", "e2e-checksums-off", "-o", "jsonpath={.status.phase}") == "Succeeded",
            "page checksums turned off on the stopped volume, as a database made before 3b has none")
    kubectl("delete", "pod", "e2e-checksums-off", "--wait")
    began = time.time()
    ok, log = offline("enable-checksums")
    took = time.time() - began
    s.check(ok and "page checksums enabled" in log, "the offline Job turned page checksums on")
    size = re.search(r"checksums on (\d+) KiB", log)
    claim(s, "enable-checksums", "database", took, None, {"database_kib": int(size.group(1)) if size else 0})
    again, log = offline("enable-checksums")
    s.check(again and "on already" in log, "and a second run finds them on and changes nothing")
    started(running)
    s.check(psql("show data_checksums") == "on", "the database reports page checksums on")
    s.check(probe_rows() == rows and held()["book"] == before["book"], "every row as it was")


def verdict(s):
    print(flush=True)
    if s.failures:
        print(f"e2e-cluster-backup FAILED: {len(s.failures)}", flush=True)
        for failure in s.failures:
            print(f"  - {failure}", flush=True)
        return 1
    print("e2e-cluster-backup OK", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
