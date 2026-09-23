"""First run, end to end: an install that was given nothing, made to serve.

What this proves, in the order the spec asks for it:

- the wizard answers one page until a first-run claim code is redeemed, and a
  wrong code is refused with the platform's own reason;
- a wrong answer is refused with a finding, and nothing is written;
- applying writes the named Secrets, patches the one policy, restarts the
  named Deployments and deletes the Job's own binding;
- the credentials reach the first-run Job sealed: what the wizard sent is not
  in the clear anywhere the broker could see it, and what the Job wrote is;
- the same answers through the endpoints a CLI would call leave the same
  configuration as the browser's route.

Run by `make e2e-first-run`.
"""
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

DASHBOARD = os.environ["DASHBOARD"]
PLATFORM = os.environ["PLATFORM"]
KUBE = os.environ["KUBE"]
FIRST_RUN_CODE = os.environ["E2E_FIRST_RUN_CODE"]
FIRST_ADMIN_CODE = os.environ["E2E_FIRST_ADMIN_CODE"]
DATABASE_PASSWORD = os.environ["E2E_DATABASE_PASSWORD"]

# Which of the wizard's two database routes this run takes. `external` points
# at a database somebody already runs, which is every deployment a firm
# depends on; `brought` starts one in the cluster, which is what somebody
# trying the product does and what needs nothing of them.
ROUTE = os.environ.get("E2E_DB_ROUTE", "external")
# Which directory signs people in. `bundled` is the Zitadel this chart runs;
# `oidc` is the firm's own provider, and the deployment is still installed with
# the bundle rendered -- which is the combination that keeps the choice open
# until the wizard, and the combination three defects hid in until 2026-09-23:
# the issuer was written where the chart does not read it, the groups claim was
# collected and dropped, and nothing here walked the route to notice either.
BACKEND = os.environ.get("E2E_BACKEND", "bundled")

# On the brought route the names are ones the harness has not prepared, so
# that finding them afterwards means the Job made them rather than that they
# were already there.
BROUGHT_NAMES = {
    "db_name": "brought",
    "db_serving_role": "brought_app",
    "db_migrating_role": "brought_migrate",
}

ANSWERS = {
    "db_route": ROUTE,
    "db_host": "core-postgres", "db_port": "5432", "db_name": "firstrun",
    "db_sslmode": "disable",
    "db_serving_role": "firstrun_app", "db_serving_password": DATABASE_PASSWORD,
    "db_migrating_role": "firstrun_migrate", "db_migrating_password": DATABASE_PASSWORD,
    "backend": "bundled", "zitadel_version": "v4.17.3",
    "zitadel_egress": "10.10.0.0/16",
    "zitadel_db_host": "core-postgres", "zitadel_db_port": "5432",
    "zitadel_db_name": "firstrun_zitadel", "zitadel_db_role": "firstrun_zitadel",
    "zitadel_db_password": DATABASE_PASSWORD, "zitadel_db_sslmode": "disable",
    "directory": "local", "admin_login": "ada", "admin_password": "Password1!",
    "dashboard_url": "http://dashboard-first-run:8080",
    "zitadel_url": "http://zitadel:8080",
    # W7.5: who administers this deployment once it is configured. The local
    # account route names itself, so this stays empty there and the wizard
    # sends the account it is creating.
    "admin_group": "",
}

if ROUTE == "brought":
    ANSWERS.update(BROUGHT_NAMES)

# The firm's own provider. Nothing is asked of a real one: the wizard checks
# that an issuer and a client id are there and writes them, so this needs no
# directory standing by to be a true test of what gets written where.
FIRMS_ISSUER = "https://directory.firm.example"
# Deliberately not `groups`, which is the dashboard's own default: a claim that
# matched the default would pass whether or not the answer reached anything,
# which is how the wizard collected this one into nowhere for as long as it did.
FIRMS_GROUPS_CLAIM = "roles"
ADMIN_GROUP = "meridian-admins"

