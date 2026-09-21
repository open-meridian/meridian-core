"""The dashboard's sign-in and access, end to end, against the bundled Zitadel.

Plays two parts, with the standard library only:

- a browser, as far as the dashboard can tell: a cookie jar that follows the
  dashboard's redirects to Zitadel and back;
- Zitadel's Login v2 UI, as far as Zitadel can tell: the authorisation
  request's id is read from the redirect to the login page, a session is made
  through the v2 session API with a password check or an LDAP intent, and
  CreateCallback finishes the request, exactly as the UI does
  (meridian-design, reference/zitadel-group-trial/rp.py and brokered.py).

Run in phases, because two steps belong to the make target rather than to a
container on the network: changing the LDAP directory, and restarting Zitadel.

    runner.py main            A, B, C, D (before), E, F, H
    runner.py after-removal   D (after the LDAP change)
    runner.py after-restart   G
    runner.py report          the table; exit 1 unless every scenario is OK

Results accumulate in /state/results.json between phases.
"""
import base64
import hashlib
import hmac
import html
import http.client
import json
import os
import re
import secrets
import sys
import time
import traceback
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from zt import ApiError, call, jwt_payload, token, wait_ready  # noqa: E402

ZITADEL = os.environ.get("ZITADEL", "http://zitadel:8080")
DASHBOARD = os.environ.get("DASHBOARD", "http://dashboard:8080")
GROUP_HOOK = os.environ.get("GROUP_HOOK", "http://group-hook:8090")
FAKE_PLATFORM = os.environ.get("FAKE_PLATFORM", "http://fake-platform:8000")
CLAIM_CODE = os.environ["E2E_CLAIM_CODE"]
STATE = "/state"
RESULTS = os.path.join(STATE, "results.json")

DN_A = "cn=ldap-group-a,ou=groups,dc=example,dc=org"
DN_B = "cn=ldap-group-b,ou=groups,dc=example,dc=org"
SKEW_S = 30  # the dashboard's allowance, crates/dashboard/src/oidc.rs SKEW_NS
EXPECTED = ["A", "A2", "B", "C", "D", "E", "F", "G", "H"]
TITLES = {
    "A": "Zitadel-native person signs in; groups from their role via the hook",
    "A2": "a Zitadel role reaches the dashboard as a directory group",
    "B": "claim: redeem once, become deployment admin, refused again",
    "C": "deployment admin creates an account, user group (LDAP DN), account group",
    "D": "LDAP person's groups reach the dashboard; a removed group is gone",
    "E": "freshness: a Zitadel session reused without the directory",
    "F": "group hook refuses unsigned and wrongly signed calls",
    "G": "Zitadel restarted: existing people still sign in",
    "H": "dashboard refuses a callback whose state cookie does not match",
}


def say(line):
    print(line, flush=True)


# ── A browser ───────────────────────────────────────────────────────────────

class Response:
    def __init__(self, status, headers, body):
        self.status = status
        self.headers = headers
        self.body = body

    def header(self, name):
        return next((v for k, v in self.headers if k.lower() == name.lower()), None)

    def set_cookies(self):
        return [v for k, v in self.headers if k.lower() == "set-cookie"]

    @property
    def location(self):
        return self.header("Location")

    def text(self):
        """The page's text, tags dropped, for evidence lines."""
        stripped = re.sub(r"<(style|head)\b.*?</\1>", " ", self.body, flags=re.S)
        stripped = re.sub(r"<[^>]+>", " ", stripped)
        return re.sub(r"\s+", " ", html.unescape(stripped)).strip()


