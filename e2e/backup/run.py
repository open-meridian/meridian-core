"""Backup and restore, proved on a scratch cluster.

Task kernel/a-deployment-is-backed-up-and-restored: a deployment holding
people, their access, plugin settings with a secret one sealed, and a book,
backed up by the chart's CronJob on its schedule and by `meridian backup
--to`; then wiped, as a reset of Rancher Desktop wipes a laptop's cluster,
leaving only the directory the backups were written to; then restored by
`meridian restore`, and held to what it was: the same people and access, the
same settings, the secret opening with the restored settings key to the
value it was sealed from, the same book line for line, the same keys, every
component ready, and a backup taken again after.

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
CHART = os.path.abspath(os.environ.get("E2E_CHART", "deploy/chart"))
BOOK_SQL = os.path.abspath("deploy/harness/book.sql")
# A directory on the node, as a laptop's would be under the Mac's home: what
# survives the wipe.
BACKUP_PATH = os.environ.get("E2E_BACKUP_PATH", "/var/lib/meridian-backup-e2e")
PLUGIN = "e2e-plugin"
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
        "exec", "-i", f"{FULL}-database-0", "--",
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
        "--set", "backup.keep=1")
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
    def completed():
        """The scheduled runs that finished, by name, oldest first."""
        jobs = json.loads(out("get", "jobs", "-l", "meridian.dev/component=backup", "-o", "json"))["items"]
        done = [j for j in jobs if j["status"].get("succeeded")
                and not j["metadata"].get("annotations", {}).get("cronjob.kubernetes.io/instantiate")]
        done.sort(key=lambda j: j["status"].get("completionTime", ""))
        return [j["metadata"]["name"] for j in done]
    wait_for("a scheduled backup", lambda: bool(completed()), seconds=200)
    s.check(True, "the CronJob wrote a backup on its schedule")
    # The completed run's own log: a later scheduled run may be mid-flight,
    # and the CronJob's label reaches its pod too, which has not yet said
    # what it wrote (flaked 2026-10-25, run 40235294905).
    finished = completed()[0]
    pods = out("get", "pods", "-l", f"job-name={finished}",
               "--field-selector=status.phase=Succeeded", "-o", "name").split()
    log = out("logs", pods[0], "--tail=-1") if pods else ""
    s.check("backup: wrote " in log and "tables" in log, "and said what it wrote, names and counts only")
    s.check(SECRET_VALUE not in log and recipient not in log, "and printed no secret")
    kubectl("patch", "cronjob", f"{FULL}-backup", "--type=merge", "-p", '{"spec":{"suspend":true}}')
    wait_for("the scheduled runs to finish", lambda: not out(
        "get", "pods", "-l", "meridian.dev/component=backup",
        "--field-selector=status.phase=Running", "-o", "name"), seconds=120)
    # What the backup below holds, read just before it.
    before = held()
    done = cli(workdir, "backup", "--release", RELEASE, "-n", NAMESPACE, "--to", "/work")
    s.check(done.returncode == 0, f"meridian backup --to: exit {done.returncode}")
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
    return verdict(s)


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