if BACKEND == "oidc":
    ANSWERS.update({
        "backend": "oidc",
        "oidc_issuer": FIRMS_ISSUER + "/",
        "oidc_client_id": "meridian-dashboard",
        "oidc_client_secret": "shh-dev-only",
        "oidc_groups_claim": FIRMS_GROUPS_CLAIM,
        # Not using the bundle, so it is given no address. Leaving one would
        # have written its issuer over the firm's and hidden the defect this
        # route exists to catch.
        "zitadel_url": "",
        # A directory states a person's groups, so the administrators are a
        # group rather than the one account the local route creates.
        "directory": "ldap",
        "admin_login": "",
        "admin_group": ADMIN_GROUP,
    })


class Scorecard:
    def __init__(self):
        self.failures = []

    def check(self, held, said):
        print(f"    {'ok' if held else 'FAILED'}: {said}", flush=True)
        if not held:
            self.failures.append(said)

    def note(self, said):
        print(f"    {said}", flush=True)


class NoRedirects(urllib.request.HTTPRedirectHandler):
    """A redirect followed silently is a cookie dropped: the second request
    would go without the session the first one just started."""

    def redirect_request(self, *_args, **_kwargs):
        return None


class Browser:
    """A cookie jar and nothing else: the wizard is plain forms."""

    def __init__(self):
        self.cookies = {}
        self.opener = urllib.request.build_opener(NoRedirects)

    def get(self, url):
        return self.send(urllib.request.Request(url))

    def post(self, url, fields):
        data = urllib.parse.urlencode(fields).encode()
        request = urllib.request.Request(url, data=data, method="POST")
        request.add_header("Content-Type", "application/x-www-form-urlencoded")
        return self.send(request)

    def send(self, request):
        if self.cookies:
            request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in self.cookies.items()))
        try:
            response = self.opener.open(request)
            status, body, headers = response.status, response.read().decode(), response.headers
        except urllib.error.HTTPError as refused:
            status, body, headers = refused.code, refused.read().decode(), refused.headers
        for value in headers.get_all("Set-Cookie") or []:
            name, _, rest = value.partition("=")
            self.cookies[name] = rest.split(";")[0]
        return status, body


def json_at(url):
    with urllib.request.urlopen(url) as response:
        return json.loads(response.read())


def wait_for(url, what, seconds=120):
    for _ in range(seconds):
        try:
            urllib.request.urlopen(url, timeout=2).read()
            return
        except Exception:
            time.sleep(1)
    raise SystemExit(f"{what} never answered at {url}")


