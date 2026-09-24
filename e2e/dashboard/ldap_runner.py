"""Signing in against the firm's LDAP, bound by the dashboard itself.

Decision 018: the deployment runs no identity server. This is the branch where
a firm has LDAP, and the whole of it happens over HTTP between a browser and
the dashboard, plus LDAP between the dashboard and the directory. Nothing is
redirected anywhere, so there is no issuer, no second address, and no token.

Self-contained rather than importing the Zitadel suite's runner, because that
suite's subject is being deleted and this one has to outlive it.

Phases, because the directory is changed between them by the make target:
  main           -- somebody signs in, and their groups decide what they see
  after-removal  -- the same person, one group poorer
"""
import json
import os
import re
import sys
import time
import urllib.parse
import urllib.request

DASHBOARD = os.environ.get("DASHBOARD", "http://dashboard:8080")
CLAIM_CODE = os.environ["E2E_CLAIM_CODE"]
STATE = os.environ.get("E2E_STATE", "/state")
RESULTS = os.path.join(STATE, "ldap-results.json")

DN_A = "cn=ldap-group-a,ou=groups,dc=example,dc=org"
DN_B = "cn=ldap-group-b,ou=groups,dc=example,dc=org"

FAILURES = []
NOTES = []


def say(line):
    print(line, flush=True)


def check(condition, line):
    NOTES.append(("ok: " if condition else "FAILED: ") + line)
    say("    " + NOTES[-1])
    if not condition:
        FAILURES.append(line)


def note(line):
    say("    " + line)


class Browser:
    """Cookies, and redirects left for the caller to follow so every hop is
    visible."""

    def __init__(self, cookies=None):
        self.cookies = dict(cookies or {})

    def send(self, method, url, fields=None):
        data = urllib.parse.urlencode(fields).encode() if fields is not None else None
        request = urllib.request.Request(url, data=data, method=method)
        if fields is not None:
            request.add_header("Content-Type", "application/x-www-form-urlencoded")
        if self.cookies:
            request.add_header(
                "Cookie", "; ".join(f"{k}={v}" for k, v in self.cookies.items())
            )
        opener = urllib.request.build_opener(NoRedirect)
        try:
            response = opener.open(request)
            status, body, headers = response.status, response.read().decode(), response.headers
        except urllib.error.HTTPError as refused:
            status, body, headers = refused.code, refused.read().decode(), refused.headers
        for value in headers.get_all("Set-Cookie") or []:
            name, _, rest = value.partition("=")
            self.cookies[name] = rest.split(";")[0]
        return Reply(status, body, headers.get("Location"))

    def get(self, url):
        return self.send("GET", url)

    def post(self, url, fields):
        return self.send("POST", url, fields)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


class Reply:
    def __init__(self, status, body, location):
        self.status = status
        self.body = body
        self.location = location


def dash(path):
    return DASHBOARD + path


def form_token(page):
    found = re.search(r'name="form_token" value="([^"]+)"', page.body)
    return found.group(1) if found else ""


def sign_in(browser, name, password):
    """The whole of it: a form, a password, a session. No redirect anywhere."""
    page = browser.get(dash("/sign-in"))
    if page.status != 200:
        return page
    return browser.post(dash("/sign-in"), {"name": name, "password": password})


def home(browser):
    return browser.get(dash("/"))


def signed_in_as(page):
    found = re.search(r"Signed in as <strong>([^<]*)</strong>", page.body)
    return found.group(1) if found else None


def is_admin_home(page):
    # What home says, which is not what /admin's heading says. Copying the
    # wrong string made bob look unprivileged on a page that had just let him
    # into /admin.
    return "You are a deployment admin" in page.body


def wait_dashboard():
    for _ in range(120):
        try:
            if Browser().get(dash("/healthz")).status == 200:
                return
        except OSError:
            pass
        time.sleep(1)
    sys.exit("the dashboard never became ready")


def remember(key, value):
    held = load()
    held[key] = value
    with open(RESULTS, "w") as out:
        json.dump(held, out)


def recall(key):
    return load().get(key)


def load():
    try:
        with open(RESULTS) as held:
            return json.load(held)
    except (OSError, ValueError):
        return {}


