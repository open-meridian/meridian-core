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

# What home's header shows a deployment admin and nobody else: the gear to
# Settings, named for a screen reader.
ADMIN_HOME = 'href="/admin" aria-label="Settings"'
# What home said to a deployment admin in charts published before this one,
# which the upgrade test's published chart still says until it is upgraded:
# the words before the one header, the header's link before one button, and
# that button before it was a gear.
ADMIN_HOME_BEFORE = (
    "You are a deployment admin",
    'href="/admin">Admin portal<',
    'href="/admin">Admin<',
)


def administers(page, installed_chart_is_published=False):
    """Whether home says the person signed in administers the deployment."""
    return ADMIN_HOME in page or (
        installed_chart_is_published and any(said in page for said in ADMIN_HOME_BEFORE)
    )

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
            "admin_password": "Password1!-e2e",
            "admin_group": "",
        },
        # What G finds in the user group, and who H signs in as.
        "granted": "local|ada",
        "administrator": ("ada", "Password1!-e2e"),
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


# Who installs the chart and answers the wizard: this runner (`runner`), or
# `meridian up --params`, as a person does (`cli`), from E2E_CLI_IMAGE --
# meridian-cli's `e2e` image, the binary with the helm and kubectl it drives.
DRIVER = os.environ.get("E2E_DRIVER", "runner")
CLI_IMAGE = os.environ.get("E2E_CLI_IMAGE", "meridian-cli-e2e:local")

# An upgrade in place (task kernel/upgrading-a-deployment-in-place): install
# the chart published last from E2E_UPGRADE_FROM, set it up, then upgrade to
# this checkout's chart and image as an administrator does. Empty is an
# install of this checkout's chart, as every other run is. The version is
# the latest published unless E2E_UPGRADE_VERSION names one.
UPGRADE_FROM = os.environ.get("E2E_UPGRADE_FROM", "")
UPGRADE_VERSION = os.environ.get("E2E_UPGRADE_VERSION", "")
# The helm this installs and upgrades with. CI gives the upgrade Helm 4, whose
# --wait waits for every resource a release names to exist, which Helm 3's
# never did: it is what found a RoleBinding the chart's own Job deletes.
HELM = os.environ.get("E2E_HELM", "helm")

# The registry's node proxy port, as the chart's registry.hostPort.
REGISTRY_PORT = int(os.environ.get("E2E_REGISTRY_PORT", "5000"))

# Headless Chromium, built beside the runtime image (e2e/cluster/browser.py).
BROWSER_IMAGE = os.environ.get("E2E_BROWSER_IMAGE", "meridian-e2e-browser:local")
# Reached through the chart's Ingress and the cluster's own controller -- on
# Rancher Desktop, Traefik on this machine's port 80 -- by a name under
# `.localhost`, which this machine resolves to itself, as every browser does
# (spec/live-plugin-development, ruling 1). The dashboard is told the same
# name as its address, so each plugin's page is `<instance>.plugins.<HOST>`,
# through the same controller. No port-forward anywhere.
HOST = os.environ.get("E2E_HOST", f"{NAMESPACE}.localhost")
INGRESS_CLASS = os.environ.get("E2E_INGRESS_CLASS", "traefik")
# The controller, from inside the cluster: a pod's browser reaches the
# deployment through it by name, as this machine does.
INGRESS_UPSTREAM = os.environ.get(
    "E2E_INGRESS_UPSTREAM", "traefik.kube-system.svc.cluster.local:80"
)
DASHBOARD_URL = f"http://{HOST}"
WIZARD = DASHBOARD_URL
# What an ingress controller answers when the dashboard behind it is not
# there, and how long to keep asking: the dashboard restarts twice after
# apply, once for the Secrets and once more with the conductor.
GATEWAY = (502, 503, 504)
GATEWAY_RETRIES = 60
# A pod has no resolver for `.localhost`, so it is told: the deployment's name
# is its own loopback, where its forwarder takes the controller's place.
HOST_ALIASES = f"""
  hostAliases:
    - ip: "127.0.0.1"
      hostnames: ["{HOST}"]"""
# The plugins section P launches, by the names their pages are found under:
# the reference plugin as `meridian plugin new` makes it, declaring nothing,
# and a copy of it declaring `custody`, which is what recording a statement
# needs, so that acting-for is what decides.
PLUGIN_INSTANCE = "reference-plugin"
CUSTODY_INSTANCE = "reference-custody"


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
    # Through the Ingress, a gateway's 502, 503 or 504 is the controller
    # saying the dashboard was not there -- it restarts twice after apply --
    # never the dashboard's answer, so it is asked again. Anything the
    # dashboard itself says is returned as it is.
    for _ in range(GATEWAY_RETRIES):
        try:
            answered = opener.open(request)
            status, body, headers = answered.status, answered.read().decode(), answered.headers
        except urllib.error.HTTPError as refused:
            status, body, headers = refused.code, refused.read().decode(), refused.headers
        if status not in GATEWAY:
            break
        time.sleep(2)
    for value in headers.get_all("Set-Cookie") or []:
        name, _, rest = value.partition("=")
        cookies[name] = rest.split(";")[0]
    return status, body