def main():
    s = Scorecard()
    wait_for(f"{DASHBOARD}/healthz", "the dashboard")
    wait_for(f"{PLATFORM}/e2e/calls", "the stand-in platform")
    wait_for(f"{KUBE}/e2e/state", "the stand-in Kubernetes API")

    print("A: the wizard before anybody is let in", flush=True)
    browser = Browser()
    status, page = browser.get(f"{DASHBOARD}/first-run")
    s.check(status == 200, f"GET /first-run: {status}")
    s.check("first-run/claim" in page, "it asks for a code")
    s.check("first-run/apply" not in page, "and offers nothing else")
    s.check("SHA256:e2e-fingerprint" in page,
            "it shows the fingerprint the platform registered, which is how an "
            "intercepted enrolment code is noticed")

    status, page = browser.get(f"{DASHBOARD}/sign-in")
    s.check(status == 303, f"somebody trying to sign in is sent to the wizard: {status}")

    print("B: a code that is not this deployment's", flush=True)
    status, page = browser.post(f"{DASHBOARD}/first-run/claim", {"code": "NOT-A-CODE"})
    s.check("no such claim code" in page, "the platform's own reason is shown")
    status, page = browser.post(f"{DASHBOARD}/first-run/check", ANSWERS)
    s.check("first-run/claim" in page, "and nothing else is reachable yet")

    print("C: the code, and the wizard", flush=True)
    status, _ = browser.post(f"{DASHBOARD}/first-run/claim", {"code": FIRST_RUN_CODE})
    s.check(status == 303, f"POST /first-run/claim: {status}")
    status, page = browser.get(f"{DASHBOARD}/first-run")
    s.check("first-run/apply" in page, "the wizard itself")

    # Where the first administrator's code comes from, so a failure later says
    # which side lost it: the platform issues it with the first-run one, in
    # the same act (ruling 3).
    redeemed = [c for c in json_at(f"{PLATFORM}/e2e/calls")["calls"]
                if c["path"].endswith("/redeem") and c["answer"].get("redeemed")]
    s.check(bool(redeemed) and redeemed[-1]["answer"].get("first_admin_code") == FIRST_ADMIN_CODE,
            "the platform returned the first administrator's code with it")

    print("D: an answer that does not work", flush=True)
    if ROUTE == "external":
        wrong = dict(ANSWERS, db_serving_role="firstrun_migrate")
        status, page = browser.post(f"{DASHBOARD}/first-run/check", wrong)
        findings = re.findall(r"<li>([^<]*)</li>", page)
        s.note(f"findings: {findings}")
        s.check(any("may create tables" in f for f in findings),
                "a serving role that may create tables is the finding worth having")
        s.check(json_at(f"{KUBE}/e2e/state")["secrets"] == {}, "and nothing was written")
    else:
        # There is nothing to connect to yet: the database this chart brings
        # does not exist until applying starts it, and the roles do not exist
        # until applying makes them. A check that invented a finding here
        # would be a check about nothing.
        s.note("the brought route has no login to get wrong before it is applied")

    print("E: the answers, tested", flush=True)
    status, page = browser.post(f"{DASHBOARD}/first-run/check", ANSWERS)
    s.check("passes" in page, f"the answers pass: {re.findall(r'<li>([^<]*)</li>', page)}")
    s.check(json_at(f"{KUBE}/e2e/state")["secrets"] == {}, "testing writes nothing")

    print("F: applied", flush=True)
    status, page = browser.post(f"{DASHBOARD}/first-run/apply", ANSWERS)
    # W7.6 and decisions/017: nobody redeems anything. Applying recorded who
    # administers this deployment, and the conductor writes the permission
    # when it restarts onto the store it was just given.
    if "administers this deployment" not in page:
        # What the platform was asked and answered says which side lost what,
        # so it is printed here rather than guessed at from a bare failure.
        s.note(f"redemptions: {[c for c in json_at(f'{PLATFORM}/e2e/calls')['calls'] if c['path'].endswith('/redeem')]}")
        # The status and the size, because the page alone cannot tell a refusal
        # from an empty body: this failed in CI on 2026-09-23 with the stripped
        # page printing as blank, and which of those it was decided where to
        # look. Without them the note says only that something went wrong.
        s.note(f"apply answered {status}, body {len(page)} bytes")
        # Every refusal path in `apply` re-renders this form with the reason as
        # a list item, so this is where it says why it stopped. The note printed
        # the head of the page instead until 2026-09-23, which is the title and
        # the stylesheet: a red run had to be diagnosed without ever seeing the
        # refusal it was reporting.
        s.note(f"findings: {re.findall(r'<li>([^<]*)</li>', page)}")
        s.note(f"page: {re.sub(r'<[^>]*>', ' ', page)[:300]}")
    s.check("administers this deployment" in page,
            "the applied page names who administers it")
    # Against the text rather than the markup. The form carries these in an
    # input's `value`, so a raw match passed on the very page that means
    # applying was refused -- it agreed with the check above it that the apply
    # had failed, and still said ok.
    named = ADMIN_GROUP if BACKEND == "oidc" else ANSWERS["admin_login"]
    s.check(named and named in re.sub(r"<[^>]*>", " ", page),
            f"and names who administers it ({named}), with nothing to redeem")
    s.check(FIRST_ADMIN_CODE not in page,
            "no code is shown: there is nothing left to claim")

    state = json_at(f"{KUBE}/e2e/state")
    s.note(f"secrets: {sorted(state['secrets'])}")
    s.check("first-run-database" in state["secrets"], "the database Secret was written")
    database = state["secrets"].get("first-run-database", {})
    s.check("url" in database and "migrate-url" in database,
            "with both logins: the serving one and the one that may migrate")
    if ROUTE == "external":
        s.check(DATABASE_PASSWORD in database.get("url", ""),
                "and the password the wizard was given reached it")
    else:
        # Nobody typed these. The chart made them, the Job read them from its
        # own environment, and they never crossed the bus -- so unlike every
        # other credential here there was nothing to seal, because what would
        # be protected in transit never travelled.
        s.check(os.environ["E2E_BROUGHT_SERVING_PASSWORD"] in database.get("url", ""),
                "the serving URL carries the password the chart generated")
        s.check(os.environ["E2E_BROUGHT_MIGRATING_PASSWORD"] in database.get("migrate-url", ""),
                "and the migrating URL its own")
        s.check(os.environ["E2E_SUPERUSER_PASSWORD"] not in database.get("url", "")
                and os.environ["E2E_SUPERUSER_PASSWORD"] not in database.get("migrate-url", ""),
                "and neither is the privileged one the Job connected with")
        scaled = [c for c in state.get("scaled", [])
                  if c.get("kind") == "statefulsets" and c.get("replicas") == 1]
        s.note(f"scaled: {state.get('scaled')}")
        s.check(bool(scaled), "the database it brought was started, by scaling and not creating")
        zitadel = state["secrets"].get("first-run-zitadel-database", {}).get("dsn", "")
        s.check("zitadel" in zitadel and os.environ["E2E_BROUGHT_ZITADEL_PASSWORD"] in zitadel,
                "Zitadel was handed the role made for it")
        s.check(os.environ["E2E_SUPERUSER_PASSWORD"] not in zitadel,
                "and never the privileged connection the Job used to make it")
    s.check(ANSWERS["db_serving_role"] in database.get("url", "")
            and ANSWERS["db_migrating_role"] in database.get("migrate-url", ""),
            "each URL naming its own role")
    addresses = state["secrets"].get("first-run-addresses", {})
    s.check(addresses.get("dashboard-url") == ANSWERS["dashboard_url"],
            "where a browser reaches this deployment was written, not left in a values file")
    if BACKEND == "bundled":
        s.check(addresses.get("zitadel-external-domain") == "zitadel"
                and addresses.get("zitadel-external-port") == "8080"
                and addresses.get("zitadel-external-secure") == "false",
                f"and Zitadel was told the same address in its own vocabulary: {addresses}")
        s.check(addresses.get("issuer") == ANSWERS["zitadel_url"],
                "which is also the issuer the dashboard will check tokens against")
    else:
        # The chart reads one issuer, out of this Secret. It was written only
        # into the dashboard's OIDC Secret until 2026-09-23 -- which the chart
        # reads the client id out of and not the issuer -- so a deployment that
        # chose the firm's directory came up with no issuer and nobody could
        # sign in. Trailing slash trimmed, because a token says one thing and
        # `https://x/` and `https://x` are not it.
        s.check(addresses.get("issuer") == FIRMS_ISSUER,
                f"the firm's issuer reached the Secret the dashboard reads: {addresses.get('issuer')!r}")
        s.check("zitadel-external-domain" not in addresses,
                "and the bundle was told no address, having not been chosen")

        oidc = state["secrets"].get("first-run-dashboard-oidc", {})
        s.check(oidc.get("client-id") == ANSWERS["oidc_client_id"],
                "the client the firm registered was written")
        s.check(oidc.get("client-secret") == ANSWERS["oidc_client_secret"],
                "and its secret, which travelled sealed and was opened here")
        # The claim the wizard asked for. Collected and dropped on the floor
        # until 2026-09-23: never a form field, never written, and the chart
        # hardcoded the variable -- four links and none of them joined.
        s.check(oidc.get("groups-claim") == FIRMS_GROUPS_CLAIM,
                f"and the groups claim it was told: {oidc.get('groups-claim')!r}")

        # Scaled down rather than never started, which is what the chart does
        # today: the bundle renders at one and the Job turns it off when the
        # firm's own directory is chosen instead.
        off = [c for c in state.get("scaled", [])
               if c.get("kind") == "deployments" and c.get("replicas") == 0]
        s.note(f"scaled: {state.get('scaled')}")
        s.check(len(off) == 2,
                f"and the bundled Zitadel and its login page were switched off: {off}")
    if BACKEND == "bundled":
        s.check(state["policies"].get("first-run-egress") is not None,
                "the one policy it may patch was patched")
        s.check(any("10.10.0.0/16" in json.dumps(rule) for rule in
                    state["policies"].get("first-run-egress") or []),
                "with the ranges the administrator confirmed")
    else:
        # The egress policy says where the bundled Zitadel may reach. There is
        # no bundled Zitadel on this route, so patching one would be opening a
        # path for something that is not running.
        s.check(state["policies"].get("first-run-egress") is None,
                "no egress was opened, there being no bundled directory to let out")

    # The two it was told to restart, by name. This asked whether *every*
    # deployment the stand-in had heard of was restarted until 2026-09-23,
    # which held only because nothing else ever touched one -- and stopped
    # holding the moment a route appeared that scales the bundle to zero.
    # Those two were never meant to restart; they were meant to stop.
    restarted = ["first-run-conductor", "first-run-dashboard"]
    s.check(all(state["deployments"].get(name, {}).get("restarted") for name in restarted),
            f"the named deployments were restarted: {restarted}")
    s.check("first-run" in state["deleted"],
            "and the Job deleted its own binding, so nothing keeps its rights")
    s.check(state["refused"] == [],
            f"it asked for nothing else: {state['refused']}")

    print("G: the administrator the wizard named", flush=True)
    # Decisions/017: applying writes the permission, and nothing is redeemed
    # for it afterwards. The local-account route names the account the wizard
    # creates, so nobody types a login twice.
    s.check("first-run/claim" not in page,
            "the applied page does not send anybody back to redeem anything")

    print("G: what the platform was told", flush=True)
    calls = json_at(f"{PLATFORM}/e2e/calls")
    paths = [c["path"] for c in calls["calls"]]
    s.note(f"calls: {paths}")
    s.check(any(p.endswith("/keys/enrol") for p in paths),
            "the deployment enrolled its own key")
    s.check(calls["state"]["enrolled"], "and the platform holds its public half")
    redemptions = [c for c in calls["calls"] if c["path"].endswith("/redeem")]
    s.check(all(sorted(c["body_keys"]) == ["code", "purpose"] for c in redemptions),
            "each redemption carried the code and its purpose, and nothing about a person")

    print("H: the same answers through the endpoints a CLI calls", flush=True)
    # The CLI holds a port-forward and posts to these same endpoints, behind
    # the same code: one implementation, so the two routes cannot drift.
    before = json_at(f"{KUBE}/e2e/state")["secrets"]
    scripted = Browser()
    status, page = scripted.post(f"{DASHBOARD}/first-run/apply", ANSWERS)
    s.check("first-run/claim" in page, "without a session it is refused, as a browser is")
    after = json_at(f"{KUBE}/e2e/state")["secrets"]
    s.check(before == after, "and it wrote nothing")

    print()
    if s.failures:
        print(f"e2e-first-run FAILED: {len(s.failures)}", flush=True)
        for failure in s.failures:
            print(f"  - {failure}", flush=True)
        return 1
    print("e2e-first-run OK", flush=True)
    return 0


sys.exit(main())