class Browser:
    """Cookies by host and path; redirects are the caller's to follow, so
    every hop is visible."""

    def __init__(self, cookies=None):
        self.cookies = cookies or {}  # host -> name -> (value, path)

    def request(self, method, url, form=None):
        parts = urllib.parse.urlsplit(url)
        host = parts.netloc
        path = parts.path or "/"
        if parts.query:
            path += "?" + parts.query
        headers = {}
        jar = self.cookies.get(host, {})
        sent = [f"{n}={v}" for n, (v, p) in jar.items() if (parts.path or "/").startswith(p)]
        if sent:
            headers["Cookie"] = "; ".join(sent)
        body = None
        if form is not None:
            body = urllib.parse.urlencode(form)
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=30)
        conn.request(method, path, body=body, headers=headers)
        raw = conn.getresponse()
        response = Response(raw.status, raw.getheaders(), raw.read().decode(errors="replace"))
        conn.close()
        for cookie in response.set_cookies():
            self._keep(host, cookie)
        return response

    def _keep(self, host, header):
        pair, *attributes = [p.strip() for p in header.split(";")]
        name, value = pair.split("=", 1)
        attrs = {a.split("=", 1)[0].lower(): (a.split("=", 1)[1] if "=" in a else "") for a in attributes}
        jar = self.cookies.setdefault(host, {})
        if attrs.get("max-age") == "0" or value == "":
            jar.pop(name, None)
        else:
            jar[name] = (value, attrs.get("path", "/"))

    def get(self, url):
        return self.request("GET", url)

    def post(self, url, form):
        return self.request("POST", url, form)

    def cookie(self, name, host="dashboard:8080"):
        found = self.cookies.get(host, {}).get(name)
        return found[0] if found else None


def dash(path):
    return DASHBOARD + path


def form_token(page):
    found = re.search(r'name="form_token" value="([^"]+)"', page.body)
    if not found:
        raise AssertionError(f"no form token on the page: {page.text()[:200]}")
    return html.unescape(found.group(1))


# ── Zitadel's Login v2, through its API ─────────────────────────────────────

def auth_request_id(login_redirect):
    """Zitadel sends the browser to the login UI with the request's id."""
    query = urllib.parse.parse_qs(urllib.parse.urlsplit(login_redirect).query)
    return query["authRequest"][0]


def password_session(login_name, password):
    return call("POST", "/v2/sessions", {"checks": {"user": {"loginName": login_name},
                                                    "password": {"password": password}}}, pat="login-client.pat")


def ldap_session(username, password, trace):
    """An LDAP sign-in brokered by Zitadel, as Login v2 performs it: start the
    intent (Zitadel binds to the directory), retrieve it (the group hook runs
    on this response), apply the user it describes, then a session checked by
    the intent."""
    idp = token("ldap-idp-id")
    started = call("POST", "/v2/idp_intents", {"idpId": idp, "ldap": {"username": username, "password": password}},
                   pat="login-client.pat")["idpIntent"]
    retrieved = call("POST", f"/v2/idp_intents/{started['idpIntentId']}",
                     {"idpIntentToken": started["idpIntentToken"]}, pat="login-client.pat")
    trace["memberOf_from_directory"] = ((retrieved.get("idpInformation") or {}).get("ldap") or {}).get(
        "attributes", {}).get("memberOf")
    uid = retrieved.get("userId")
    if uid:
        update = retrieved.get("updateUser") or {}
        body = {"metadata": update.get("metadata", [])}
        human = {k: v for k, v in (update.get("human") or {}).items() if k in ("profile", "email", "phone")}
        if human:
            body["human"] = human
        trace["hook_wrote"] = decode_groups_metadata(update.get("metadata", []))
        call("PATCH", f"/v2/users/{uid}", body, pat="login-client.pat")
        trace["zitadel_user"] = "existing, updated from the directory"
    else:
        create = dict(retrieved["createUser"])
        create["organizationId"] = token("org-id")
        trace["hook_wrote"] = decode_groups_metadata(create.get("metadata", []))
        uid = call("POST", "/v2/users/new", create, pat="login-client.pat")["id"]
        trace["zitadel_user"] = "created on first sign-in"
    trace["user_id"] = uid
    for attempt in range(20):  # the new user's projection is eventually consistent
        try:
            return call("POST", "/v2/sessions", {"checks": {
                "user": {"userId": uid},
                "idpIntent": {"idpIntentId": started["idpIntentId"], "idpIntentToken": started["idpIntentToken"]}}},
                pat="login-client.pat")
        except ApiError:
            if attempt == 19:
                raise
            time.sleep(0.5)


def decode_groups_metadata(entries):
    for entry in entries or []:
        if entry.get("key") == "groups":
            return json.loads(base64.b64decode(entry["value"]))
    return None