def get(path, cookies):
    request = urllib.request.Request(f"{WIZARD}{path}")
    request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in cookies.items()))
    for _ in range(GATEWAY_RETRIES):
        try:
            return urllib.request.urlopen(request).read().decode()
        except urllib.error.HTTPError as refused:
            if refused.code not in GATEWAY:
                return refused.read().decode()
        time.sleep(2)
    return ""


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

    # A gateway's 502 to 504 is the dashboard not being there yet, as in post.
    for _ in range(GATEWAY_RETRIES):
        try:
            answered = urllib.request.build_opener(NoRedirects).open(request)
        except urllib.error.HTTPError as refused:
            answered = refused
        if getattr(answered, "status", None) not in GATEWAY and getattr(answered, "code", None) not in GATEWAY:
            break
        time.sleep(2)
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
        "dashboard_url": DASHBOARD_URL,
    }


def by_cli(s, deployment_id, enrolment_code):
    """Install and answer the wizard with `meridian up --params`, as a person does.

    Result 1 of plans/a-person-reaches-a-plugin, and spec/the-cli's `e2e-up`:
    the chart installed and the wizard answered by the CLI rather than by this
    runner's own HTTP calls. It runs in a container holding the binary and the
    helm and kubectl it drives, on this machine's network, so its kubeconfig
    is this machine's, and it finds the cluster's ingress controller and
    installs through it by `--host`, as a person's `meridian up` does.

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
        # This machine's `.localhost` is the VM's here, where the controller
        # listens too; the container has no resolver for it.
        "--add-host", f"{HOST}:127.0.0.1",
        "-v", f"{kubeconfig}:/kube/config:ro", "-e", "KUBECONFIG=/kube/config",
        "-v", f"{os.path.abspath(CHART)}:/chart:ro",
        "-v", f"{params.name}:/params.yaml:ro",
        *[flag for name in environment for flag in ("-e", name)],
        CLI_IMAGE, "up",
        "--namespace", NAMESPACE, "--release", RELEASE, "--chart", "/chart",
        "--id", deployment_id, "--platform", PLATFORM_FROM_POD,
        *(["--image", IMAGE] if IMAGE else []),
        "--params", "/params.yaml", "--host", HOST, "--development", "--no-doctor",
        # Plain HTTP and no certificate, as the runner's own install: for testing.
        "--plain-http",
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


def apply(manifest):
    """A manifest applied in this run's namespace."""
    done = subprocess.run(
        ["kubectl", "--namespace", NAMESPACE, "apply", "-f", "-"],
        input=manifest, capture_output=True, text=True,
    )
    if done.returncode != 0:
        raise SystemExit(f"kubectl apply failed\n{done.stdout}\n{done.stderr}")


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
        # A new namespace's default service account is made by a controller
        # a moment later, and a pod asking for it before then is refused: the
        # install's hooks would wait on pods that were never made.
        wait_for(
            f"the default service account in {NAMESPACE}",
            lambda: subprocess.run(
                ["kubectl", "--namespace", NAMESPACE, "get", "serviceaccount", "default"],
                capture_output=True,
            ).returncode == 0,
            seconds=120,
        )
        s.note(f"{NAMESPACE} has its default service account")
        values = [
            "--set", f"deployment.id={deployment_id}",
            "--set", f"deployment.enrolmentCode={enrolment_code}",
            "--set", f"platform.address={PLATFORM_FROM_POD}",
        ]
        chart = [CHART]
        if UPGRADE_FROM:
            # The published chart, with the image it was published beside:
            # what a deployment made before this checkout is running.
            version = UPGRADE_VERSION or next(
                line.split(":", 1)[1].strip().strip('"')
                for line in run(HELM, "show", "chart", UPGRADE_FROM).splitlines()
                if line.startswith("version:")
            )
            s.note(f"from {UPGRADE_FROM} {version}")
            chart = [UPGRADE_FROM, "--version", version]
        elif IMAGE:
            repository, _, tag = IMAGE.rpartition(":")
            values += ["--set", f"image.repository={repository}", "--set", f"image.tag={tag}"]
        values += [
            # For development: the live shape is part of what this run proves.
            "--set", "development=true",
            "--set", "ingress.enabled=true",
            "--set", f"ingress.host={HOST}",
            "--set", f"ingress.className={INGRESS_CLASS}",
            # Plain HTTP, which the chart serves only when told and only for
            # testing: this run reaches Traefik on :80 (task kernel/a-
            # development-deployment-serves-https, ruling 3).
            "--set", "ingress.plainHttp=true",
        ]
        run(HELM, "upgrade", "--install", RELEASE, *chart, "--namespace", NAMESPACE, *values)

        wait_for(
            "the dashboard",
            lambda: "1/1" in kubectl("get", "pods", "-l", "meridian.dev/component=dashboard", "--no-headers"),
        )
        s.check(True, "the dashboard is up")

        # Through the Ingress: the controller takes a moment to read it.
        wait_for(
            "the dashboard through the Ingress",
            lambda: urllib.request.urlopen(f"{WIZARD}/healthz").status == 200,
        )
        s.check(True, f"the dashboard answers at {WIZARD}, through the cluster's ingress controller")

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
        if passes not in page:
            import re

            # What the check found, or the run says only that it failed.
            s.note(f"findings: {re.findall(r'<li>([^<]*)</li>', page)} ({status})")
        s.check(passes in page, "the answers pass")
        status, page = post("/first-run/apply", answers, cookies)
        if "administers this deployment" not in page:
            import re as _re

            s.note(f"page: {_re.sub(r'<[^>]*>', ' ', page)[:400]}")
        s.check("administers this deployment" in page, "applied, and it names who administers it")

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
    # Through the Ingress, which reaches whichever dashboard pod is running:
    # a port-forward to one that apply restarted used to exit, and waiting on
    # it was 300 seconds of asking a closed port.

    def signing_in():
        # Out of first run, which looks different by the way in: a form for
        # a password, or a redirect to the provider.
        status, away, page = fetch("GET", f"{WIZARD}/sign-in", {})
        if WAY_IN["by"] == "password":
            return 'name="password"' in page
        return status in (302, 303) and (away or "").startswith(IDP_ISSUER)

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
    home = get("/", session)
    published = bool(UPGRADE_FROM)
    if not administers(home, published):
        import re

        s.note(f"home: {re.sub(r'<[^>]*>', ' ', home.split('</style>')[-1])[:300]}")
    s.check(administers(home, published), f"and home says {who} administers it")
    if WAY_IN["somebody_else"]:
        # In the directory, so in; not in the group, so not an
        # administrator. Without this, a permission granted to everybody
        # who signs in passes every check above.
        name, password = WAY_IN["somebody_else"]
        status, session = signed_in(name, password)
        s.check(status == 303 and bool(session), f"{name} signs in too: {status}")
        s.check(
            not administers(get("/", session), published),
            f"and is not an administrator, being outside the group the wizard named",
        )

    if UPGRADE_FROM:
        upgraded(s)
        # And the deployment it upgraded is the same one: the administrator
        # the wizard named signs in to the new version.
        status, session = signed_in(*WAY_IN["administrator"])
        s.check(
            status == 303 and ADMIN_HOME in get("/", session),
            f"after the upgrade {who} signs in, and is still a deployment admin: {status}",
        )
        return verdict(s)

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

    print("T: a terminal connects, as the person running it", flush=True)
    # W6.13 and W6.14. The loopback redirect needs the terminal and the
    # browser on one machine, and a pod is one: its containers share
    # 127.0.0.1. So they run together, with a forwarder to the dashboard on
    # the port the dashboard thinks is its address -- which is also what
    # lets a provider's redirect back land in here, on every branch. The
    # terminal is the real CLI where this run has its image (E2E_DRIVER=cli,
    # meridian-cli's `make e2e-up`) and a stand-in for it where it does not.
    name, password = WAY_IN["administrator"]
    real_cli = DRIVER == "cli"
    kubectl("delete", "pod", "e2e-terminal", "--ignore-not-found", "--wait")
    cli_container = f"""
    - name: cli
      image: {CLI_IMAGE}
      imagePullPolicy: Never
      env: [{{name: XDG_CONFIG_HOME, value: /shared/config}}]
      volumeMounts: [{{name: shared, mountPath: /shared}}]
      command:
        - sh
        - -c
        - |
          meridian connect {DASHBOARD_URL} > /shared/connect-1.out 2>&1; echo "exit=$?" >> /shared/connect-1.out
          until [ -f /shared/sign-out-now ]; do sleep 1; done
          meridian sign-out > /shared/sign-out.out 2>&1; echo "exit=$?" >> /shared/sign-out.out
          until [ -f /shared/connect-again ]; do sleep 1; done
          meridian connect {DASHBOARD_URL} > /shared/connect-2.out 2>&1; echo "exit=$?" >> /shared/connect-2.out
""" if real_cli else ""
    apply(f"""
apiVersion: v1
kind: Pod
metadata: {{name: e2e-terminal}}
spec:
  restartPolicy: Never{HOST_ALIASES}
  volumes: [{{name: shared, emptyDir: {{}}}}]
  containers:
    - name: browser
      image: {BROWSER_IMAGE}
      imagePullPolicy: Never
      command: [python, /e2e/terminal.py]
      volumeMounts: [{{name: shared, mountPath: /shared}}]
      env:
        - {{name: E2E_DASHBOARD, value: "http://{INGRESS_UPSTREAM}"}}
        - {{name: E2E_HOST, value: "{HOST}"}}
        - {{name: E2E_BY, value: "{WAY_IN['by']}"}}
        - {{name: E2E_NAME, value: "{name}"}}
        - {{name: E2E_PASSWORD, value: "{password or ''}"}}
        - {{name: E2E_TERMINAL, value: "{'cli' if real_cli else 'python'}"}}
{cli_container}""")
    phase = ""
    for _ in range(420):
        phase = kubectl("get", "pod", "e2e-terminal", "-o", "jsonpath={.status.phase}")
        if phase in ("Succeeded", "Failed"):
            break
        time.sleep(1)
    for line in kubectl("logs", "e2e-terminal", "-c", "browser").splitlines():
        print(f"    {line}" if line else "", flush=True)
    s.check(
        phase == "Succeeded",
        f"a terminal connected, signed out and was ended, by {'the real CLI' if real_cli else 'a stand-in'}: "
        f"{phase or 'it never ran'}",
    )

    print("R: the deployment's own registry", flush=True)
    # spec/the-local-plugin-registry: an image put in the registry from inside
    # the cluster is pulled by the node from its own localhost, through the
    # node's proxy, with nothing configured on the node. The Job stands in for
    # the dashboard's upload, which is what will put plugins there; it carries
    # the release's label because the registry admits this release's pods and
    # nothing else.
    registry = f"{RELEASE}-meridian-runtime-registry"
    wait_for(
        "the registry",
        lambda: "1/1" in kubectl("get", "statefulset", registry, "--no-headers"),
        seconds=300,
    )
    apply(f"""
apiVersion: batch/v1
kind: Job
metadata: {{name: e2e-upload}}
spec:
  backoffLimit: 4
  template:
    metadata: {{labels: {{app.kubernetes.io/instance: {RELEASE}}}}}
    spec:
      restartPolicy: Never
      # Until the registry answers. A pod's first seconds are refused by the
      # registry's NetworkPolicy while the cluster's policy engine learns the
      # new address, so a pod that copies the moment it starts is refused
      # every time -- and each retry of the Job is a new pod with a new one.
      initContainers:
        - name: until-reachable
          image: curlimages/curl:8.10.1
          command: [sh, -c, "for i in $(seq 1 60); do curl -sf -o /dev/null http://{registry}:5000/v2/ && exit 0; sleep 1; done; exit 1"]
      containers:
        - name: copy
          image: gcr.io/go-containerregistry/crane:v0.22.1
          args: [copy, --insecure, "busybox:1.36", "{registry}:5000/e2e/busybox:1"]
""")
    wait_for(
        "the upload",
        lambda: kubectl("get", "job", "e2e-upload", "-o", "jsonpath={.status.succeeded}") == "1",
        seconds=300,
    )
    apply(f"""
apiVersion: v1
kind: Pod
metadata: {{name: e2e-pulled}}
spec:
  restartPolicy: Never
  containers:
    - name: pulled
      image: localhost:{REGISTRY_PORT}/e2e/busybox:1
      imagePullPolicy: Always
      command: [echo, pulled]
""")
    wait_for(
        "the pull",
        lambda: kubectl("get", "pod", "e2e-pulled", "-o", "jsonpath={.status.phase}")
        in ("Succeeded", "Failed"),
        seconds=180,
    )
    s.check(
        kubectl("get", "pod", "e2e-pulled", "-o", "jsonpath={.status.phase}") == "Succeeded",
        f"the node pulled localhost:{REGISTRY_PORT}/e2e/busybox:1 from the registry",
    )

    print("L: the launcher, and the broker learning what it launches", flush=True)
    # decisions/019 and the registry spec's decision 3. The launcher serving
    # means it read its template, reached the cluster's API as its own
    # account, and joined the bus with its own credential; the broker's own
    # process watching means it read the Deployments and its Secret. Launching
    # through them is section P, once uploads arrive (W8.1).
    launcher = f"deployment/{RELEASE}-meridian-runtime-launcher"
    wait_for(
        "the launcher serving",
        lambda: "the launcher is serving" in kubectl("logs", launcher),
        seconds=300,
    )
    s.check(True, "the launcher reached the cluster's API and the bus as itself")
    broker_log = kubectl("logs", f"deployment/{RELEASE}-meridian-runtime-broker")
    s.check(
        "the broker is running" in broker_log
        and "launched plugins could not be read" not in broker_log,
        "the broker runs as its own process's child, and reads the Deployments and its Secret",
    )

    edge_storage(s)

    print("P: a person makes a plugin, launches it, and opens its page", flush=True)
    # Results 2 and 3 of plans/a-person-reaches-a-plugin, by the real CLI:
    # `meridian plugin new`, `connect`, `upload`, `launch` and `list`, then
    # the page in a real browser (e2e/cluster/plugin.py). The CLI builds with
    # docker as it does on a person's machine, and is given the node's own
    # daemon to do it with -- which exists on Rancher Desktop and Docker
    # Desktop, the local clusters the registry is for, and not on k3d or kind.
    if DRIVER != "cli":
        s.note("skipped: it is the real CLI's, which meridian-cli's `make e2e-up` runs")
    else:
        name, password = WAY_IN["administrator"]
        instance = PLUGIN_INSTANCE
        kubectl("delete", "pod", "e2e-plugin", "--ignore-not-found", "--wait")
        step = lambda said, command: (
            f"          {command} > /shared/{said}.out 2>&1; echo \"exit=$?\" >> /shared/{said}.out"
        )
        script = "\n".join([
            step("new", "meridian plugin new reference-plugin --into /shared/reference-plugin"),
            step("connect", f"meridian connect {DASHBOARD_URL}"),
            step("upload", "meridian plugin upload --dir /shared/reference-plugin"),
            step("launch", f"meridian plugin launch reference-plugin 0.1.0 --instance {instance} --yes"),
            # The copy: made the same way, with `custody` declared in its own
            # pyproject.toml, as an author declares a role.
            step("new-custody", "meridian plugin new reference-custody --into /shared/reference-custody"),
            step("declare-custody",
                 "sed -i 's/^roles = \\[\\]$/roles = [\"custody\"]/' /shared/reference-custody/pyproject.toml"
                 " && grep '^roles' /shared/reference-custody/pyproject.toml"),
            step("upload-custody", "meridian plugin upload --dir /shared/reference-custody"),
            step("launch-custody",
                 f"meridian plugin launch reference-custody 0.1.0 --instance {CUSTODY_INSTANCE} --yes"),
            step("list", "meridian plugin list"),
            # Until the browser has withdrawn her permission, whatever it asks
            # the CLI to run: the live loop's commands, each written as
            # /shared/ask/<n>.sh and answered in <n>.out with its exit.
            "          mkdir -p /shared/ask",
            "          until [ -f /shared/list-again ]; do",
            "            for asked in /shared/ask/*.sh; do",
            "              [ -e \"$asked\" ] || continue",
            "              sh \"$asked\" > \"${asked%.sh}.tmp\" 2>&1; echo \"exit=$?\" >> \"${asked%.sh}.tmp\"",
            "              mv \"${asked%.sh}.tmp\" \"${asked%.sh}.out\"; rm \"$asked\"",
            "            done",
            "            sleep 0.2",
            "          done",
            # Then: the same session.
            step("list-again", "meridian plugin list"),
        ])
        apply(f"""
apiVersion: v1
kind: Pod
metadata: {{name: e2e-plugin}}
spec:
  restartPolicy: Never{HOST_ALIASES}
  volumes:
    - {{name: shared, emptyDir: {{}}}}
    - {{name: docker, hostPath: {{path: /var/run/docker.sock, type: Socket}}}}
  containers:
    - name: browser
      image: {BROWSER_IMAGE}
      imagePullPolicy: Never
      command: [python, /e2e/plugin.py]
      volumeMounts: [{{name: shared, mountPath: /shared}}]
      env:
        - {{name: E2E_DASHBOARD, value: "http://{INGRESS_UPSTREAM}"}}
        - {{name: E2E_HOST, value: "{HOST}"}}
        - {{name: E2E_BY, value: "{WAY_IN['by']}"}}
        - {{name: E2E_NAME, value: "{name}"}}
        - {{name: E2E_PASSWORD, value: "{password or ''}"}}
        - {{name: E2E_INSTANCE, value: "{instance}"}}
        - {{name: E2E_CUSTODY_INSTANCE, value: "{CUSTODY_INSTANCE}"}}
    - name: cli
      image: {CLI_IMAGE}
      imagePullPolicy: Never
      env: [{{name: XDG_CONFIG_HOME, value: /shared/config}}]
      volumeMounts:
        - {{name: shared, mountPath: /shared}}
        - {{name: docker, mountPath: /var/run/docker.sock}}
      command:
        - sh
        - -c
        - |
{script}
""")
        # Up means available: its sidecar registered with the broker's new
        # credential and its plugin reported. Then the browser is told.
        deployments = [f"{RELEASE}-meridian-runtime-plugin-{each}" for each in (instance, CUSTODY_INSTANCE)]
        up = False
        for _ in range(1200):
            phase = kubectl("get", "pod", "e2e-plugin", "-o", "jsonpath={.status.phase}")
            if phase in ("Succeeded", "Failed"):
                break
            available = [
                kubectl("get", "deployment", each, "--ignore-not-found",
                        "-o", "jsonpath={.status.availableReplicas}")
                for each in deployments
            ]
            if available == ["1", "1"]:
                up = True
                break
            time.sleep(1)
        s.check(up, f"the launcher made {' and '.join(deployments)}, and they came up")
        if up:
            # decisions/028: the custody copy holds an edge role, so the
            # launcher made its instance's claim, labelled with its plugin,
            # and its pod mounts it; the reference plugin holds none.
            claim = f"{RELEASE}-meridian-runtime-storage-{CUSTODY_INSTANCE}"
            held = json.loads(kubectl("get", "pvc", claim, "--ignore-not-found", "-o", "json") or "{}")
            s.check(
                held.get("status", {}).get("phase") == "Bound"
                and held["metadata"]["labels"].get("meridian.dev/plugin") == "reference-custody",
                f"the custody plugin's storage, {claim}, is made, bound and its plugin's: "
                f"{held.get('status', {}).get('phase', 'absent')}",
            )
            volumes = kubectl("get", "deployment", deployments[1], "-o",
                              "jsonpath={.spec.template.spec.volumes[*].persistentVolumeClaim.claimName}")
            s.check(volumes == claim, f"and its pod mounts that claim and no other: {volumes or 'none'}")
            s.check(
                kubectl("get", "pvc", f"{RELEASE}-meridian-runtime-storage-{instance}",
                        "--ignore-not-found", "-o", "name") == "",
                "the reference plugin, holding no edge role, has no storage",
            )
        if up:
            kubectl("exec", "e2e-plugin", "-c", "browser", "--", "touch", "/shared/open-now")
        phase = ""
        for _ in range(600):
            phase = kubectl("get", "pod", "e2e-plugin", "-o", "jsonpath={.status.phase}")
            if phase in ("Succeeded", "Failed"):
                break
            time.sleep(1)
        for line in kubectl("logs", "e2e-plugin", "-c", "browser").splitlines():
            print(f"    {line}" if line else "", flush=True)
        s.check(
            phase == "Succeeded",
            f"the reference plugin and its custody copy were made, uploaded, launched and "
            f"opened, acting-for decided, and the terminal's session held to its permission: "
            f"{phase or 'it never ran'}",
        )
        # And turned off: the launcher, restarted by the upgrade, removes what
        # it made live, and the chart no longer renders the live shape for it
        # (spec/live-plugin-development, ruling 2). Last, since the upgrade
        # restarts the dashboard and every session with it.
        live_deployment = f"{RELEASE}-meridian-runtime-plugin-reference-live"
        was_live = kubectl("get", "deployment", live_deployment, "--ignore-not-found", "-o", "name")
        s.check(bool(was_live), f"{live_deployment} ran while the deployment was for development")
        run("helm", "upgrade", RELEASE, CHART, "--namespace", NAMESPACE,
            "--reuse-values", "--set", "development=false")
        gone = False
        for _ in range(300):
            if not kubectl("get", "deployment", live_deployment, "--ignore-not-found", "-o", "name"):
                gone = True
                break
            time.sleep(1)
        s.check(gone, "turned off, the live instance is removed by the restarted launcher")
        template = kubectl("get", "configmap", f"{RELEASE}-meridian-runtime-launcher-template", "-o", "json")
        s.check("plugin-live.json" not in template, "and the launcher holds no live shape to make")
        s.check(
            kubectl("get", "deployment", f"{RELEASE}-meridian-runtime-plugin-{instance}",
                    "--ignore-not-found", "-o", "name") != "",
            "while the plugins launched as versions keep running",
        )
        if phase != "Succeeded" or not up:
            # For reading, and never the reason the run stops: the plugin
            # may not exist to be read.
            for deployment, container in [(d, c) for d in deployments for c in ("sidecar", "plugin")]:
                s.note(f"{deployment}, {container}:")
                try:
                    said = kubectl("logs", f"deployment/{deployment}", "-c", container, "--tail=40")
                except SystemExit as failed:
                    said = str(failed)
                for line in said.splitlines():
                    print(f"      {line}", flush=True)

    if SIGN_IN == "local":
        # W6.16, last, because every stage above signs in with the password
        # the wizard was given. The only administrator's password is lost,
        # and a code from the platform is the way back: nothing is learned
        # from the cluster and no namespace is deleted.
        print("Z: a lost password, and a code from the platform the way back", flush=True)
        name, password = WAY_IN["administrator"]
        status, before = signed_in(name, password)
        s.check(status == 303 and bool(before), f"{name} holds a session before the reset: {status}")
        reset_code = platform(
            "issue_claim_code", "--deployment", deployment_id, "--purpose", "reset-local-admin"
        ).splitlines()[-1]
        renewed = "Renewed-e2e-password"
        status, _ = post(
            "/sign-in/reset",
            {"code": reset_code, "login": name, "password": renewed, "password_again": renewed},
            {},
        )
        s.check(status == 303, f"the code is redeemed and the password set: {status}")
        status, _ = post("/sign-in", {"name": name, "password": password}, {})
        s.check(status == 401, f"the lost password is refused now: {status}")
        status, after = signed_in(name, renewed)
        s.check(status == 303 and bool(after), f"the new one signs {name} in: {status}")
        s.check(ADMIN_HOME in get("/", after), "still a deployment admin")
        s.check(
            ADMIN_HOME not in get("/", before),
            "and the session held before the reset has ended",
        )
        status, _ = post(
            "/sign-in/reset",
            {"code": reset_code, "login": name, "password": renewed, "password_again": renewed},
            {},
        )
        s.check(status != 303, f"and the code, once spent, is refused: {status}")

    return verdict(s)


