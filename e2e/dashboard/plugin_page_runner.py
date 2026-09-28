"""A person reaches a plugin's page (W6.9, decisions/014 and 021).

The whole path in processes of their own: the dashboard, holding a key the
chart's Job would have made, gives a signed-in person a one-time code for the
plugin's own host; that host's session asks the dashboard to sign every
request; the plugin's sidecar verifies it and forwards it on loopback to a
stand-in that says what reached it.

Ada signs in with the account first run made, claims the deployment, and
grants herself read on one account through the plugin -- what an
administrator does -- and only then can she open it. The plugin says which
accounts its connection reaches, and she links one from the list the
dashboard shows beside the link action, where its sync state then says what
to do about it (W2.8, W6.4, W2.1).
"""
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ldap_runner import (  # noqa: E402
    FAILURES,
    Browser,
    check,
    dash,
    form_token,
    say,
    sign_in,
    wait_dashboard,
)

CLAIM_CODE = os.environ["E2E_CLAIM_CODE"]
NAME = os.environ.get("E2E_LOCAL_ACCOUNT_NAME", "ada")
PASSWORD = os.environ.get("E2E_LOCAL_ACCOUNT_PASSWORD", "Password1!")
INSTANCE = os.environ.get("E2E_PLUGIN_INSTANCE", "custody-test-1")
# The dashboard's own address is http://dashboard:8080, so the plugin's page
# is at this host on the same port. A browser resolves it by name; this sends
# to the dashboard and names the host, which is the same request.
PLUGIN_HOST = f"{INSTANCE}.plugins.dashboard:8080"
FRONT_DOOR = os.environ.get("E2E_FRONT_DOOR", f"http://sidecar-{INSTANCE}:9292")


def on_plugin_host(browser, path):
    """A GET on the plugin's host, with only that host's cookies."""
    request = urllib.request.Request(dash(path), headers={"Host": PLUGIN_HOST})
    if browser.cookies:
        request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in browser.cookies.items()))
    try:
        response = urllib.request.build_opener(NoRedirect).open(request)
        status, body, headers = response.status, response.read().decode(), response.headers
    except urllib.error.HTTPError as refused:
        status, body, headers = refused.code, refused.read().decode(), refused.headers
    cookies = headers.get_all("Set-Cookie") or []
    for value in cookies:
        name, _, rest = value.partition("=")
        browser.cookies[name] = rest.split(";")[0]
    return status, body, headers.get("Location"), cookies


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def post_on_plugin_host(browser, path):
    request = urllib.request.Request(dash(path), data=b"", method="POST",
                                     headers={"Host": PLUGIN_HOST})
    request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in browser.cookies.items()))
    try:
        response = urllib.request.build_opener(NoRedirect).open(request)
        return response.status, response.read().decode()
    except urllib.error.HTTPError as refused:
        return refused.code, refused.read().decode()


def unlinked_on(page):
    """The external accounts the admin page lists as waiting for a link."""
    table = page.body.split('<table class="list unlinked">', 1)
    if len(table) < 2:
        return []
    return re.findall(r'<tr data-id="([^"]+)"', table[1].split("</table>", 1)[0])


def unlinked_table(page):
    """The admin page's table of accounts waiting for a link, as text."""
    table = page.body.split('<table class="list unlinked">', 1)
    return table[1].split("</table>", 1)[0] if len(table) > 1 else ""


def sync_on(page):
    """The admin page's sync status table, as text."""
    table = page.body.split('<table class="list sync">', 1)
    return table[1].split("</table>", 1)[0] if len(table) > 1 else ""


def admin_until(ada, holds, seconds=30):
    """/admin, read again until `holds` of it or `seconds` pass: what a
    plugin says reaches the dashboard over the bus, a moment later."""
    deadline = time.monotonic() + seconds
    page = ada.get(dash("/admin"))
    while not holds(page) and time.monotonic() < deadline:
        time.sleep(1)
        page = ada.get(dash("/admin"))
    return page


def report(plugin):
    status, body = post_on_plugin_host(plugin, "/report")
    return json.loads(body) if status == 200 else {"failed": f"{status} {body[:200]}"}


def write(plugin):
    status, body = post_on_plugin_host(plugin, "/write")
    return json.loads(body) if status == 200 else {"ok": False, "detail": f"{status} {body[:200]}"}


def front_door(header=None):
    request = urllib.request.Request(FRONT_DOOR + "/")
    if header is not None:
        request.add_header("Meridian-Caller", header)
    try:
        return urllib.request.urlopen(request).status
    except urllib.error.HTTPError as refused:
        return refused.code


def row_id(page, section, name):
    """The identifier the conductor gave the row named `name` in a table."""
    table = page.body.split(f"<h2>{section}</h2>", 1)[-1].split("</table>", 1)[0]
    found = re.search(r'<tr data-id="([^"]+)" data-name="' + re.escape(name) + '"', table)
    return found.group(1) if found else None