def create_callback(auth_request, session):
    """CreateCallback: Zitadel finishes the request and says where the
    browser goes next, or refuses."""
    return call("POST", f"/v2/oidc/auth_requests/{auth_request}",
                {"session": {"sessionId": session["sessionId"], "sessionToken": session["sessionToken"]}},
                pat="login-client.pat")["callbackUrl"]


def metadata_groups(user_id):
    entries = call("POST", f"/v2/users/{user_id}/metadata/search", {}).get("metadata", [])
    return decode_groups_metadata(entries)


# ── Signing in to the dashboard ─────────────────────────────────────────────

def dashboard_sign_in(browser, session_for):
    """/sign-in, to Zitadel, a session, back to /callback. `session_for`
    makes (or reuses) the Zitadel session once the request exists."""
    out = {"started_at": time.time()}
    begun = browser.get(dash("/sign-in"))
    assert begun.status == 303, f"/sign-in answered {begun.status}: {begun.text()[:300]}"
    authorize = begun.location
    query = urllib.parse.parse_qs(urllib.parse.urlsplit(authorize).query)
    out["authorize"] = {k: query[k][0] for k in ("prompt", "max_age", "code_challenge_method", "scope") if k in query}
    out["signin_cookie_set"] = browser.cookie("meridian_signin") is not None
    to_login = browser.get(authorize)
    assert to_login.status in (302, 303), f"authorize answered {to_login.status}: {to_login.body[:300]}"
    request_id = auth_request_id(to_login.location)
    out["auth_request"] = request_id
    seen = call("GET", f"/v2/oidc/auth_requests/{request_id}", pat="login-client.pat")["authRequest"]
    out["zitadel_saw"] = {k: seen.get(k) for k in ("prompt", "maxAge", "scope", "redirectUri")}
    session = session_for()
    out["zitadel_session"] = session["sessionId"]
    out["session"] = session
    try:
        callback = create_callback(request_id, session)
    except ApiError as refused:
        out["zitadel_refused"] = refused.body
        return out
    out["callback_url"] = callback
    back = browser.get(callback)
    out["callback_status"] = back.status
    out["callback_location"] = back.location
    out["callback_text"] = back.text()[:400] if back.status != 303 else ""
    out["session_cookie"] = browser.cookie("meridian_session") is not None
    return out


def direct_id_token(session, prompt_login=False):
    """The same client, the same Zitadel session, but the code exchanged here
    rather than by the dashboard: the ID token Zitadel issues, decoded, as
    evidence of what the dashboard was given. PKCE, as the dashboard does."""
    verifier = secrets.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    params = {"client_id": token("client-id"), "redirect_uri": "http://dashboard:8080/callback",
              "response_type": "code", "scope": "openid profile email", "state": secrets.token_urlsafe(8),
              "nonce": secrets.token_urlsafe(8), "code_challenge": challenge, "code_challenge_method": "S256"}
    if prompt_login:
        params.update({"prompt": "login", "max_age": "0"})
    redirect = Browser().get(ZITADEL + "/oauth/v2/authorize?" + urllib.parse.urlencode(params))
    callback = create_callback(auth_request_id(redirect.location), session)
    code = urllib.parse.parse_qs(urllib.parse.urlsplit(callback).query)["code"][0]
    conn = http.client.HTTPConnection("zitadel", 8080, timeout=30)
    conn.request("POST", "/oauth/v2/token", urllib.parse.urlencode({
        "grant_type": "authorization_code", "code": code, "redirect_uri": "http://dashboard:8080/callback",
        "client_id": token("client-id"), "code_verifier": verifier}),
        {"Content-Type": "application/x-www-form-urlencoded"})
    raw = conn.getresponse()
    body = json.loads(raw.read())
    assert raw.status == 200, f"token exchange {raw.status}: {body}"
    return jwt_payload(body["id_token"])


def home(browser):
    page = browser.get(dash("/"))
    return page


def signed_in_as(page):
    found = re.search(r"Signed in as <strong>([^<]*)</strong>", page.body)
    return html.unescape(found.group(1)) if found else None


def is_admin_home(page):
    return "You are a deployment admin" in page.body


# ── Results ─────────────────────────────────────────────────────────────────

def load():
    if os.path.exists(RESULTS):
        with open(RESULTS) as f:
            return json.load(f)
    return {"results": {}, "memory": {}}


STORE = load()


