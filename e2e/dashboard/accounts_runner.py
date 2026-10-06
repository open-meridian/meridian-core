"""Signing in against an account this deployment holds itself.

Decision 018, branch three: the firm has no directory at all. First run asks
for a login and a password, the Job hashes it, and the dashboard makes the
account at its next start from what was written. Nothing redirects anywhere
and nothing crosses the bus.

This is the branch that had two defects survive a year of green tests, both of
the same shape -- a value the wizard collected and nobody read. Neither was
caught because nothing signed anybody in on this route. This does.

Phases:
  main      -- the account first run made exists, and somebody uses it
  retired   -- the terminal sessions from before delegations are served no
               more, and one held from before names nobody (contract v15)
  delegate  -- the CLI connects by delegation (W6.17), as `meridian connect`
               does: registered, consented to after a fresh sign-in, a
               code traded for a pair, and a refresh nobody asked for
  restarted -- after the dashboard is recreated, the delegation stands,
               pushes and uploads a plugin and refreshes, a refresh token
               presented twice revokes it, the admin revokes one client's
               and the other's works on, and the client revokes its own
               (W6.14, W6.18)
  tickets   -- the admin reports a problem on a page, her agent files one
               through /mcp, and she works hers at its page: assigned,
               resolved citing a note, closed; and nothing of either reaches
               the platform (contract v13, W6.21 to W6.23)
  locked    -- and enough wrong passwords stop it being usable at all
"""
import base64
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# Shared with the LDAP suite, which is self-contained for the same reason this
# is: neither depends on anything that was deleted with the identity server.
from ldap_runner import (  # noqa: E402
    FAILURES,
    Browser,
    check,
    dash,
    form_token,
    home,
    is_admin_home,
    recall,
    remember,
    say,
    sign_in,
    signed_in_as,
    wait_dashboard,
)

CLAIM_CODE = os.environ["E2E_CLAIM_CODE"]
NAME = os.environ.get("E2E_LOCAL_ACCOUNT_NAME", "ada")
PASSWORD = os.environ.get("E2E_LOCAL_ACCOUNT_PASSWORD", "Password1!")
LOCK_AFTER = 5


def main_phase():
    say("A: the account first run made is there, and it is the only way in")
    ada = Browser()
    page = ada.get(dash("/sign-in"))
    check(page.status == 200 and 'name="password"' in page.body,
          f"the dashboard serves its own form: {page.status}")
    check(page.location is None, f"and sends the browser nowhere: {page.location!r}")

    refused = ada.post(dash("/sign-in"), {"name": NAME, "password": "not-the-password"})
    check(refused.status == 401, f"a wrong password: {refused.status}")
    check(NAME not in refused.body,
          "and the refusal does not name them back, so the page cannot be used to find who works here")

    missing = Browser().post(dash("/sign-in"), {"name": "nobody-at-all", "password": PASSWORD})
    check(missing.status == 401, f"a name that is not there: {missing.status}")

    say("B: and it signs in, with the password first run hashed and never stored")
    signed = sign_in(ada, NAME, PASSWORD)
    check(signed.status == 303 and signed.location == "/",
          f"{NAME} signs in: {signed.status} to {signed.location!r}")
    page = home(ada)
    check(signed_in_as(page) is not None, f"home says {signed_in_as(page)!r}")

    say("C: the claim makes them this deployment's first administrator")
    claim = ada.get(dash("/claim"))
    check(claim.status == 200, f"GET /claim {claim.status}")
    redeemed = ada.post(dash("/claim"), {"code": CLAIM_CODE, "form_token": form_token(claim)})
    check(redeemed.status == 303 and redeemed.location == "/admin",
          f"the platform's code: {redeemed.status} to {redeemed.location!r}")
    page = home(ada)
    check(is_admin_home(page), f"home shows deployment admin: {is_admin_home(page)}")
    admin = ada.get(dash("/admin"))
    check(admin.status == 200, f"GET /admin: {admin.status}")


# What `meridian connect` listens on. Nothing listens here: the code is read
# from the redirect, which is all the loopback address is for.
BACK = "http://127.0.0.1:53682/callback"
# The oldest release that must keep working across the change: the session it
# holds is the same token, presented the same way.
CLI_VERSION = "0.1.14"
PLUGIN = "e2e-restart"


