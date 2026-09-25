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

# The firm's own provider, stood in for by e2e/dashboard/fake_idp.py outside
# the cluster. Its issuer is one name for the pod and for the browser, as a
# real one's is: the pod resolves host.docker.internal, and this runner --
# the browser -- connects to 127.0.0.1 for that name (see `fetch`).
IDP_ISSUER = os.environ.get("E2E_IDP_ISSUER", "http://host.docker.internal:18100")

WAYS_IN = {
    "local": {
        "answers": {
            "backend": "local",
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
        "by": "password",
    },
    "ldap": {
        "answers": {
            "backend": "ldap",
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
        "by": "password",
    },
    "oidc": {
        "answers": {
            "backend": "oidc",
            "oidc_issuer": IDP_ISSUER,
            "oidc_client_id": "meridian-dashboard",
            "oidc_client_secret": "idp-dev-only-secret",
            # Empty is the default claim, `groups`, which the stand-in uses.
            "oidc_groups_claim": "",
            "admin_group": "meridian-admins",
        },
        "granted": "meridian-admins",
        # Nobody types a password here: the provider decides who they are.
        # Ada is its default person, in meridian-admins; Ben is in staff.
        "administrator": ("", None),
        "somebody_else": ("ben", None),
        "by": "redirect",
    },
}
if SIGN_IN not in WAYS_IN:
    raise SystemExit(f"E2E_SIGN_IN is {SIGN_IN!r}; it is one of {sorted(WAYS_IN)}")
WAY_IN = WAYS_IN[SIGN_IN]

PORT = int(os.environ.get("E2E_PORT", "18480"))

# Who installs the chart and answers the wizard: this runner (`runner`), or
# `meridian up --params`, as a person does (`cli`), from E2E_CLI_IMAGE --
# meridian-cli's `e2e` image, the binary with the helm and kubectl it drives.
DRIVER = os.environ.get("E2E_DRIVER", "runner")
CLI_IMAGE = os.environ.get("E2E_CLI_IMAGE", "meridian-cli-e2e:local")

# Headless Chromium, built beside the runtime image (e2e/cluster/browser.py).
BROWSER_IMAGE = os.environ.get("E2E_BROWSER_IMAGE", "meridian-e2e-browser:local")
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


def fetch(method, url, cookies, fields=None):
    """One request, as a browser would make it, following nothing.

    `host.docker.internal` is what the pod calls this machine, and a real
    provider's issuer is one name everybody resolves. This machine does not
    resolve that one, so the connection goes to 127.0.0.1 and the name stays
    in the Host header -- a hosts entry, in effect, and the issuer string the
    dashboard checks is the same on both sides.
    """
    import urllib.parse

    parts = urllib.parse.urlsplit(url)
    target = url
    headers = {}
    if parts.hostname == "host.docker.internal":
        target = urllib.parse.urlunsplit(parts._replace(netloc=f"127.0.0.1:{parts.port}"))
        headers["Host"] = parts.netloc
    data = urllib.parse.urlencode(fields).encode() if fields is not None else None
    request = urllib.request.Request(target, data=data, method=method, headers=headers)
    if cookies:
        request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in cookies.items()))

    class NoRedirects(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *_a, **_k):
            return None

    try:
        answered = urllib.request.build_opener(NoRedirects).open(request)
    except urllib.error.HTTPError as refused:
        answered = refused
    for value in answered.headers.get_all("Set-Cookie") or []:
        name, _, rest = value.partition("=")
        cookies[name] = rest.split(";")[0]
    return answered.status if hasattr(answered, "status") else answered.code, \
        answered.headers.get("Location"), answered.read().decode()


def sign_in_at_provider(hint):
    """Start at the dashboard, sign in at the provider, and come back.

    Returns the callback's status and the dashboard's cookies. The provider
    is given none of them, as a browser would give it none.
    """
    session = {}
    status, away, _ = fetch("GET", f"{WIZARD}/sign-in", session)
    if status not in (302, 303) or "/authorize" not in (away or ""):
        return status, session
    if hint:
        away += f"&login_hint={hint}"
    status, back, _ = fetch("GET", away, {})
    if status not in (302, 303) or not back:
        return status, session
    status, _, _ = fetch("GET", back, session)
    return status, session