def save():
    with open(RESULTS + ".tmp", "w") as f:
        json.dump(STORE, f, indent=2)
    os.replace(RESULTS + ".tmp", RESULTS)


def remember(key, value):
    STORE["memory"][key] = value
    save()


def recall(key):
    return STORE["memory"].get(key)


class Scenario:
    """Collects evidence; a failed check or an exception is FAILED."""

    def __init__(self, key):
        self.key = key
        self.evidence = []
        self.ok = True

    def note(self, line):
        self.evidence.append(line)
        say(f"  [{self.key}] {line}")

    def check(self, condition, line):
        self.note(("ok: " if condition else "FAILED: ") + line)
        if not condition:
            self.ok = False
        return condition

    def __enter__(self):
        say(f"── {self.key}: {TITLES[self.key]}")
        return self

    def __exit__(self, kind, value, tb):
        if kind is not None:
            self.ok = False
            self.note(f"FAILED with {kind.__name__}: {value}")
            traceback.print_exception(kind, value, tb)
        previous = STORE["results"].get(self.key)
        if previous:  # a scenario split across phases is OK only if every part is
            self.evidence = previous["evidence"] + self.evidence
            self.ok = self.ok and previous["ok"]
        STORE["results"][self.key] = {"ok": self.ok, "evidence": self.evidence}
        save()
        say(f"── {self.key}: {'OK' if self.ok else 'FAILED'}")
        return True  # keep going to the next scenario


# ── The scenarios ───────────────────────────────────────────────────────────

def scenario_a():
    with Scenario("A") as s:
        ada = Browser()
        signed = dashboard_sign_in(ada, lambda: password_session("ada", "Password1!"))
        s.check(signed["authorize"].get("prompt") == "login" and signed["authorize"].get("max_age") == "0"
                and signed["authorize"].get("code_challenge_method") == "S256",
                f"the dashboard sent Zitadel {signed['authorize']}")
        s.note(f"Zitadel's view of that request: {signed['zitadel_saw']}")
        s.check(signed.get("callback_status") == 303 and signed.get("callback_location") == "/"
                and signed["session_cookie"],
                f"/callback answered {signed.get('callback_status')} to {signed.get('callback_location')!r}, "
                f"meridian_session set: {signed['session_cookie']} {signed.get('callback_text', '')}")
        page = home(ada)
        s.check(page.status == 200 and signed_in_as(page) == "Ada Native",
                f"GET / {page.status}: signed in as {signed_in_as(page)!r}")
        s.check("Claim it" in page.body, "home offers the claim page: nobody administers the deployment yet")
        claims = direct_id_token(signed["session"])
        s.note(f"ID token for the same Zitadel session, decoded: sub={claims['sub']} iss={claims['iss']} "
               f"auth_time={claims.get('auth_time')} groups={claims.get('groups')} "
               f"urn:zitadel:iam:org:project:roles={sorted((claims.get('urn:zitadel:iam:org:project:roles') or {}))}")
        s.check(claims.get("groups") == ["e2e-staff"],
                "`groups` is ada's role on the dashboard's project; Zitadel emits no `groups` claim itself, "
                "the hook's /token answer is its only source")
        remember("ada_cookies", ada.cookies)
        remember("ada_subject", f"{claims['iss']}|{claims['sub']}")