def sentence(page):
    found = re.search(r"<p[^>]*>(.*?)</p>", page.body, re.S)
    return found.group(1) if found else page.body[:200]


def administer(ada, action, fields, patience=0):
    """Post an admin form, again for up to `patience` seconds while it is
    refused: a plugin is known to the conductor once its sidecar's report
    arrives, which is at most 30 seconds after the conductor starts."""
    deadline = time.monotonic() + patience
    while True:
        admin = ada.get(dash("/admin"))
        done = ada.post(dash(action), dict(fields, form_token=form_token(admin)))
        if done.status == 303 or time.monotonic() > deadline:
            break
        time.sleep(2)
    check(done.status == 303, f"POST {action}: {done.status} {sentence(done)}")
    return ada.get(dash("/admin"))


def main():
    wait_dashboard()

    say("A: Ada signs in and claims the deployment")
    ada = Browser()
    signed = sign_in(ada, NAME, PASSWORD)
    check(signed.status == 303, f"signs in: {signed.status}")
    claim = ada.get(dash("/claim"))
    redeemed = ada.post(dash("/claim"), {"code": CLAIM_CODE, "form_token": form_token(claim)})
    check(redeemed.status == 303, f"claims: {redeemed.status}")

    say("B: administering the deployment opens a plugin, and is not access to it")
    # spec/deployment-dashboard-and-access, ruling 19: a deployment admin
    # opens any plugin's page, and is asserted with what she holds on it --
    # before any grant, nothing. Somebody who is neither admin nor granted is
    # refused at both doors; the dashboard's own tests hold that.
    opened = ada.get(dash(f"/plugins/{INSTANCE}"))
    check(opened.status == 303, f"before any grant, /plugins/{INSTANCE}: {opened.status} {sentence(opened)}")
    before = Browser()
    status, _, _, _ = on_plugin_host(before, (opened.location or "")[len(f"http://{PLUGIN_HOST}"):])
    check(status == 303, f"the code redeemed: {status}")
    status, body, _, _ = on_plugin_host(before, "/holdings")
    seen = json.loads(body) if status == 200 else {}
    check(status == 200 and (seen.get("caller") or {}).get("access") == [],
          f"and the plugin is told she holds nothing on it: {status} {(seen.get('caller') or {}).get('access')}")

    say("C: she grants herself read on one account through the plugin")
    page = administer(ada, "/admin/accounts", {"account_id": "", "name": "Plugin page account"})
    account = row_id(page, "Accounts", "Plugin page account")
    check(account is not None, "the account is listed")
    page = administer(ada, "/admin/account-groups",
                      {"account_group_id": "", "name": "Plugin page accounts", "account_ids": account})
    account_group = row_id(page, "Account groups", "Plugin page accounts")
    page = administer(ada, "/admin/access-groups",
                      {"access_group_id": "", "name": "Plugin page readers",
                       "entries": f"{INSTANCE} custody read"}, patience=45)
    access_group = row_id(page, "Access groups", "Plugin page readers")
    # The user group the claim made her deployment admin through: the one
    # permission there is before hers.
    admins = re.search(r'<tr data-id="[^"]+" data-user-group="([^"]+)"', page.body)
    check(None not in (account_group, access_group, admins),
          f"groups listed: {account_group} {access_group} {admins and admins.group(1)}")
    administer(ada, "/admin/permissions",
               {"user_group_id": admins.group(1), "account_group_id": account_group,
                "access_group_id": access_group})

    say("D: she opens the plugin, and its host gets a session of its own")
    opened = ada.get(dash(f"/plugins/{INSTANCE}"))
    check(opened.status == 303, f"/plugins/{INSTANCE}: {opened.status} {sentence(opened)}")
    prefix = f"http://{PLUGIN_HOST}/.meridian/enter?code="
    check((opened.location or "").startswith(prefix), f"to the plugin's host: {opened.location!r}")
    enter_path = (opened.location or "")[len(f"http://{PLUGIN_HOST}"):]

    plugin = Browser()  # the plugin's host: none of the dashboard's cookies
    status, _, location, cookies = on_plugin_host(plugin, enter_path)
    check(status == 303 and location == "/", f"the code redeemed: {status} to {location!r}")
    check(any(c.startswith("meridian_plugin_session=") and "Domain" not in c for c in cookies),
          f"a host-only session: {cookies}")
    again, _, _, _ = on_plugin_host(Browser(), enter_path)
    check(again == 401, f"the same code again: {again}")

    say("E: the plugin is told who she is, by its sidecar, and nothing else")
    plugin.cookies["meridian_session"] = ada.cookies.get("meridian_session", "")
    status, body, _, cookies = on_plugin_host(plugin, "/holdings?page=2")
    check(status == 200, f"the page: {status} {body[:200]}")
    seen = json.loads(body) if status == 200 else {}
    caller = seen.get("caller") or {}
    check(seen.get("path") == "/holdings?page=2", f"the path: {seen.get('path')!r}")
    check(seen.get("callers") == 1, f"one Meridian-Caller: {seen.get('callers')}")
    check(caller.get("audience") == INSTANCE, f"for this instance: {caller.get('audience')!r}")
    check(caller.get("display_name") == "Ada Park", f"naming her: {caller.get('display_name')!r}")
    check(caller.get("lifetime_ns") == 60 * 1_000_000_000, f"for 60 seconds: {caller.get('lifetime_ns')}")
    check(caller.get("access") == [{"tag": "custody", "read": [account], "write": []}],
          f"holding what she was granted: {caller.get('access')}")
    check(seen.get("cookie") is None, f"no cookie reached the plugin: {seen.get('cookie')!r}")
    check(not cookies, f"and the plugin set none: {cookies}")

    say("F: the sidecar admits only what the dashboard signed, once")
    check(front_door() == 401, "with no assertion, refused")
    check(front_door(seen.get("raw") or "x") == 403, "an assertion already used, refused")

    say("G: the accounts the plugin reaches wait beside the link action (W2.8, W6.4)")
    said = report(plugin)
    check(said.get("accounts") == "published", f"the accounts are published: {said}")
    check(said.get("sync") == "published",
          f"and its sync state, though nobody linked it, since it describes the connection: {said}")
    page = admin_until(ada, lambda page: "ext-e2e-roth" in unlinked_on(page)
                       and "Needs sign-in" in unlinked_table(page))
    waiting = unlinked_on(page)
    check(waiting[:2] == ["ext-e2e", "ext-e2e-roth"], f"both wait, in the plugin's order: {waiting}")
    check("Roth IRA" in page.body and "E2E Brokerage" in page.body,
          "with the custodian's name and the venue's own type")
    check('name="external_account_id" value="ext-e2e"' in page.body,
          "each with a link of its own")
    check("Sign in again at the venue" in unlinked_table(page),
          "and the connection's state beside the unlinked account, with what to do")
    administer(ada, "/admin/links", {"plugin_instance_id": INSTANCE,
                                     "external_account_id": "ext-e2e", "account_id": account})
    waiting = unlinked_on(ada.get(dash("/admin")))
    check("ext-e2e" not in waiting and "ext-e2e-roth" in waiting,
          f"linked, it stops waiting, and the other does not: {waiting}")

    say("H: linked, its sync state is shown against its account (W2.1)")
    # The sidecar reads the plugin's links again within 30 seconds of a
    # change, so the state is reported again until it arrives with the account.
    deadline = time.monotonic() + 45
    while True:
        said = report(plugin)
        time.sleep(1)
        shown = sync_on(ada.get(dash("/admin")))
        row = shown.split('data-id="ext-e2e"', 1)[-1].split("</tr>", 1)[0]
        if ("Needs sign-in" in row and "not linked" not in row) or time.monotonic() > deadline:
            break
        time.sleep(2)
    check(said.get("sync") == "published", f"the sync state is published: {said}")
    check("not linked" not in row, f"now against the account it is linked to: {row[:300]}")
    check("Needs sign-in" in shown, f"the state: {shown[:300]}")
    check("Sign in again at the venue" in shown, f"and what to do: {shown[:300]}")
    check("the daily sign-in has lapsed" in shown, "with the connector's own words")

    say("I: the plugin writes for her only what she may write (W4.9)")
    refused = write(plugin)
    check(not refused.get("ok") and refused.get("code") == "PERMISSION_DENIED",
          f"while she only reads, the sidecar refuses: {refused}")
    administer(ada, "/admin/access-groups",
               {"access_group_id": access_group, "name": "Plugin page readers",
                "entries": f"{INSTANCE} custody write"})
    # The sidecar reads the plugin's write scope again within 30 seconds.
    deadline = time.monotonic() + 45
    written = write(plugin)
    while not written.get("ok") and time.monotonic() < deadline:
        time.sleep(3)
        written = write(plugin)
    check(written.get("ok"), f"once she writes, it is recorded: {written}")

    say("J: signing out of the dashboard ends the plugin's session")
    home = ada.get(dash("/"))
    ada.post(dash("/sign-out"), {"form_token": form_token(home)})
    status, _, location, _ = on_plugin_host(plugin, "/")
    check(status == 303 and (location or "").endswith(f"/plugins/{INSTANCE}"),
          f"after sign-out: {status} to {location!r}")

    if FAILURES:
        say("")
        say(f"e2e-plugin-page FAILED: {len(FAILURES)}")
        for failure in FAILURES:
            say(f"  - {failure}")
        sys.exit(1)


if __name__ == "__main__":
    main()