def the_answers():
    """What the wizard is told, whichever driver tells it.

    One place, because the runner and `meridian up --params` must answer the
    same questions the same way, or a difference between them is a difference
    in what was asked rather than in who asked it.
    """
    return {
        "db_route": ROUTE,
        "db_name": "meridian",
        "db_serving_role": "meridian_app",
        "db_migrating_role": "meridian_migrate",
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


def by_cli(s, deployment_id, enrolment_code):
    """Install and answer the wizard with `meridian up --params`, as a person does.

    Result 1 of plans/a-person-reaches-a-plugin, and spec/the-cli's `e2e-up`:
    the chart installed and the wizard answered by the CLI rather than by this
    runner's own HTTP calls. It runs in a container holding the binary and the
    helm and kubectl it drives, on this machine's network, so its kubeconfig
    and its port-forward are this machine's.

    `--no-doctor`: the doctor asks the registry whether the image exists, and
    this image is the working tree's, built here and in no registry. Every
    other check is the machine's, which the rest of this run proves anyway.
    """
    import tempfile

    first_run_code = platform(
        "issue_claim_code", "--deployment", deployment_id, "--purpose", "first-run"
    ).splitlines()[-1]

    # Credentials never go in the params file -- the CLI refuses one that holds
    # any -- so each comes from MERIDIAN_<FIELD>, as a person's would.
    answers = the_answers()
    secret = {k: v for k, v in answers.items() if k.endswith(("password", "secret"))}
    plain = {k: v for k, v in answers.items() if k not in secret}
    params = tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False)
    params.write("".join(f"{k}: {json.dumps(v)}\n" for k, v in plain.items()))
    params.close()
    os.chmod(params.name, 0o644)

    kubeconfig = os.environ.get("KUBECONFIG") or os.path.expanduser("~/.kube/config")
    environment = {
        "MERIDIAN_ENROLMENT_CODE": enrolment_code,
        "MERIDIAN_FIRST_RUN_CODE": first_run_code,
        **{f"MERIDIAN_{k.upper()}": v for k, v in secret.items()},
    }
    command = [
        "docker", "run", "--rm", "--network", "host",
        "--add-host", "host.docker.internal:host-gateway",
        "-v", f"{kubeconfig}:/kube/config:ro", "-e", "KUBECONFIG=/kube/config",
        "-v", f"{os.path.abspath(CHART)}:/chart:ro",
        "-v", f"{params.name}:/params.yaml:ro",
        *[flag for name in environment for flag in ("-e", name)],
        CLI_IMAGE, "up",
        "--namespace", NAMESPACE, "--release", RELEASE, "--chart", "/chart",
        "--id", deployment_id, "--platform", PLATFORM_FROM_POD,
        *(["--image", IMAGE] if IMAGE else []),
        "--params", "/params.yaml", "--port", str(PORT), "--no-doctor",
    ]
    done = subprocess.run(
        command, capture_output=True, text=True, env={**os.environ, **environment}
    )
    os.unlink(params.name)
    for line in (done.stdout + done.stderr).splitlines():
        print(f"    | {line}", flush=True)
    s.check(done.returncode == 0, f"meridian up finished: exit {done.returncode}")
    s.check(
        "administers this deployment" in done.stdout,
        "and said who administers the deployment it configured",
    )
    held = platform_psql(
        f"select fingerprint from domain_deploymentkey where deployment_id = '{deployment_id}'"
    )
    s.check(bool(held), "the platform holds the key the conductor enrolled")


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

    if DRIVER == "cli":
        print("B-E: meridian up, answering the wizard from a file", flush=True)
        by_cli(s, deployment_id, enrolment_code)
    else:
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
                f"signing people in with {dict(local='an account it holds', ldap='the firm LDAP', oidc='the firm provider')[SIGN_IN]}",
                flush=True,
            )
            answers = the_answers()
            passes = "Everything answered so far passes"
            if SIGN_IN == "ldap":
                # The check binds to the directory from the Job, in the cluster,
                # as the dashboard will. Until 2026-09-25 it dialled nothing and
                # passed this; a wrong password was found by the first sign-in.
                wrong = {**answers, "ldap_bind_password": "not-the-bind-password"}
                status, page = post("/first-run/check", wrong, cookies)
                s.check(
                    passes not in page and "could not sign in to the directory" in page,
                    "a wrong bind password is refused before anything is written",
                )
            if SIGN_IN == "oidc":
                # Read from the pod, as the dashboard will: the discovery
                # document names its issuer, and one character off is another.
                wrong = {**answers, "oidc_issuer": IDP_ISSUER + "/"}
                status, page = post("/first-run/check", wrong, cookies)
                import re

                # The findings, not the page: the form itself has an issuer field.
                findings = re.findall(r"<li>([^<]*)</li>", page)
                s.note(f"findings: {findings}")
                s.check(
                    passes not in page and any("issuer" in f for f in findings),
                    "an issuer the provider does not call itself is refused before anything is written",
                )
            status, page = post("/first-run/check", answers, cookies)
            s.check(passes in page, "the answers pass")
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
        # Out of first run, which looks different by the way in: a form for
        # a password, or a redirect to the provider.
        status, away, page = fetch("GET", f"{WIZARD}/sign-in", {})
        if WAY_IN["by"] == "password":
            return 'name="password"' in page
        return status in (302, 303) and (away or "").startswith(IDP_ISSUER)

    try:
        wait_for("the dashboard, out of first run", signing_in, seconds=300)
        def signed_in(name, password):
            if WAY_IN["by"] == "redirect":
                return sign_in_at_provider(name)
            session = {}
            status, _ = post("/sign-in", {"name": name, "password": password}, session)
            return status, session

        name, password = WAY_IN["administrator"]
        who = name or "the provider's person"
        if WAY_IN["by"] == "password":
            status, _ = post("/sign-in", {"name": name, "password": "not-the-password"}, {})
            s.check(status == 401, f"a wrong password for {name} is refused: {status}")
        status, session = signed_in(name, password)
        s.check(status == 303 and bool(session), f"{who} signs in and holds a session: {status}")
        s.check("You are a deployment admin" in get("/", session), f"and home says {who} administers it")
        if WAY_IN["somebody_else"]:
            # In the directory, so in; not in the group, so not an
            # administrator. Without this, a permission granted to everybody
            # who signs in passes every check above.
            name, password = WAY_IN["somebody_else"]
            status, session = signed_in(name, password)
            s.check(status == 303 and bool(session), f"{name} signs in too: {status}")
            s.check(
                "You are a deployment admin" not in get("/", session),
                f"and is not an administrator, being outside the group the wizard named",
            )
    finally:
        if forward is not None:
            forward.terminate()
            forward.wait()

    if WAY_IN["by"] == "password":
        print("I: and in a real browser", flush=True)
        # Everything above is an HTTP client copying a cookie by hand, which
        # proves the server's half. This is Chromium keeping and sending the
        # cookie itself, honouring HttpOnly and SameSite, and carrying the
        # sign-out form's token. In the cluster, as a pod, so it reaches the
        # dashboard by the one name every cluster resolves alike. Not on the
        # provider's branch: the provider sends the browser back to the
        # dashboard's address as the runner forwards it, which no pod can reach.
        name, password = WAY_IN["administrator"]
        kubectl("delete", "pod", "e2e-browser", "--ignore-not-found", "--wait")
        kubectl(
            "run", "e2e-browser", f"--image={BROWSER_IMAGE}",
            "--image-pull-policy=Never", "--restart=Never",
            f"--env=E2E_DASHBOARD=http://{RELEASE}-meridian-runtime-dashboard",
            f"--env=E2E_NAME={name}", f"--env=E2E_PASSWORD={password}",
        )
        phase = ""
        for _ in range(300):
            phase = kubectl("get", "pod", "e2e-browser", "-o", "jsonpath={.status.phase}")
            if phase in ("Succeeded", "Failed"):
                break
            time.sleep(1)
        for line in kubectl("logs", "e2e-browser").splitlines():
            print(f"    {line}" if line else "", flush=True)
        s.check(phase == "Succeeded", f"the browser's checks held: {phase or 'it never ran'}")

    print(flush=True)
    if s.failures:
        print(f"e2e-cluster FAILED: {len(s.failures)}", flush=True)
        for failure in s.failures:
            print(f"  - {failure}", flush=True)
        return 1
    print("e2e-cluster OK", flush=True)
    return 0


sys.exit(main())