def scenario_b():
    with Scenario("B") as s:
        ada = Browser(recall("ada_cookies"))
        page = ada.get(dash("/claim"))
        s.check(page.status == 200 and "Claim this deployment" in page.body, f"GET /claim {page.status}")
        token_value = form_token(page)
        wrong = ada.post(dash("/claim"), {"code": "NOT-A-CODE", "form_token": token_value})
        s.check(wrong.status == 400 and "no such claim code" in wrong.body,
                f"a wrong code: {wrong.status}, \"{wrong.text()[:120]}\"")
        redeemed = ada.post(dash("/claim"), {"code": CLAIM_CODE, "form_token": token_value})
        s.check(redeemed.status == 303 and redeemed.location == "/admin",
                f"the platform's code: {redeemed.status} to {redeemed.location!r}")
        admin = ada.get(dash("/admin"))
        s.check(admin.status == 200 and "Administer this deployment" in admin.body,
                f"GET /admin as the redeemer: {admin.status}")
        rows = re.findall(r"<tr><td>([^<]*)</td><td>([^<]*)</td><td>([^<]*)</td><td>([^<]*)</td></tr>", admin.body)
        s.note(f"user groups now: {[(r[1], r[3]) for r in rows]}")
        again = ada.post(dash("/claim"), {"code": CLAIM_CODE, "form_token": token_value})
        s.check(again.status == 400 and "Not redeemed" in again.body,
                f"redeeming again: {again.status}, \"{again.text()[:140]}\"")
        page = ada.get(dash("/claim"))
        s.check(page.status == 409, f"GET /claim once claimed: {page.status}")
        seen = json.loads(Browser().get(FAKE_PLATFORM + "/e2e/redemptions").body)["attempts"]
        s.note(f"what the platform was asked: {seen}")
        s.check([a.get("redeemed") for a in seen] == [False, True],
                "the platform was asked twice (wrong code, then the right one) and never a third time: "
                "the conductor refused the repeat before spending a call")
        s.check(all(a.get("body_keys") == ["code"] for a in seen),
                "the platform was sent the code and nothing about who redeemed it")


def admin_rows(page, section):
    """Rows of one table on /admin, as lists of cell text."""
    start = page.body.index(f"<h2>{section}</h2>")
    end = page.body.find("</table>", start)
    table = page.body[start:end]
    rows = re.findall(r"<tr>(.*?)</tr>", table)
    return [[html.unescape(c) for c in re.findall(r"<td>(.*?)</td>", r)] for r in rows if "<td>" in r]


def scenario_c():
    with Scenario("C") as s:
        ada = Browser(recall("ada_cookies"))
        page = ada.get(dash("/admin"))
        token_value = form_token(page)

        def post(path, fields):
            answered = ada.post(dash(path), {**fields, "form_token": token_value})
            s.check(answered.status == 303, f"POST {path} {fields}: {answered.status} {answered.text()[:160] if answered.status != 303 else ''}")

        post("/admin/accounts", {"account_id": "", "name": "E2E Account"})
        page = ada.get(dash("/admin"))
        account = next((r[0] for r in admin_rows(page, "Accounts") if r[1] == "E2E Account"), None)
        s.check(account is not None, f"the account appears on /admin as {account}")
        post("/admin/user-groups", {"user_group_id": "", "name": "LDAP group B", "directory_groups": DN_B, "logins": ""})
        post("/admin/user-groups", {"user_group_id": "", "name": "Staff role", "directory_groups": "e2e-staff",
                                    "logins": ""})
        post("/admin/account-groups", {"account_group_id": "", "name": "E2E accounts", "account_ids": account or ""})
        page = ada.get(dash("/admin"))
        groups = {r[1]: r for r in admin_rows(page, "User groups")}
        s.check("LDAP group B" in groups and groups["LDAP group B"][2] == DN_B,
                f"user group 'LDAP group B' on /admin names {groups.get('LDAP group B', [None] * 3)[2]!r}")
        s.check("Staff role" in groups, "user group 'Staff role' on /admin names 'e2e-staff'")
        account_groups = {r[1]: r for r in admin_rows(page, "Account groups")}
        s.check("E2E accounts" in account_groups and account_groups["E2E accounts"][2] == account,
                f"account group 'E2E accounts' on /admin lists {account_groups.get('E2E accounts', [None] * 3)[2]!r}")
        # What makes membership observable on the dashboard's own pages: each
        # user group reaches deployment admin, so /admin answers 200 through
        # it and 403 without it. Nothing else a person can hold is visible yet:
        # an access group entry needs a plugin that has reported, and none has.
        for name in ("LDAP group B", "Staff role"):
            post("/admin/permissions", {"user_group_id": groups[name][0], "account_group_id": "",
                                        "access_group_id": "deployment-admin"})
        page = ada.get(dash("/admin"))
        permissions = admin_rows(page, "Permissions")
        granted = {row[1] for row in permissions if row[3] == "deployment-admin"}
        s.check({groups["LDAP group B"][0], groups["Staff role"][0]} <= granted,
                f"/admin lists permissions to deployment-admin for {sorted(granted)}")
        remember("user_groups", {k: v[0] for k, v in groups.items()})