def edge_storage(s):
    """decisions/028, as the chart installed it: the launcher holds an edge
    plugin's shape and the claim it makes, and, where the cluster has
    ValidatingAdmissionPolicy, a plugin's pod mounting any storage but its
    own instance's claim, or that claim without an edge role, is refused.

    Asked of the API server by server-side dry run, so nothing is made: a
    pod carrying the plugin label, a sidecar holding the roles, and the
    volume under test. Launching an edge plugin for real is section P's.
    """
    print("S: an edge plugin's storage, and the policy on plugin pods", flush=True)
    template = json.loads(kubectl(
        "get", "configmap", f"{RELEASE}-meridian-runtime-launcher-template", "-o", "json"))["data"]
    s.check(
        {"plugin-storage.json", "claim.json"} <= set(template),
        f"the launcher holds an edge plugin's shape and its claim: {sorted(template)}",
    )
    policy = f"{RELEASE}-meridian-runtime-plugin-storage"
    if not subprocess.run(["kubectl", "get", "validatingadmissionpolicy", policy],
                          capture_output=True).returncode == 0:
        s.note("skipped the policy: this cluster has no ValidatingAdmissionPolicy (before 1.30)")
        return
    own = f"{RELEASE}-meridian-runtime-storage-e2e-edge"

    def admitted(roles, volume):
        manifest = json.dumps({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "e2e-edge", "labels": {
                "meridian.dev/component": "sidecar", "meridian.dev/instance": "e2e-edge"}},
            "spec": {
                "containers": [
                    {"name": "sidecar", "image": "busybox:1",
                     "env": [{"name": "MERIDIAN_PLUGIN_ROLES", "value": roles}]},
                    {"name": "plugin", "image": "busybox:1",
                     "volumeMounts": [{"name": "storage", "mountPath": "/var/lib/meridian/storage"}]},
                ],
                "volumes": [{"name": "storage", **volume}],
            },
        })
        done = subprocess.run(
            ["kubectl", "--namespace", NAMESPACE, "create", "--dry-run=server", "-f", "-"],
            input=manifest, capture_output=True, text=True,
        )
        return done.returncode == 0, (done.stdout + done.stderr).strip()

    claim = lambda name: {"persistentVolumeClaim": {"claimName": name}}
    ok, said = admitted("custody", claim(own))
    s.check(ok, f"a custody plugin's pod mounting its own instance's claim is admitted: {said}")
    ok, said = admitted("operations,reporting", claim(own))
    s.check(ok, f"so is one holding reporting, an outbound edge, beside operations: {said}")
    for roles, volume, why, words in [
        ("custody", claim(f"{RELEASE}-meridian-runtime-storage-another"),
         "another instance's claim", "its own instance's claim"),
        ("custody", {"hostPath": {"path": "/var/lib"}}, "a host path", "its own instance's claim"),
        ("operations", claim(own), "its own claim, holding no edge role", "edge role"),
        ("", claim(own), "its own claim, holding no role", "edge role"),
    ]:
        ok, said = admitted(roles, volume)
        s.check(not ok and words in said, f"a plugin's pod mounting {why} is refused: {said}")