def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def terminal(method, path, session, body=None, content_type=None):
    """A request as the CLI makes one: bearer, version, no cookie."""
    request = urllib.request.Request(dash(path), data=body, method=method)
    request.add_header("Authorization", f"Bearer {session}")
    request.add_header("Meridian-CLI-Version", CLI_VERSION)
    if content_type:
        request.add_header("Content-Type", content_type)
    opener = urllib.request.build_opener(NoRedirect)
    try:
        answer = opener.open(request)
        return answer.status, answer.read().decode(errors="replace"), answer.headers
    except urllib.error.HTTPError as refused:
        return refused.code, refused.read().decode(errors="replace"), refused.headers


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def hidden(page, name):
    found = re.search(rf'name="{name}" value="([^"]+)"', page.body)
    return found.group(1) if found else ""


def retired_phase():
    say("E: the terminal sessions from before delegations are gone (contract v15)")
    for method, path in [("GET", "/terminal/authorize?redirect_uri=x"),
                         ("POST", "/terminal/token"), ("POST", "/terminal/sign-out"),
                         ("POST", "/admin/end-terminal-sessions")]:
        if method == "GET":
            answer = Browser().get(dash(path))
        else:
            answer = Browser().post(dash(path), {"code": "x"})
        check(answer.status in (404, 405), f"{method} {path} is served no more: {answer.status}")
    status, body, _ = terminal("GET", "/terminal/plugins", "a-terminal-session-from-before")
    check(status == 401 and '"reason":"unknown"' in body.replace(" ", ""),
          f"and a terminal session from before names nobody: {status} {body[:200]}")


def push_and_upload(session):
    """What `meridian plugin upload` sends, through the dashboard: an image's
    blobs and manifest to the deployment's registry, then its record."""
    base = f"/terminal/registry/v2/plugins/{PLUGIN}"

    def blob(content):
        digest = "sha256:" + hashlib.sha256(content).hexdigest()
        status, body, headers = terminal("POST", f"{base}/blobs/uploads/", session, b"")
        location = headers.get("Location") or ""
        check(status == 202 and location, f"a blob upload starts: {status} {body[:200]}")
        joiner = "&" if "?" in location else "?"
        status, body, _ = terminal("PUT", f"{location}{joiner}digest={digest}", session,
                                   content, "application/octet-stream")
        check(status == 201, f"and the blob is sent: {status} {body[:200]}")
        return digest

    config = json.dumps({"architecture": "amd64", "os": "linux",
                         "rootfs": {"type": "layers", "diff_ids": []}}).encode()
    layer = b"a layer nobody runs"
    manifest = json.dumps({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": blob(config), "size": len(config)},
        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                    "digest": blob(layer), "size": len(layer)}],
    }).encode()
    status, body, headers = terminal("PUT", f"{base}/manifests/0.1.0", session, manifest,
                                     "application/vnd.oci.image.manifest.v1+json")
    digest = headers.get("Docker-Content-Digest") or ""
    check(status == 201 and digest, f"the manifest names them: {status} {body[:200]}")

    status, body, _ = terminal("POST", "/terminal/plugins", session, json.dumps({
        "name": PLUGIN, "version": "0.1.0", "roles": [], "interface": False,
        "sdk_version": "0.1.0", "image_digest": digest,
    }).encode(), "application/json")
    return status, body


def token_endpoint(fields):
    answer = Browser().post(dash("/oauth/token"), fields)
    try:
        return answer.status, json.loads(answer.body)
    except ValueError:
        return answer.status, {"body": answer.body[:200]}