def scenario_a2():
    with Scenario("A2") as s:
        cy = Browser()
        signed = dashboard_sign_in(cy, lambda: password_session("cy", "Password1!"))
        s.check(signed.get("callback_status") == 303, f"cy signs in: /callback {signed.get('callback_status')}")
        claims = direct_id_token(signed["session"])
        s.note(f"cy's ID token groups={claims.get('groups')}; cy is in no user group's logins")
        admin = cy.get(dash("/admin"))
        s.check(admin.status == 200,
                f"GET /admin as cy: {admin.status}, reached only through user group 'Staff role' naming the role")


def scenario_d_before():
    with Scenario("D") as s:
        bob = Browser()
        trace = {}
        signed = dashboard_sign_in(bob, lambda: ldap_session("bob", "bobpass", trace))
        s.note(f"the directory said memberOf={trace.get('memberOf_from_directory')}; "
               f"the hook wrote groups={trace.get('hook_wrote')} ({trace.get('zitadel_user')})")
        s.check(signed.get("callback_status") == 303 and signed["session_cookie"],
                f"bob signs in through the LDAP provider: /callback {signed.get('callback_status')}")
        page = home(bob)
        s.check(signed_in_as(page) == "Bob Ldap" and is_admin_home(page),
                f"home: signed in as {signed_in_as(page)!r}, deployment admin shown: {is_admin_home(page)}")
        admin = bob.get(dash("/admin"))
        s.check(admin.status == 200, f"GET /admin as bob, in {DN_B}: {admin.status}")
        claims = direct_id_token(signed["session"])
        s.check(claims.get("groups") == [DN_A, DN_B], f"bob's ID token groups={claims.get('groups')}")
        s.note(f"Zitadel metadata `groups` on bob: {metadata_groups(trace['user_id'])}")
        remember("bob_cookies", bob.cookies)
        remember("bob_user_id", trace["user_id"])


def scenario_d_after():
    with Scenario("D") as s:
        s.note("-- bob removed from ldap-group-b at the directory (by the make target) --")
        bob = Browser()
        trace = {}
        signed = dashboard_sign_in(bob, lambda: ldap_session("bob", "bobpass", trace))
        s.note(f"the directory said memberOf={trace.get('memberOf_from_directory')}; "
               f"the hook wrote groups={trace.get('hook_wrote')}")
        s.check(signed.get("callback_status") == 303, f"bob signs in again: /callback {signed.get('callback_status')}")
        claims = direct_id_token(signed["session"])
        s.check(claims.get("groups") == [DN_A], f"bob's ID token groups={claims.get('groups')}")
        page = home(bob)
        s.check(signed_in_as(page) == "Bob Ldap" and not is_admin_home(page),
                f"home: signed in as {signed_in_as(page)!r}, deployment admin shown: {is_admin_home(page)}")
        admin = bob.get(dash("/admin"))
        s.check(admin.status == 403, f"GET /admin in the new session: {admin.status}")
        earlier = Browser(recall("bob_cookies")).get(dash("/admin"))
        s.note(f"bob's earlier dashboard session, still live, GET /admin: {earlier.status} "
               "(by design: access held at sign-in lasts until that session ends, spec requirement 4)")