def main_phase():
    say("A: somebody the directory knows signs in, with no redirect anywhere")
    alice = Browser()
    page = alice.get(dash("/sign-in"))
    check(
        page.status == 200 and 'name="password"' in page.body,
        f"the dashboard serves its own form: {page.status}",
    )
    # Not a redirect. There is nowhere to redirect to, and that is the point.
    check(page.location is None, f"and sends the browser nowhere: {page.location!r}")

    refused = alice.post(dash("/sign-in"), {"name": "alice", "password": "not-her-password"})
    check(refused.status == 401, f"a wrong password: {refused.status}")
    check(
        "were not accepted" in refused.body and "alice" not in refused.body,
        "and the refusal names neither half, so the page cannot be used to find who works here",
    )

    signed = sign_in(alice, "alice", "alicepass")
    check(
        signed.status == 303 and signed.location == "/",
        f"alice signs in: {signed.status} to {signed.location!r}",
    )
    page = home(alice)
    check(signed_in_as(page) == "Alice Ldap", f"home says {signed_in_as(page)!r}")

    say("B: the claim makes her this deployment's first administrator")
    claim = alice.get(dash("/claim"))
    check(claim.status == 200, f"GET /claim {claim.status}")
    redeemed = alice.post(
        dash("/claim"), {"code": CLAIM_CODE, "form_token": form_token(claim)}
    )
    check(
        redeemed.status == 303 and redeemed.location == "/admin",
        f"the platform's code: {redeemed.status} to {redeemed.location!r}",
    )

    say("C: a user group for the directory group bob is in")
    admin = alice.get(dash("/admin"))
    check(admin.status == 200, f"GET /admin as the redeemer: {admin.status}")
    token_value = form_token(admin)
    made = alice.post(
        dash("/admin/user-groups"),
        {
            "user_group_id": "",
            "name": "LDAP group B",
            "directory_groups": DN_B,
            "logins": "",
            "form_token": token_value,
        },
    )
    check(made.status in (200, 303), f"a user group for {DN_B}: {made.status}")

    admin = alice.get(dash("/admin"))
    group_id = ""
    for row in re.findall(r"<tr><td>([^<]*)</td><td>([^<]*)</td>", admin.body):
        if row[1] == "LDAP group B":
            group_id = row[0]
    check(bool(group_id), f"the group has an identifier: {group_id!r}")

    granted = alice.post(
        dash("/admin/permissions"),
        {
            "user_group_id": group_id,
            "account_group_id": "",
            "access_group_id": "deployment-admin",
            "form_token": form_token(admin),
        },
    )
    check(granted.status in (200, 303), f"deployment admin for that group: {granted.status}")

    say("D: bob's groups reach the dashboard through the bind, and decide what he sees")
    bob = Browser()
    signed = sign_in(bob, "bob", "bobpass")
    check(
        signed.status == 303,
        f"bob signs in with a password the directory checked: {signed.status}",
    )
    page = home(bob)
    check(
        signed_in_as(page) == "Bob Ldap" and is_admin_home(page),
        f"home: signed in as {signed_in_as(page)!r}, deployment admin shown: {is_admin_home(page)}",
    )
    admin = bob.get(dash("/admin"))
    check(admin.status == 200, f"GET /admin as bob, who is in {DN_B}: {admin.status}")
    remember("bob_cookies", bob.cookies)


def after_removal_phase():
    say("D: bob, one group poorer")
    note("-- bob removed from ldap-group-b at the directory (by the make target) --")
    bob = Browser()
    signed = sign_in(bob, "bob", "bobpass")
    check(signed.status == 303, f"bob signs in again: {signed.status}")

    page = home(bob)
    check(
        signed_in_as(page) == "Bob Ldap" and not is_admin_home(page),
        f"home: signed in as {signed_in_as(page)!r}, deployment admin shown: {is_admin_home(page)}",
    )
    admin = bob.get(dash("/admin"))
    check(admin.status == 403, f"GET /admin in the new session: {admin.status}")

    earlier = Browser(recall("bob_cookies")).get(dash("/admin"))
    note(
        f"bob's earlier session, still live, GET /admin: {earlier.status} "
        "(by design: access held at sign-in lasts until that session ends, "
        "spec requirement 4)"
    )


def main():
    phase = sys.argv[1] if len(sys.argv) > 1 else "main"
    wait_dashboard()
    if phase == "main":
        main_phase()
    elif phase == "after-removal":
        after_removal_phase()
    else:
        sys.exit(f"unknown phase {phase}")

    if FAILURES:
        say("")
        say(f"e2e-dashboard-ldap FAILED: {len(FAILURES)}")
        for failure in FAILURES:
            say(f"  - {failure}")
        sys.exit(1)


if __name__ == "__main__":
    main()