def verdict(s):
    print(flush=True)
    if s.failures:
        print(f"e2e-cluster FAILED: {len(s.failures)}", flush=True)
        for failure in s.failures:
            print(f"  - {failure}", flush=True)
        return 1
    print("e2e-cluster OK", flush=True)
    return 0


def upgraded(s):
    """This checkout's chart and image over the published one, set up and serving.

    The upgrade an administrator makes: the new chart's defaults with the
    deployment's own values, `--reset-then-reuse-values`, which `--reuse-values`
    is not (it kept the old chart's image tag), and `--wait`, which failed on
    the chart's own RoleBinding. Then what the task's done-when asks of the
    namespace it leaves: every pod on the new image, none restarted, no more
    than three old ReplicaSets a Deployment and no finished Job from an
    earlier revision (task kernel/upgrading-a-deployment-in-place).
    """
    print("U: upgraded in place, to the chart and the image this checkout builds", flush=True)
    repository, _, tag = IMAGE.rpartition(":")
    started = time.time()
    done = subprocess.run(
        [HELM, "upgrade", RELEASE, CHART, "--namespace", NAMESPACE,
         "--reset-then-reuse-values", "--wait", "--timeout", "10m",
         "--set", f"image.repository={repository}", "--set", f"image.tag={tag}"],
        capture_output=True, text=True,
    )
    for line in (done.stdout + done.stderr).splitlines():
        print(f"    | {line}", flush=True)
    s.check(
        done.returncode == 0,
        f"helm upgrade --reset-then-reuse-values --wait succeeded: exit {done.returncode}, "
        f"{int(time.time() - started)}s",
    )
    revision = int(json.loads(run(HELM, "status", RELEASE, "--namespace", NAMESPACE, "-o", "json"))["version"])

    def pods():
        return json.loads(kubectl("get", "pods", "-o", "json"))["items"]

    def ours(container):
        return "meridian-runtime" in container["image"]

    def containers(pod):
        return pod["spec"].get("initContainers", []) + pod["spec"]["containers"]

    def jobs():
        return json.loads(kubectl("get", "jobs", "-o", "json"))["items"]

    # The one thing the chart installed first leaves that no upgrade can
    # take away: its pre-install key Job, which that chart never deleted and
    # this one deletes on success. A pre-install hook does not run on an
    # upgrade, so it stays, finished, with its pod on the old image. That Job
    # and nothing else: any other Job or pod left on the old image fails.
    old_key_job = f"{RELEASE}-meridian-runtime-key"

    def left_by_the_old_chart(job):
        annotations = job["metadata"].get("annotations", {})
        return (
            job["metadata"]["name"] == old_key_job
            and "pre-install" in annotations.get("helm.sh/hook", "")
            and "hook-succeeded" not in annotations.get("helm.sh/hook-delete-policy", "")
            and bool(job["status"].get("succeeded"))
        )

    # A pod the upgrade replaced may still be stopping after --wait returns.
    def stale():
        kept = {job["metadata"]["uid"] for job in jobs() if left_by_the_old_chart(job)}
        left, exempt = [], []
        for pod in pods():
            if not any(ours(c) and c["image"] != IMAGE for c in containers(pod)):
                continue
            owners = {
                owner["uid"]
                for owner in pod["metadata"].get("ownerReferences", [])
                if owner.get("kind") == "Job"
            }
            (exempt if owners & kept else left).append(pod["metadata"]["name"])
        return sorted(left), sorted(exempt)
    for _ in range(180):
        if not stale()[0]:
            break
        time.sleep(1)
    left, exempt = stale()
    if exempt:
        s.note(f"on the old image, the pod of {old_key_job}, which the chart installed first kept: {exempt}")
    s.check(not left, f"every pod runs {IMAGE}" + (f"; not: {left}" if left else ""))

    restarted = sorted(
        f"{pod['metadata']['name']}/{status['name']} ({status['restartCount']})"
        for pod in pods()
        if any(ours(container) for container in containers(pod))
        for status in pod["status"].get("initContainerStatuses", [])
        + pod["status"].get("containerStatuses", [])
        if status.get("restartCount", 0)
    )
    s.check(
        not restarted,
        "no container restarted: each waited for its migration and the broker instead"
        + (f"; restarted: {restarted}" if restarted else ""),
    )

    migrated = f"{RELEASE}-meridian-runtime-migrate-{revision}"
    wait_for(
        f"{migrated} to finish",
        lambda: kubectl("get", "job", migrated, "-o", "jsonpath={.status.succeeded}") == "1",
        seconds=300,
    )
    s.check(True, f"{migrated} migrated the schema")

    deployments = json.loads(kubectl("get", "deployments", "-o", "json"))["items"]
    limits = {d["metadata"]["name"]: d["spec"].get("revisionHistoryLimit") for d in deployments}
    s.check(
        all(limit == 3 for limit in limits.values()),
        f"every Deployment keeps three old ReplicaSets: {limits}",
    )

    # Old ReplicaSets are trimmed by the Deployment controller after the
    # rollout, so asked until they are, for a while.
    def old_replica_sets():
        counted = {name: 0 for name in limits}
        for replica_set in json.loads(kubectl("get", "replicasets", "-o", "json"))["items"]:
            owner = (replica_set["metadata"].get("ownerReferences") or [{}])[0].get("name")
            if owner in counted and not replica_set["spec"].get("replicas"):
                counted[owner] += 1
        return counted
    for _ in range(60):
        if all(count <= 3 for count in old_replica_sets().values()):
            break
        time.sleep(1)
    counted = old_replica_sets()
    s.check(
        all(count <= 3 for count in counted.values()),
        f"at most three old ReplicaSets a Deployment: {counted}",
    )

    # Named by revision and removed by Helm with the next one. A hook Job
    # carries no revision: it goes when it succeeds. The old chart's key Job,
    # above, is the one it kept, and the only one excused.
    def earlier_jobs():
        earlier, kept = [], []
        for job in jobs():
            name = job["metadata"]["name"]
            annotations = job["metadata"].get("annotations", {})
            finished = job["status"].get("succeeded") or job["status"].get("failed")
            suffix = name.rsplit("-", 1)[-1]
            if left_by_the_old_chart(job):
                kept.append(name)
            elif "helm.sh/hook" in annotations:
                if finished:
                    earlier.append(name)
            elif not (suffix.isdigit() and int(suffix) == revision):
                earlier.append(name)
        return earlier, kept
    for _ in range(120):
        if not earlier_jobs()[0]:
            break
        time.sleep(1)
    earlier, kept = earlier_jobs()
    if kept:
        s.note(f"left by the chart installed first, which kept its hook Job once done: {kept}")
    s.check(
        not earlier,
        f"no finished Job from before revision {revision}" + (f"; left: {earlier}" if earlier else ""),
    )


sys.exit(main())