def scenario_e():
    with Scenario("E") as s:
        alice = Browser()
        trace = {}
        first = dashboard_sign_in(alice, lambda: ldap_session("alice", "alicepass", trace))
        s.check(first.get("callback_status") == 303, f"alice signs in at the directory: /callback "
                                                     f"{first.get('callback_status')}")
        zitadel_session = first["session"]
        factors = call("GET", f"/v2/sessions/{zitadel_session['sessionId']}")["session"]["factors"]
        verified_at = factors.get("intent", {}).get("verifiedAt")
        s.note(f"her Zitadel session {zitadel_session['sessionId']}: intent verified at {verified_at}")
        wait = SKEW_S + 10
        s.note(f"waiting {wait}s, past the dashboard's {SKEW_S}s allowance, with that session alive")
        time.sleep(wait)
        # A second dashboard sign-in, finished with the same Zitadel session:
        # what Login v2 would do if it offered "continue as alice" instead of
        # sending her to the directory again.
        second = dashboard_sign_in(alice, lambda: zitadel_session)
        started = second["started_at"]
        s.note(f"second sign-in started at {started:.3f} "
               f"({time.strftime('%H:%M:%S', time.gmtime(started))}Z), sent {second['authorize']}")
        if "zitadel_refused" in second:
            s.note(f"Zitadel refused to finish the request with the old session: {second['zitadel_refused']}")
            s.check(True, "no token was issued, so no dashboard session could follow")
            remember("E_outcome", "Zitadel refused CreateCallback")
            return
        s.note("Zitadel issued a code for the old session, despite prompt=login and max_age=0 "
               "(the session API finishes whatever session it is handed)")
        claims = direct_id_token(zitadel_session, prompt_login=True)
        auth_time = claims.get("auth_time")
        s.note(f"an ID token for the same session and the same parameters: auth_time={auth_time} "
               f"({time.strftime('%H:%M:%S', time.gmtime(auth_time))}Z); sign-in start minus auth_time = "
               f"{started - auth_time:.1f}s, allowance {SKEW_S}s")
        s.note(f"the dashboard answered /callback with {second.get('callback_status')}: "
               f"\"{second.get('callback_text', '')[:220]}\"")
        refused = second.get("callback_status") == 400 and "earlier sign-in" in second.get("callback_text", "")
        s.check(refused, "the dashboard refused the token on auth_time, and started no session")
        s.check(started - auth_time > SKEW_S, "auth_time is the directory check from the first sign-in, "
                                              "not this one")
        remember("E_outcome", {"zitadel_issued_code": True, "auth_time": auth_time, "sign_in_started": started,
                               "dashboard_status": second.get("callback_status")})


def scenario_f():
    with Scenario("F") as s:
        bob_id = recall("bob_user_id")
        before = metadata_groups(bob_id)
        body = json.dumps({"fullMethod": "/zitadel.user.v2.UserService/RetrieveIdentityProviderIntent",
                           "response": {"idpInformation": {"ldap": {"attributes": {
                               "uid": ["bob"], "memberOf": ["cn=domain-admins,ou=groups,dc=example,dc=org"]}}},
                               "updateUser": {"userId": bob_id, "username": "bob"}}}).encode()
        now = int(time.time())

        def send(path, payload, signature):
            conn = http.client.HTTPConnection("group-hook", 8090, timeout=10)
            headers = {"Content-Type": "application/json"}
            if signature:
                headers["ZITADEL-Signature"] = signature
            conn.request("POST", path, payload, headers)
            raw = conn.getresponse()
            return raw.status, raw.read().decode()

        def sign(key, stamp, payload):
            return f"t={stamp},v1=" + hmac.new(key.encode(), f"{stamp}.".encode() + payload, hashlib.sha256).hexdigest()

        wrong_key = "not-" + token("intent-signing-key")
        for path in ("/intent", "/token"):
            status, reply = send(path, body, None)
            s.check(status == 401 and "metadata" not in reply, f"{path} unsigned: {status} \"{reply}\"")
            status, reply = send(path, body, sign(wrong_key, now, body))
            s.check(status == 401 and "metadata" not in reply, f"{path} signed with the wrong key: {status} \"{reply}\"")
        status, reply = send("/intent", body, sign(token("token-signing-key"), now, body))
        s.check(status == 401, f"/intent signed with the /token target's key: {status} \"{reply}\"")
        status, reply = send("/intent", body, sign(token("intent-signing-key"), now - 301, body))
        s.check(status == 401, f"/intent with its own key but 301s old (a replay): {status} \"{reply}\"")
        status, reply = send("/intent", body, sign(token("intent-signing-key"), now, body))
        s.check(status == 200 and "metadata" in reply,
                f"control: the same body signed with the intent target's key: {status}, a user to write returned")
        after = metadata_groups(bob_id)
        s.check(before == after, f"bob's groups in Zitadel unchanged: before={before} after={after}; the hook "
                                 "writes nothing itself, it only answers Zitadel, which never saw these calls")


