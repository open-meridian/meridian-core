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
    """A query, or "" when it cannot be answered yet.

    Waiting for a schema means asking for a table that does not exist, and a
    helper that raised there turned "not yet" into "stop" -- which is how the
    first run of this ended, on a relation the conductor had not made.
    """
    done = subprocess.run(
        ["kubectl", "--namespace", NAMESPACE, "exec",
         f"{RELEASE}-meridian-runtime-database-0", "--",
         "psql", "-U", "postgres", "-d", database, "-Atc", sql],
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
        # The wizard answers `bundled` below, and the Secrets it fills for
        # Zitadel are rendered only when the chart brought one. Choosing a
        # backend the chart did not render fails at the step that writes,
        # which is late.
        "--set", "identity.bundled.enabled=true",
        "--set", "zitadel.image.tag=v4.17.3",
        "--set", "zitadel.login.image.tag=v4.17.3",
        # Where Zitadel may reach: the cluster's pod and service ranges, which
        # is where its database and the group hook are. The chart refuses to
        # render without this rather than installing something that reaches
        # nothing.
        "--set", 'identity.bundled.egress.allowCidrs={10.42.0.0/16,10.43.0.0/16}',
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

        print("E: applied, on a database it brings", flush=True)
        answers = {
            "db_route": "brought",
            "db_name": "meridian",
            "db_serving_role": "meridian_app",
            "db_migrating_role": "meridian_migrate",
            "backend": "bundled",
            "zitadel_version": "v4.17.3",
            "zitadel_egress": "10.42.0.0/16,10.43.0.0/16",
            "directory": "local",
            "admin_login": "ada",
            "admin_email": "ada@example.org",
            "admin_given_name": "Ada",
            "admin_password": "Password1!",
            "admin_group": "",
            "dashboard_url": WIZARD,
            "zitadel_url": f"http://{RELEASE}-meridian-runtime-zitadel.{NAMESPACE}.svc.cluster.local:8080",
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

    print("F: the database it brought", flush=True)
    wait_for(
        "the database",
        lambda: "1/1" in kubectl("get", "pods", "-l", "meridian.dev/component=database", "--no-headers"),
        seconds=300,
    )
    s.check(True, "a Postgres that did not exist before the wizard is running")
    roles = psql("select rolname from pg_roles where rolname like 'meridian\\_%'")
    s.note(f"roles: {roles.split()}")
    s.check("meridian_app" in roles and "meridian_migrate" in roles, "with both roles made")
    s.check(
        psql("select has_schema_privilege('meridian_app','public','CREATE')") == "f",
        "and the serving role may not create tables",
    )
    s.check(
        psql("select datname from pg_database where datname = 'zitadel'") == "zitadel",
        "and Zitadel has its own database on the same server",
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
    s.check("ada" in groups, "the account the wizard created holds deployment admin")

    print(flush=True)
    if s.failures:
        print(f"e2e-cluster FAILED: {len(s.failures)}", flush=True)
        for failure in s.failures:
            print(f"  - {failure}", flush=True)
        return 1
    print("e2e-cluster OK", flush=True)
    return 0


sys.exit(main())
