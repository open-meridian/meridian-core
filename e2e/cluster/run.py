"""First run on a real cluster, against a real platform, with nobody in it.

Path 1 of the five in spec/installation-and-first-run: a database this
deployment brings, and a platform running beside it. Every other end-to-end
test here stands something in -- a Kubernetes that records calls without
performing them, a platform that implements three endpoints in Python. This
one stands in nothing, and that is the point: what it proves is the wiring
none of the others can reach, where the first-run Job writes a name into a
Secret, the chart hands it to the conductor as an environment variable, and
the conductor writes a permission when it restarts.

It needs a cluster and the platform's compose, and it needs no person: the
deployment is registered, its enrolment code issued and its first-run code
issued by management commands rather than by somebody clicking.

Run by `make e2e-cluster`.
"""

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

NAMESPACE = os.environ.get("E2E_NAMESPACE", "meridian-e2e")
RELEASE = os.environ.get("E2E_RELEASE", "trial")
CHART = os.environ.get("E2E_CHART", "deploy/chart")
IMAGE = os.environ.get("E2E_IMAGE", "")
PLATFORM_DIR = os.environ.get("PLATFORM", "../meridian-platform")

# What a pod calls the machine this runs on. The platform's compose publishes
# 9290, and a k3s pod resolves this name to the host.
PLATFORM_FROM_POD = os.environ.get("E2E_PLATFORM_FROM_POD", "http://host.docker.internal:9290")
PLATFORM_FROM_HERE = os.environ.get("E2E_PLATFORM_FROM_HERE", "http://127.0.0.1:9290")

# A new deployment every run, because a deployment that has enrolled cannot
# enrol again: `issue_enrolment_code` refuses one that already holds a key,
# deliberately, and `register_deployment` is idempotent on the name. Reusing
# the name meant the second run asked for a code against the first run's
# deployment and was told no -- correctly.
RUN = os.environ.get("E2E_RUN", str(int(time.time())))

# Which of the wizard's two database routes this run takes. `brought` starts
# the Postgres the chart ships; `external` points at one somebody already
# runs, which here is a container outside the cluster -- a firm's own
# database, a managed one from a cloud, or one in Docker like this.
ROUTE = os.environ.get("E2E_DB_ROUTE", "brought")
EXTERNAL_HOST = os.environ.get("E2E_EXTERNAL_HOST", "host.docker.internal")
EXTERNAL_PORT = os.environ.get("E2E_EXTERNAL_PORT", "15440")
EXTERNAL_CONTAINER = os.environ.get("E2E_EXTERNAL_CONTAINER", "meridian-e2e-database")
EXTERNAL_PASSWORD = os.environ.get("E2E_EXTERNAL_PASSWORD", "e2e-dev-only")

# How people sign in, which is the other half of what the wizard asks. The
# database route and this are independent: each branch is the same install
# with different answers, so a failure says which half it came from.
#
# `local` is an account this deployment holds. `ldap` is the firm's directory,
# run as a container outside the cluster exactly as the external database is:
# from in here a firm's LDAP is a host and a port. The tree is the compose
# suite's (e2e/dashboard/ldap), so both suites mean the same people.
SIGN_IN = os.environ.get("E2E_SIGN_IN", "local")
LDAP_SERVER = os.environ.get("E2E_LDAP_SERVER", "ldap://host.docker.internal:15389")
LDAP_GROUP_A = "cn=ldap-group-a,ou=groups,dc=example,dc=org"  # alice and bob
LDAP_GROUP_B = "cn=ldap-group-b,ou=groups,dc=example,dc=org"  # bob alone

WAYS_IN = {
    "local": {
        "answers": {
            "directory": "local",
            "admin_login": "ada",
            "admin_email": "ada@example.org",
            "admin_given_name": "Ada",
            "admin_password": "Password1!",
            "admin_group": "",
        },
        # What G finds in the user group, and who H signs in as.
        "granted": "local|ada",
        "administrator": ("ada", "Password1!"),
        "somebody_else": None,
    },
    "ldap": {
        "answers": {
            "directory": "ldap",
            "ldap_servers": LDAP_SERVER,
            "ldap_base_dn": "ou=people,dc=example,dc=org",
            "ldap_bind_dn": "cn=admin,dc=example,dc=org",
            "ldap_bind_password": "ldap-admin-dev-only",
            # A group only bob is in, so that alice -- who is in the
            # directory and may sign in -- is the case where a person gets in
            # and is not an administrator. Every person in the other group
            # would be one, which proves nothing about the group.
            "admin_group": LDAP_GROUP_B,
        },
        "granted": LDAP_GROUP_B,
        "administrator": ("bob", "bobpass"),
        "somebody_else": ("alice", "alicepass"),
    },
}
if SIGN_IN not in WAYS_IN:
    raise SystemExit(f"E2E_SIGN_IN is {SIGN_IN!r}; it is one of {sorted(WAYS_IN)}")