def delegated(browser=None, resource="/terminal", covers=None):
    """`meridian connect`, by delegation: the token answer and the client.
    For another resource, `/mcp`, consenting to what `covers` names."""
    registered = urllib.request.Request(
        dash("/oauth/register"), method="POST",
        data=json.dumps({
            "client_name": "meridian on e2e",
            "redirect_uris": [BACK],
            "software_id": "meridian-cli",
            "token_endpoint_auth_method": "none",
        }).encode(),
        headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(registered) as answer:
            status, said = answer.status, json.loads(answer.read())
    except urllib.error.HTTPError as refused:
        status, said = refused.code, {}
    client_id = said.get("client_id", "")
    check(status == 201 and client_id, f"the CLI registers this computer: {status}")

    verifier = b64url(os.urandom(32))
    challenge = b64url(hashlib.sha256(verifier.encode()).digest())
    browser = browser or Browser()
    asked = browser.get(dash("/oauth/authorize?" + urllib.parse.urlencode({
        "response_type": "code",
        "client_id": client_id,
        "redirect_uri": BACK,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
        "state": "e2e-delegate",
        "resource": dash(resource),
    })))
    request = hidden(asked, "authorize")
    check(asked.status == 200 and request, f"the authorisation is the sign-in form: {asked.status}")
    consenting = browser.post(dash("/sign-in"),
                              {"name": NAME, "password": PASSWORD, "authorize": request})
    check(consenting.status == 200 and "Allow a client to act as you" in consenting.body
          and "meridian on e2e" in consenting.body,
          f"signed in afresh, and asked to consent, naming the client: {consenting.status}")
    decided = browser.post(dash("/oauth/authorize"), {
        "request": hidden(consenting, "request"),
        "confirm": hidden(consenting, "confirm"),
        "decision": "allow",
        "days": "90",
        **(covers or {"covers": "everything"}),
    })
    back = urllib.parse.urlparse(decided.location or "")
    code = urllib.parse.parse_qs(back.query).get("code", [""])[0]
    check(decided.status == 302 and code, f"the loopback address gets a code: {decided.status}")
    status, pair = token_endpoint({
        "grant_type": "authorization_code", "code": code, "code_verifier": verifier,
        "redirect_uri": BACK, "client_id": client_id, "resource": dash(resource),
    })
    check(status == 200 and pair.get("access_token", "").startswith("mda_")
          and pair.get("refresh_token", "").startswith("mdr_"),
          f"and the CLI trades it for an access token and a refresh token: {status} {pair}")
    return client_id, pair


def refreshed(client_id, refresh_token):
    return token_endpoint({"grant_type": "refresh_token", "refresh_token": refresh_token,
                           "client_id": client_id})


def delegate_phase():
    say("H: the CLI connects by delegation, as `meridian connect` now does")
    client_id, first = delegated()
    status, body, _ = terminal("GET", "/terminal/plugins", first.get("access_token", ""))
    check(status == 200, f"its access token lists the catalogue: {status} {body[:200]}")
    status, second = refreshed(client_id, first.get("refresh_token", ""))
    check(status == 200 and second.get("delegation_id") == first.get("delegation_id"),
          f"and a refresh brings the next pair on the same delegation: {status} {second}")
    remember("client_id", client_id)
    remember("spent_refresh", first.get("refresh_token", ""))
    remember("access", second.get("access_token", ""))
    remember("refresh", second.get("refresh_token", ""))


def delegation_restarted_phase():
    say("I: the dashboard restarted, and the delegation stands")
    client_id, access, refresh = recall("client_id"), recall("access"), recall("refresh")
    status, body, _ = terminal("GET", "/terminal/plugins", access)
    check(status == 200, f"the access token from before the restart acts after it: {status} {body[:200]}")
    status, body = push_and_upload(access)
    check(status == 201 and f'"name":"{PLUGIN}"' in body.replace(" ", ""),
          f"and a plugin is pushed and uploaded on it: {status} {body[:200]}")
    status, third = refreshed(client_id, refresh)
    check(status == 200, f"and refreshes: {status} {third}")

    say("J: a refresh token presented twice revokes the delegation")
    status, said = refreshed(client_id, recall("spent_refresh"))
    check(status == 400 and said.get("reason") == "reused",
          f"the spent refresh token is refused as reused: {status} {said}")
    status, body, _ = terminal("GET", "/terminal/plugins", third.get("access_token", ""))
    check(status == 401 and '"reason":"revoked"' in body.replace(" ", ""),
          f"and the owner's newest pair is refused as revoked: {status} {body[:200]}")

    say("K: the admin revokes one client's delegation, and the other's works on")
    admin = Browser()
    sign_in(admin, NAME, PASSWORD)
    _, one = delegated()
    other_client, other = delegated()
    page = admin.get(dash("/admin/people/local%7C" + NAME + "/delegations"))
    check(page.status == 200 and one.get("delegation_id", "?") in page.body,
          f"the admin sees the person's delegations: {page.status}")
    revoked = admin.post(dash("/admin/people/local%7C" + NAME + "/delegations/revoke"), {
        "form_token": form_token(page), "delegation_id": one.get("delegation_id", ""),
    })
    check(revoked.status == 303, f"and revokes one: {revoked.status}")
    status, _, _ = terminal("GET", "/terminal/plugins", one.get("access_token", ""))
    check(status == 401, f"that client is refused at its next request: {status}")
    status, _, _ = terminal("GET", "/terminal/plugins", other.get("access_token", ""))
    check(status == 200, f"the other works on: {status}")

    say("L: `meridian sign-out` revokes the delegation it holds")
    answer = Browser().post(dash("/oauth/revoke"), {
        "token": other.get("refresh_token", ""), "token_type_hint": "refresh_token",
        "client_id": other_client,
    })
    check(answer.status == 200, f"revoked by its client: {answer.status}")
    status, body, _ = terminal("GET", "/terminal/plugins", other.get("access_token", ""))
    check(status == 401 and "revoked" in body, f"and refused from then on: {status} {body[:200]}")


PLATFORM = os.environ.get("E2E_PLATFORM_ADDRESS", "http://fake-platform:8000")


def platform_heard():
    with urllib.request.urlopen(PLATFORM + "/e2e/requests", timeout=10) as answer:
        return json.loads(answer.read())["requests"]


def mcp_call(access, name, arguments):
    request = urllib.request.Request(
        dash("/mcp"), method="POST",
        data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": name, "arguments": arguments}}).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {access}"})
    with urllib.request.urlopen(request, timeout=30) as answer:
        return json.loads(answer.read())["result"]["structuredContent"]