def scenario_h():
    with Scenario("H") as s:
        browser = Browser()
        begun = browser.get(dash("/sign-in"))
        state = browser.cookie("meridian_signin")
        to_login = browser.get(begun.location)
        callback = create_callback(auth_request_id(to_login.location), password_session("ada", "Password1!"))
        returned_state = urllib.parse.parse_qs(urllib.parse.urlsplit(callback).query)["state"][0]
        s.note(f"meridian_signin cookie={state[:8]}..., state returned by Zitadel={returned_state[:8]}...")

        tampered = Browser({"dashboard:8080": {"meridian_signin": ("someone-elses-" + secrets.token_urlsafe(8),
                                                                    "/callback")}})
        answered = tampered.get(callback)
        s.check(answered.status == 400 and "started in another browser" in answered.body
                and tampered.cookie("meridian_session") is None,
                f"/callback with a different state cookie: {answered.status}, \"{answered.text()[:110]}\", "
                f"session cookie set: {tampered.cookie('meridian_session') is not None}")
        stranger = Browser()
        answered = stranger.get(callback)
        s.check(answered.status == 400 and stranger.cookie("meridian_session") is None,
                f"/callback with no state cookie at all: {answered.status}")
        answered = browser.get(callback)
        s.check(answered.status == 303 and browser.cookie("meridian_session") is not None,
                f"control: the same callback in the browser that started it: {answered.status}, session started")


def scenario_g():
    with Scenario("G") as s:
        s.note(f"Zitadel answering again after its container restarted")
        people = call("POST", "/v2/users", {"queries": [{"userNameQuery": {
            "userName": "ada", "method": "TEXT_QUERY_METHOD_EQUALS"}}]})["result"]
        s.check(len(people) == 1, f"ada is still the same Zitadel user: {people[0]['userId'] if people else None}")
        ada = Browser()
        signed = dashboard_sign_in(ada, lambda: password_session("ada", "Password1!"))
        page = home(ada)
        s.check(signed.get("callback_status") == 303 and signed_in_as(page) == "Ada Native" and is_admin_home(page),
                f"ada (made in Zitadel before the restart): /callback {signed.get('callback_status')}, "
                f"signed in as {signed_in_as(page)!r}, deployment admin: {is_admin_home(page)}")
        bob = Browser()
        trace = {}
        signed = dashboard_sign_in(bob, lambda: ldap_session("bob", "bobpass", trace))
        page = home(bob)
        s.check(signed.get("callback_status") == 303 and signed_in_as(page) == "Bob Ldap"
                and trace.get("user_id") == recall("bob_user_id"),
                f"bob (brokered, first signed in before the restart): /callback {signed.get('callback_status')}, "
                f"signed in as {signed_in_as(page)!r}, same Zitadel user: {trace.get('user_id') == recall('bob_user_id')}")


def report():
    say("")
    say("e2e-dashboard: results")
    failed = False
    for key in EXPECTED:
        result = STORE["results"].get(key)
        verdict = "OK" if result and result["ok"] else ("FAILED" if result else "NOT RUN")
        failed = failed or verdict != "OK"
        say(f"  {key:<3} {verdict:<8} {TITLES[key]}")
    say("")
    for key in EXPECTED:
        result = STORE["results"].get(key)
        if not result:
            continue
        say(f"{key}: {TITLES[key]}")
        for line in result["evidence"]:
            say(f"    {line}")
    return 1 if failed else 0


def wait_dashboard(seconds=120):
    """Until the dashboard serves: it refuses everything until it has read the
    access records from the conductor once."""
    deadline = time.time() + seconds
    last = None
    while time.time() < deadline:
        try:
            last = Browser().get(dash("/healthz")).status
            if last == 200:
                return
        except OSError as failed:
            last = failed
        time.sleep(1)
    raise RuntimeError(f"the dashboard did not serve within {seconds}s: {last}")


def main():
    phase = sys.argv[1] if len(sys.argv) > 1 else "main"
    if phase == "report":
        sys.exit(report())
    wait_ready()
    wait_dashboard()
    if phase == "main":
        STORE["results"].clear()
        STORE["memory"].clear()
        save()
        for run in (scenario_a, scenario_b, scenario_c, scenario_a2, scenario_d_before, scenario_e,
                    scenario_f, scenario_h):
            run()
    elif phase == "after-removal":
        scenario_d_after()
    elif phase == "after-restart":
        scenario_g()
    else:
        sys.exit(f"unknown phase {phase}")


if __name__ == "__main__":
    main()