WAY_IN = WAYS_IN[SIGN_IN]

PORT = int(os.environ.get("E2E_PORT", "18480"))
WIZARD = f"http://127.0.0.1:{PORT}"


class Scorecard:
    def __init__(self):
        self.failures = []

    def check(self, held, said):
        print(f"    {'ok' if held else 'FAILED'}: {said}", flush=True)
        if not held:
            self.failures.append(said)

    def note(self, said):
        print(f"    {said}", flush=True)


def run(*command, **kwargs):
    """A command that must work, with its output returned."""
    done = subprocess.run(command, capture_output=True, text=True, **kwargs)
    if done.returncode != 0:
        raise SystemExit(
            f"{' '.join(command)} failed ({done.returncode})\n{done.stdout}\n{done.stderr}"
        )
    return done.stdout.strip()


def kubectl(*args):
    return run("kubectl", "--namespace", NAMESPACE, *args)


def psql(sql, database="meridian"):
    """A query against whichever database this deployment was given.

    Returns "" when it cannot be answered yet. Waiting for a schema means
    asking for a table that does not exist, and a helper that raised there
    turned "not yet" into "stop" -- which is how the first run of this ended,
    on a relation the conductor had not made.
    """
    if ROUTE == "brought":
        where = ["kubectl", "--namespace", NAMESPACE, "exec",
                 f"{RELEASE}-meridian-runtime-database-0", "--"]
    else:
        where = ["docker", "exec", EXTERNAL_CONTAINER]
    done = subprocess.run(
        [*where, "psql", "-U", "postgres", "-d", database, "-Atc", sql],
        capture_output=True,
        text=True,
    )
    return done.stdout.strip() if done.returncode == 0 else ""


def platform(*args):
    """A management command in the platform's own compose."""
    return run(
        "docker",
        "compose",
        "--project-directory",
        PLATFORM_DIR,
        "-f",
        f"{PLATFORM_DIR}/docker-compose.yaml",
        "run",
        "--rm",
        "-T",
        "site",
        "python",
        "-m",
        "django",
        *args,
        "--settings",
        "platform_site.web.settings",
    )


def platform_psql(sql):
    """A query against the platform's own database."""
    return run(
        "docker", "compose", "--project-directory", PLATFORM_DIR,
        "-f", f"{PLATFORM_DIR}/docker-compose.yaml",
        "exec", "-T", "postgres", "psql", "-U", "meridian", "-d", "platform", "-Atc", sql,
    )


def post(path, fields, cookies):
    import urllib.parse

    data = urllib.parse.urlencode(fields).encode()
    request = urllib.request.Request(f"{WIZARD}{path}", data=data, method="POST")
    request.add_header("Content-Type", "application/x-www-form-urlencoded")
    if cookies:
        request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in cookies.items()))

    class NoRedirects(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *_a, **_k):
            return None

    opener = urllib.request.build_opener(NoRedirects)
    try:
        answered = opener.open(request)
        status, body, headers = answered.status, answered.read().decode(), answered.headers
    except urllib.error.HTTPError as refused:
        status, body, headers = refused.code, refused.read().decode(), refused.headers
    for value in headers.get_all("Set-Cookie") or []:
        name, _, rest = value.partition("=")
        cookies[name] = rest.split(";")[0]
    return status, body


def get(path, cookies):
    request = urllib.request.Request(f"{WIZARD}{path}")
    request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in cookies.items()))
    try:
        return urllib.request.urlopen(request).read().decode()
    except urllib.error.HTTPError as refused:
        return refused.read().decode()


def wait_for(what, ready, seconds=420):
    for _ in range(seconds):
        try:
            if ready():
                return
        except Exception:
            pass
        time.sleep(1)
    raise SystemExit(f"{what} never happened")