def tickets_phase():
    say("M: a problem reported on a page reaches the deployment admin, and nothing leaves")
    before = len(platform_heard())
    ada = Browser()
    sign_in(ada, NAME, PASSWORD)
    form = ada.get(dash("/tickets/new?concerns=dashboard"))
    check(form.status == 200 and "Report a problem" in form.body,
          f"Report a problem, from the dashboard: {form.status}")
    filed = ada.post(dash("/tickets"), {
        "form_token": form_token(form), "concerns": "dashboard", "kind": "defect",
        "title": "The plugins list is slow", "seen": "It takes a minute to load.",
    })
    check(filed.status == 303 and (filed.location or "").startswith("/tickets/TKT-"),
          f"filed on the page: {filed.status} {filed.location!r}")
    ticket = (filed.location or "").split("?", 1)[0]
    page = ada.get(dash(ticket))
    check(page.status == 200 and "Route: Open Meridian" in page.body.replace("&#39;", "'")
          and "later release" in page.body,
          f"the rules advise Open Meridian's route, with nothing sent: {page.status}")

    say("N: her agent files through /mcp, as her through it")
    _, pair = delegated(resource="/mcp", covers={"covers": "some", "deployment_admin": "1"})
    answered = mcp_call(pair.get("access_token", ""), "dashboard__file_ticket", {
        "title": "The chart's ingress needs a note", "kind": "request",
        "concerns": {"kind": "chart"},
    })
    check(answered.get("outcome") == "made", f"filed through /mcp: {answered}")
    read = mcp_call(pair.get("access_token", ""), "dashboard__read_ticket",
                    {"ticket_id": answered.get("data", {}).get("ticket_id", "")})
    check(read.get("data", {}).get("filed_by", {}).get("provenance") == "client",
          f"recorded as hers, through the client: {read}")

    say("O: only she works it, at its page")
    for act in [{"act": "assign", "owner": "local|" + NAME},
                {"act": "resolve", "resolution": "note", "cites": "1"},
                {"act": "reopen"},
                {"act": "close", "resolution": "not_a_problem"}]:
        shown = ada.get(dash(ticket))
        notes = len(re.findall(r'<li data-number="\d+"', shown.body))
        done = ada.post(dash(ticket + "/work"),
                        {"form_token": form_token(shown), "against_notes": str(notes), **act})
        check(done.status == 303, f"{act['act']}: {done.status}")
    page = ada.get(dash(ticket))
    check('data-state="closed"' in page.body, "and it is closed")

    heard = platform_heard()[before:]
    leaked = [r for r in heard if "ticket" in (r["path"] + r["body"]).lower() or "TKT-" in r["body"]]
    check(not leaked, f"the platform heard nothing of a ticket: {leaked}")


def locked_phase():
    say("D: enough wrong passwords and the account stops being usable")
    guesser = Browser()
    statuses = []
    for _ in range(LOCK_AFTER):
        statuses.append(guesser.post(dash("/sign-in"),
                                     {"name": NAME, "password": "still-not-it"}).status)
    check(all(status == 401 for status in statuses),
          f"each wrong password is refused: {statuses}")

    # The one that matters: the right password, during the lock. A lock the
    # correct password lifts is not a lock.
    locked = Browser().post(dash("/sign-in"), {"name": NAME, "password": PASSWORD})
    check(locked.status == 429, f"the right password while locked: {locked.status}")
    check("Too many attempts" in locked.body,
          "and it says so, because somebody not told keeps trying and cannot tell this "
          "from a wrong password")


def main():
    phase = sys.argv[1] if len(sys.argv) > 1 else "main"
    wait_dashboard()
    if phase == "main":
        main_phase()
    elif phase == "retired":
        retired_phase()
    elif phase == "delegate":
        delegate_phase()
    elif phase == "restarted":
        delegation_restarted_phase()
    elif phase == "tickets":
        tickets_phase()
    elif phase == "locked":
        locked_phase()
    else:
        sys.exit(f"unknown phase {phase}")

    if FAILURES:
        say("")
        say(f"e2e-dashboard-accounts FAILED: {len(FAILURES)}")
        for failure in FAILURES:
            say(f"  - {failure}")
        sys.exit(1)


if __name__ == "__main__":
    main()