def main():
    s = Scorecard()

    wait_for(
        "the platform",
        lambda: urllib.request.urlopen(f"{PLATFORM_FROM_HERE}/health").status == 200,
        seconds=120,
    )

    print("A: a deployment on the platform, with no key and no person", flush=True)
    deployment_id = platform(
        "register_deployment", "--organisation", "E2E", "--deployment", f"{RELEASE}-{RUN}"
    ).splitlines()[-1]
    s.check(deployment_id.startswith("DEP-"), f"registered {deployment_id}")
    enrolment_code = platform(
        "issue_enrolment_code", "--deployment", deployment_id
    ).splitlines()[-1]
    s.check(enrolment_code.startswith("ENR-"), "and it has an enrolment code to carry")

    print("B: installed, carrying only those two values", flush=True)
    run("kubectl", "create", "namespace", NAMESPACE)
    values = [
        "--set", f"deployment.id={deployment_id}",
        "--set", f"deployment.enrolmentCode={enrolment_code}",
        "--set", f"platform.address={PLATFORM_FROM_POD}",
    ]
    if IMAGE:
        repository, _, tag = IMAGE.rpartition(":")
        values += ["--set", f"image.repository={repository}", "--set", f"image.tag={tag}"]
    run("helm", "upgrade", "--install", RELEASE, CHART, "--namespace", NAMESPACE, *values)

    wait_for(
        "the dashboard",
        lambda: "1/1" in kubectl("get", "pods", "-l", "meridian.dev/component=dashboard", "--no-headers"),
    )
    s.check(True, "the dashboard is up")

    forward = subprocess.Popen(
        ["kubectl", "--namespace", NAMESPACE, "port-forward",
         f"svc/{RELEASE}-meridian-runtime-dashboard", f"{PORT}:80"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        wait_for("the forward", lambda: urllib.request.urlopen(f"{WIZARD}/healthz").status == 200)

        print("C: the conductor enrolled its own key", flush=True)
        wait_for(
            "enrolment",
            lambda: "not registered"
            not in urllib.request.urlopen(f"{WIZARD}/first-run").read().decode(),
            seconds=180,
        )
        page = urllib.request.urlopen(f"{WIZARD}/first-run").read().decode()
        # Requirement 8, and the reason it exists: an administrator comparing
        # the two is how a code somebody else spent is noticed. Both sides are
        # asked here, which no other test does -- the stand-in platform simply
        # returns a fingerprint the runner already knows.
        held = platform_psql(
            f"select fingerprint from domain_deploymentkey "
            f"where deployment_id = '{deployment_id}'"
        )
        s.note(f"the platform holds {held}")
        s.check(bool(held), "the platform holds a key nobody handled")
        s.check(held in page, "and the wizard shows that same fingerprint")

        print("D: the wizard, opened with a first-run code", flush=True)
        first_run_code = platform(
            "issue_claim_code", "--deployment", deployment_id, "--purpose", "first-run"
        ).splitlines()[-1]
        cookies = {}
        status, page = post("/first-run/claim", {"code": first_run_code}, cookies)
        # Redeeming is a signed call through the conductor, so a yes here is
        # the platform verifying a signature against the key it registered.
        # Nothing else in this run proves enrolment as directly.
        s.check(status == 303, f"the code is redeemed, so the signature held: {status}")

        print(
            f"E: applied, on a database it {'brings' if ROUTE == 'brought' else 'was pointed at'}, "
            f"signing people in with {'an account it holds' if SIGN_IN == 'local' else 'the firm LDAP'}",
            flush=True,
        )
        answers = {
            "db_route": ROUTE,
            "db_name": "meridian",
            "db_serving_role": "meridian_app",
            "db_migrating_role": "meridian_migrate",
            "backend": "bundled",
            **WAY_IN["answers"],
            **(
                {}
                if ROUTE == "brought"
                else {
                    # A database somebody already runs, reached by the name a
                    # pod can resolve. Its two roles were made before any of
                    # this, as a firm's own database administrator would.
                    "db_host": EXTERNAL_HOST,
                    "db_port": EXTERNAL_PORT,
                    "db_sslmode": "disable",
                    "db_serving_password": EXTERNAL_PASSWORD,
                    "db_migrating_password": EXTERNAL_PASSWORD,
                }
            ),
            "dashboard_url": WIZARD,
        }
        status, page = post("/first-run/check", answers, cookies)
        s.check("passed" in page or "passes" in page, "the answers pass")
        status, page = post("/first-run/apply", answers, cookies)
        if "administers this deployment" not in page:
            import re as _re

            s.note(f"page: {_re.sub(r'<[^>]*>', ' ', page)[:400]}")
        s.check("administers this deployment" in page, "applied, and it names who administers it")
    finally:
        forward.terminate()
        # Gone before H binds the same port again.
        forward.wait()

    print("F: the database", flush=True)
    if ROUTE == "brought":
        wait_for(
            "the database",
            lambda: "1/1"
            in kubectl("get", "pods", "-l", "meridian.dev/component=database", "--no-headers"),
            seconds=300,
        )
        s.check(True, "a Postgres that did not exist before the wizard is running")
    else:
        s.check(
            kubectl("get", "pods", "-l", "meridian.dev/component=database", "--no-headers") == "",
            "the Postgres this chart could have brought was left at zero, unasked for",
        )
    roles = psql("select rolname from pg_roles where rolname like 'meridian\\_%'")
    s.note(f"roles: {roles.split()}")
    s.check(
        "meridian_app" in roles and "meridian_migrate" in roles,
        "with both roles" + (" made" if ROUTE == "brought" else " it was pointed at"),
    )
    s.check(
        psql("select has_schema_privilege('meridian_app','public','CREATE')") == "f",
        "and the serving role may not create tables",
    )

    print("G: the administrator the wizard named", flush=True)
    # The thing no other test here can reach: the Job wrote the name into a
    # Secret, the chart handed it to the conductor, and the conductor wrote
    # the permission when it restarted onto the store it had just been given.
    wait_for(
        "the permission",
        lambda: "deployment-admin" in psql("select access_group_id from config_permission"),
        seconds=300,
    )
    groups = psql("select logins, directory_groups from config_user_group")
    s.note(f"user groups: {groups}")
    s.check(
        WAY_IN["granted"] in groups,
        f"{WAY_IN['granted']}, as the wizard named it, holds deployment admin",
    )

    print("H: and they sign in, which is what the permission was for", flush=True)
    # G is a row. A row was what the compose run asserted for weeks while the
    # account it named did not exist, so nobody could have used it. This is
    # the dashboard in its target configuration, on this cluster, checking a
    # password against the hash the Job wrote and the store the chart gave it.
    # A new forward, because the one above went to a container apply
    # restarted -- and started again whenever it dies, since the dashboard
    # restarts once more when the conductor it reads from does, and a forward
    # to a restarted container exits rather than reconnecting. Waiting on a
    # dead one was 300 seconds of asking a closed port.
    forward = None

    def signing_in():
        nonlocal forward
        if forward is None or forward.poll() is not None:
            forward = subprocess.Popen(
                ["kubectl", "--namespace", NAMESPACE, "port-forward",
                 f"svc/{RELEASE}-meridian-runtime-dashboard", f"{PORT}:80"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            time.sleep(2)
        return 'name="password"' in urllib.request.urlopen(f"{WIZARD}/sign-in").read().decode()

    try:
        wait_for("the dashboard, out of first run", signing_in, seconds=300)
        name, password = WAY_IN["administrator"]
        status, _ = post("/sign-in", {"name": name, "password": "not-the-password"}, {})
        s.check(status == 401, f"a wrong password for {name} is refused: {status}")
        session = {}
        status, _ = post("/sign-in", {"name": name, "password": password}, session)
        s.check(status == 303 and bool(session), f"the right one is not, and {name} holds a session: {status}")
        s.check("You are a deployment admin" in get("/", session), f"and home says {name} administers it")
        if WAY_IN["somebody_else"]:
            # In the directory, so in; not in the group, so not an
            # administrator. Without this, a permission granted to everybody
            # who signs in passes every check above.
            name, password = WAY_IN["somebody_else"]
            session = {}
            status, _ = post("/sign-in", {"name": name, "password": password}, session)
            s.check(status == 303 and bool(session), f"{name} signs in too: {status}")
            s.check(
                "You are a deployment admin" not in get("/", session),
                f"and is not an administrator, being outside the group the wizard named",
            )
    finally:
        if forward is not None:
            forward.terminate()
            forward.wait()

    print(flush=True)
    if s.failures:
        print(f"e2e-cluster FAILED: {len(s.failures)}", flush=True)
        for failure in s.failures:
            print(f"  - {failure}", flush=True)
        return 1
    print("e2e-cluster OK", flush=True)
    return 0


sys.exit(main())
