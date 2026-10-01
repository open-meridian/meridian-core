"""A person reaches a plugin's page at a level she holds (W6.9, decisions/014,
021 and 027; sdk-contract/a-plugin-has-admins).

The whole path in processes of their own: the dashboard, holding a key the
chart's Job would have made, gives a signed-in person a one-time code for the
plugin's own host at the level she chose -- Manage for `admin`, Open for
`write`, View for `read` -- that host's session asks the dashboard to sign
every request at that level alone, cut to it; the plugin's sidecar verifies
it and forwards it on loopback to a stand-in that says what reached it.

Ada signs in with the account first run made and claims the deployment,
which links her to All plugins (admin) too: she finds the plugin on the home
with Manage alone, and a Manage session carries no account. She grants
herself read on one account through the plugin, and finds View beside it,
whose session carries that account and no write set. The plugin says which
accounts its connection reaches, and the dashboard counts those nothing links
on the plugin's health. She links them on the plugin's admin page, under
Manage, which reads the deployment's accounts and sends each link acting for
her: to her account, to a new one it creates, since she is a deployment
admin, and one removed again; the sidecar refuses the plugin as itself, an
account it did not report, and the same read under View, and the conductor a
link naming both. The plugin's overview then shows the sync state against the
account it is linked to, with what to do (W2.8, W6.4, W2.1).

The stand-in is a plugin built before contract v5, declaring two admin pages
in the list v5 retired: the sidecar reads them as pages at `admin`, which the
plugin area opens under Manage, and the plugin serves them there alone.

Granted write, she finds Manage, Open and View; a command sent for her is
admitted under Open alone. The plugin declares a required secret, which she
sets on its Settings tab as its admin, and the report turns healthy with the
plugin never restarted (W6.11, W4.7, W4.8). Bea, in a user group granted the
plugin's `admin` alone, finds Manage alone, sets its settings, links an
external account to an existing account and is refused naming a new one, and
sees no account data; granted read on All accounts, the built-in group, her
View reaches an account no other group lists. Ada's link to All plugins
(admin) withdrawn, she configures the plugin no more. An access group naming
the plugin at read and write is refused.

The secret is looked for everywhere it must not be: every page fetched in the
run, and every report on the bus, which this watches as a subscriber of the
two report topics and nothing else. The target greps the components' logs and
the configuration store for it afterwards, and the sidecar's log for the level
each act was sent under.
"""
import hashlib
import json
import os
import re
import socket
import sys
import threading
import time
import urllib.error
import urllib.parse
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
# A second person, the target's to seed with the same password: granted the
# plugin's admin alone, and not a deployment admin.
BEA = os.environ.get("E2E_SECOND_ACCOUNT_NAME", "bea")
INSTANCE = os.environ.get("E2E_PLUGIN_INSTANCE", "custody-test-1")
# The dashboard's own address is http://dashboard:8080, so the plugin's page
# is at this host on the same port. A browser resolves it by name; this sends
# to the dashboard and names the host, which is the same request.
PLUGIN_HOST = f"{INSTANCE}.plugins.dashboard:8080"
FRONT_DOOR = os.environ.get("E2E_FRONT_DOOR", f"http://sidecar-{INSTANCE}:9292")
# Obviously fake, and the same value the target greps the logs for.
SECRET = os.environ.get("E2E_SETTING_SECRET", "sk-test-not-a-real-key-e2e")
BROKER = os.environ.get("E2E_BROKER", "nats://tests:tests-dev-only@nats:4222")
REPORT_TOPICS = ("platform.deployment.event.plugin-report",
                 "platform.deployment.event.component-report")
# Every page body the run fetched, to look for the secret in.
PAGES = []


def kept(send):
    def sending(self, method, url, fields=None):
        reply = send(self, method, url, fields)
        PAGES.append(reply.body)
        return reply
    return sending


Browser.send = kept(Browser.send)


def fields_of(message):
    """A protobuf message's fields by number: an int for a varint, bytes for
    anything length-delimited. Enough to read an envelope and a report."""
    fields, at = {}, 0
    while at < len(message):
        key, at = varint(message, at)
        number, wire = key >> 3, key & 7
        if wire == 0:
            value, at = varint(message, at)
        elif wire == 2:
            length, at = varint(message, at)
            value, at = message[at:at + length], at + length
        elif wire == 1:
            value, at = message[at:at + 8], at + 8
        elif wire == 5:
            value, at = message[at:at + 4], at + 4
        else:
            raise ValueError(f"wire type {wire}")
        fields.setdefault(number, []).append(value)
    return fields


def varint(data, at):
    shift = value = 0
    while True:
        byte = data[at]
        at += 1
        value |= (byte & 0x7F) << shift
        shift += 7
        if not byte & 0x80:
            return value, at


def levels_of(values):
    """A repeated enum's values, packed or not."""
    levels = []
    for value in values:
        if isinstance(value, int):
            levels.append(value)
            continue
        at = 0
        while at < len(value):
            level, at = varint(value, at)
            levels.append(level)
    return levels


class Reports:
    """Every report on the bus from now on, as a subscriber of the report
    topics alone -- never of a plugin's configuration, which carries its
    secret to its sidecar by design."""

    def __init__(self):
        self.raw = []
        self.plugins = []
        self.lock = threading.Lock()
        url = urllib.parse.urlparse(BROKER)
        self.sock = socket.create_connection((url.hostname, url.port or 4222), timeout=10)
        self.file = self.sock.makefile("rb")
        self.file.readline()  # INFO
        connect = {"verbose": False, "pedantic": False, "user": url.username,
                   "pass": url.password, "name": "e2e-reports", "lang": "python",
                   "version": "0", "protocol": 1}
        lines = [f"CONNECT {json.dumps(connect)}"]
        lines += [f"SUB {topic} {sid}" for sid, topic in enumerate(REPORT_TOPICS, 1)]
        self.sock.sendall(("\r\n".join(lines + ["PING"]) + "\r\n").encode())
        self.sock.settimeout(None)
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        while True:
            line = self.file.readline()
            if not line:
                return
            if line.startswith(b"PING"):
                self.sock.sendall(b"PONG\r\n")
            elif line.startswith(b"MSG "):
                parts = line.split()
                payload = self.file.read(int(parts[-1]))
                self.file.readline()
                self.keep(parts[1].decode(), payload)
            elif line.startswith(b"-ERR"):
                print(f"the broker said {line!r}", flush=True)

    def keep(self, topic, envelope):
        with self.lock:
            self.raw.append(envelope)
        if topic != REPORT_TOPICS[0]:
            return
        report = fields_of(fields_of(envelope).get(3, [b""])[0])
        said = {
            "instance": report.get(1, [b""])[0].decode(),
            "registered": bool(report.get(4, [0])[0]),
            "healthy": bool(report.get(5, [0])[0]),
            "detail": report.get(6, [b""])[0].decode(),
            # Field 13: what it declared, by name (W4.8).
            "declared": [fields_of(d).get(1, [b""])[0].decode() for d in report.get(13, [])],
            # Field 14, its interface: each page (field 4), its path and levels.
            "pages": [
                (fields_of(page).get(1, [b""])[0].decode(), levels_of(fields_of(page).get(3, [])))
                for interface in report.get(14, [])
                for page in fields_of(interface).get(4, [])
            ],
        }
        with self.lock:
            self.plugins.append(said)

    def mark(self):
        with self.lock:
            return len(self.plugins)

    def until(self, holds, since=0, seconds=60):
        """The first report of the plugin, from the `since`th heard, for which
        `holds`."""
        deadline = time.monotonic() + seconds
        seen = since
        while time.monotonic() < deadline:
            with self.lock:
                fresh = self.plugins[seen:]
                seen = len(self.plugins)
            for said in fresh:
                if said["instance"] == INSTANCE and holds(said):
                    return said
            time.sleep(0.5)
        with self.lock:
            last = [s for s in self.plugins if s["instance"] == INSTANCE][-1:]
        return {"timed_out": True, "last": last}

    def any_carry(self, text):
        with self.lock:
            return sum(1 for raw in self.raw if text.encode() in raw), len(self.raw)


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
    PAGES.append(body)
    return status, body, headers.get("Location"), cookies


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


def post_on_plugin_host(browser, path, sent=None):
    data = json.dumps(sent).encode() if sent is not None else b""
    request = urllib.request.Request(dash(path), data=data, method="POST",
                                     headers={"Host": PLUGIN_HOST})
    request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in browser.cookies.items()))
    try:
        response = urllib.request.build_opener(NoRedirect).open(request)
        return response.status, response.read().decode()
    except urllib.error.HTTPError as refused:
        return refused.code, refused.read().decode()


VIEW = f"/admin/plugins/{INSTANCE}"


def unlinked_said(page):
    """What the plugin's line on the admin portal says of its unlinked
    external accounts, if anything."""
    plugins = page.body.split('<table class="list plugins">', 1)[-1].split("</table>", 1)[0]
    found = re.search(r'data-flag="unlinked"[^>]*>(.*?)</p>', plugins, re.S)
    return found.group(1) if found else ""


def sync_on(page):
    """The plugin's overview's sync status table, as text."""
    table = page.body.split('<table class="list sync">', 1)
    return table[1].split("</table>", 1)[0] if len(table) > 1 else ""


def admin_until(ada, holds, seconds=30, path="/admin"):
    """A dashboard page, read again until `holds` of it or `seconds` pass:
    what a plugin says reaches the dashboard over the bus, a moment later."""
    deadline = time.monotonic() + seconds
    page = ada.get(dash(path))
    while not holds(page) and time.monotonic() < deadline:
        time.sleep(1)
        page = ada.get(dash(path))
    return page


def tabs_on(page):
    """The admin view's tabs, as (href, name), in order."""
    nav = page.body.split('<nav class="tabs view-tabs"', 1)[-1].split("</nav>", 1)[0]
    return [(href.replace("&amp;", "&"), name)
            for href, name in re.findall(r'<a href="([^"]+)"[^>]*>([^<]+)</a>', nav)]


def link(plugin, **asked):
    status, body = post_on_plugin_host(plugin, "/link", asked)
    return json.loads(body) if status == 200 else {"ok": False, "detail": f"{status} {body[:200]}"}


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


def account_row(page, account_id):
    """The Accounts tab's row for one account, as HTML."""
    table = page.body.split("<h2>Accounts</h2>", 1)[-1].split("</table>", 1)[0]
    return table.split(f'<tr data-id="{account_id}"', 1)[-1].split("</tr>", 1)[0]


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


def settings_form(ada, patience=45):
    """The plugin's admin view on its Settings tab, once the conductor has
    its declarations and the dashboard has read them again, which is within
    30 seconds."""
    deadline = time.monotonic() + patience
    while True:
        page = ada.get(dash(f"{VIEW}?tab=settings"))
        if (page.status == 200 and 'data-setting="api_key"' in page.body) \
                or time.monotonic() > deadline:
            return page
        time.sleep(2)


def plugin_settings(plugin):
    status, body, _, _ = on_plugin_host(plugin, "/settings")
    return json.loads(body) if status == 200 else {"failed": f"{status} {body[:200]}"}


def settings_reach_the_running_plugin(ada, plugin, reports):
    """W6.11, W4.7, W4.8: a required secret, set in the dashboard, reaches a
    plugin that reported unhealthy for want of it, without a restart."""
    missing = reports.until(lambda r: r["registered"] and not r["healthy"])
    check(missing.get("detail") == "required setting api_key is not set",
          f"before it is set, the plugin is reported unhealthy, naming it: {missing}")
    check(missing.get("declared") == ["api_key", "poll_minutes"],
          f"and the report carries what it declared: {missing.get('declared')}")
    before = plugin_settings(plugin)
    check(before.get("missing_required") == ["api_key"],
          f"the plugin is told it is missing: {before}")

    form = settings_form(ada)
    check(form.status == 200, f"the settings page: {form.status} {sentence(form)}")
    field = form.body.split('data-setting="api_key"', 1)[-1].split("</div>", 1)[0]
    check('type="password"' in field and "not set" in field and "Required" in field,
          f"the secret is a password field, not set, and says it is required: {field[:300]}")
    old = ada.get(dash(f"/admin/plugins/{INSTANCE}/settings"))
    check(old.status == 303 and (old.location or "").endswith(f"{VIEW}?tab=settings"),
          f"the form's old address is the view's Settings tab: {old.status} {old.location!r}")
    # Only a report heard after this counts: one sent while the conductor
    # was still starting says healthy, having nothing to say otherwise.
    mark = reports.mark()
    done = ada.post(dash(f"/admin/plugins/{INSTANCE}/settings"),
                    {"form_token": form_token(form), "secret.api_key": SECRET,
                     "value.poll_minutes": "15"})
    check(done.status == 303, f"saved: {done.status} {sentence(done)}")
    after = ada.get(dash(f"{VIEW}?tab=settings&saved=1"))
    field = after.body.split('data-setting="api_key"', 1)[-1].split("</div>", 1)[0]
    check(">set<" in field and 'value=""' in field,
          f"and then it is set, and its field is still empty: {field[:300]}")
    check('name="value.poll_minutes" value="15"' in after.body, "the number is shown as it stands")

    healthy = reports.until(lambda r: r["registered"] and r["healthy"], since=mark)
    check(healthy.get("healthy") is True,
          f"the report turns healthy without a restart: {healthy}")
    deadline = time.monotonic() + 45
    held = plugin_settings(plugin)
    while held.get("missing_required") != [] and time.monotonic() < deadline:
        time.sleep(1)
        held = plugin_settings(plugin)
    check(held.get("missing_required") == [], f"the plugin is missing nothing: {held}")
    check(held.get("api_key_sha256") == hashlib.sha256(SECRET.encode()).hexdigest(),
          "the plugin holds the secret that was set")
    check(held.get("values", {}).get("poll_minutes") == "15", f"and the number: {held}")
    check(held.get("started_at_ns") == before.get("started_at_ns")
          and held.get("registrations") == 1,
          f"from the same process, registered once: {before} then {held}")

    # W6.10: the view says so, from what the sidecar reports.
    deadline = time.monotonic() + 45
    view = ada.get(dash(f"/admin/plugins/{INSTANCE}"))
    while "Healthy" not in view.body.split('id="health"', 1)[-1][:400] and time.monotonic() < deadline:
        time.sleep(2)
        view = ada.get(dash(f"/admin/plugins/{INSTANCE}"))
    health = view.body.split('id="health"', 1)[-1].split("</section>", 1)[0]
    check('<span class="badge good">Healthy</span>' in health, f"the view says it is healthy: {health[:400]}")
    head = view.body.split("</header>", 1)[0]
    check("Ada Park" in head and 'href="/admin">Settings<' in head and "/sign-out" in head,
          "under the one header: the person, the way back to Settings and signing out")
    # W6.9: the tabs every plugin has, and a link to its area, where its own
    # pages are; it frames none of them.
    tabs = tabs_on(view)
    check([name for _, name in tabs] == ["Overview", "Settings", "Access"],
          f"the view keeps the tabs every plugin has: {tabs}")
    check(f'href="/plugins/{INSTANCE}?level=admin" data-area' in view.body and "<iframe" not in view.body,
          "and links to the plugin's area under Manage, framing nothing")


def buttons(browser, instance=INSTANCE, seconds=45):
    """The levels the home offers on a plugin, as its buttons name them, once
    it lists the plugin: a plugin reached through All plugins (admin) is
    listed once its sidecar has reported."""
    deadline = time.monotonic() + seconds
    while True:
        page = browser.get(dash("/"))
        card = page.body.split(f'<li data-instance="{instance}">', 1)
        if len(card) > 1 or time.monotonic() > deadline:
            break
        time.sleep(2)
    if len(card) < 2:
        return []
    card = card[1].split("</li>", 1)[0]
    return re.findall(r'<a class="plugin-level" data-level="[a-z]+" href="[^"]+">([A-Za-z]+)</a>', card)


def session_at(browser, level):
    """A plugin-host session at `level`, entered as the frame enters it: a
    Browser holding it, or the refusal's status and sentence."""
    opened = browser.get(dash(f"/plugins/{INSTANCE}/enter?level={level}"))
    if opened.status != 303:
        return None, f"{opened.status} {sentence(opened)}"
    host = Browser()
    status, _, _, _ = on_plugin_host(host, (opened.location or "")[len(f"http://{PLUGIN_HOST}"):])
    return (host, "") if status == 303 else (None, f"redeemed: {status}")


def claims_on(host, path="/holdings"):
    status, body, _, _ = on_plugin_host(host, path)
    seen = json.loads(body) if status == 200 else {}
    return status, seen.get("caller") or {}, seen


def permission_of(page, access_group_id):
    """The permission a user group holds to an access group, by its row."""
    found = re.search(r'<tr data-id="([^"]+)" data-user-group="[^"]+" data-account-group="[^"]*" '
                      r'data-access-group="' + re.escape(access_group_id) + '"', page.body)
    return found.group(1) if found else None


def main():
    wait_dashboard()
    reports = Reports()

    say("A: Ada signs in and claims the deployment")
    ada = Browser()
    signed = sign_in(ada, NAME, PASSWORD)
    check(signed.status == 303, f"signs in: {signed.status}")
    claim = ada.get(dash("/claim"))
    redeemed = ada.post(dash("/claim"), {"code": CLAIM_CODE, "form_token": form_token(claim)})
    check(redeemed.status == 303, f"claims: {redeemed.status}")

    say("B: a deployment admin is admin on the plugin through All plugins (admin), and reaches no account")
    # The product owner, 2026-09-30, superseding ruling 19: the claim links
    # her to All plugins (admin) as first run would; she holds no data grant.
    offered = buttons(ada)
    check(offered == ["Manage"], f"the home offers Manage alone: {offered}")
    manager, why = session_at(ada, "admin")
    check(manager is not None, f"a Manage session: {why}")
    status, caller, _ = claims_on(manager) if manager else (0, {}, {})
    check(status == 200 and caller.get("level") == 3 and caller.get("access") == {"read": [], "write": []},
          f"the plugin is told the session is at admin, with no account: {status} {caller}")
    check(caller.get("deployment_admin") is True,
          f"and that she administers the deployment, who may name a new account: {caller}")
    view, why = session_at(ada, "read")
    check(view is None and "403" in why and "You do not hold read" in why,
          f"no View without a data grant: {why}")

    say("C: she grants herself read on one account through the plugin")
    page = administer(ada, "/admin/accounts",
                      {"account_id": "", "name": "Plugin page account", "custodian": "Fidelity",
                       "account_type": "Roth IRA", "owner": "Fund I", "note": "Made by the e2e."})
    account = row_id(page, "Accounts", "Plugin page account")
    check(account is not None, "the account is listed")
    row = account_row(page, account)
    noted = re.search(r'<span class="hint note" id="(account-note-\d+)">Made by the e2e\.</span>', row)
    check(all(f"<td>{said}</td>" in row for said in ("Fidelity", "Roth IRA", "Fund I"))
          and noted is not None,
          f"with its custodian, type, owner and note (W6.3): {row[:400]}")
    check(noted is not None and f'class="note-mark" aria-label="Note" aria-describedby="{noted[1]}"' in row
          and 'id="accounts-note-bubble"' in page.body,
          f"and its note in the one bubble, reached by its marker, which it describes: {row[:400]}")
    check('data-filter="accounts-table"' in page.body, "and the tab offers a search")
    page = administer(ada, "/admin/account-groups",
                      {"account_group_id": "", "name": "Plugin page accounts", "account_ids": account})
    account_group = row_id(page, "Account", "Plugin page accounts")
    page = administer(ada, "/admin/access-groups",
                      {"access_group_id": "", "name": "Plugin page readers",
                       "entries": f"{INSTANCE} read"}, patience=45)
    access_group = row_id(page, "Access", "Plugin page readers")
    # The user group the claim made her deployment admin through.
    admins = re.search(r'<tr data-id="[^"]+" data-user-group="([^"]+)"', page.body)
    check(None not in (account_group, access_group, admins),
          f"groups listed: {account_group} {access_group} {admins and admins.group(1)}")
    administer(ada, "/admin/permissions",
               {"user_group_id": admins.group(1), "account_group_id": account_group,
                "access_group_id": access_group})
    # W6.7: write already includes read.
    both = ada.post(dash("/admin/access-groups"),
                    {"access_group_id": "", "name": "Both", "entries": f"{INSTANCE} read\n{INSTANCE} write",
                     "form_token": form_token(page)})
    check(both.status == 400 and "write includes read" in both.body,
          f"an access group naming the plugin at read and write is refused: {both.status} {sentence(both)}")

    say("D: she finds Manage and View, and each opens the plugin's area at its level")
    offered = buttons(ada)
    check(offered == ["Manage", "View"], f"the home offers Manage and View: {offered}")
    # This dashboard is at http://dashboard:8080, a host with no domain, so a
    # browser would keep no framed page's session (plugins.frames): the area
    # sends her to its first page's way in, at the level, in a window of its
    # own. The area itself is held by the dashboard's tests, on a name with one.
    frame = ada.get(dash(f"/plugins/{INSTANCE}?level=read"))
    check(frame.status == 303
          and (frame.location or "").startswith(f"/plugins/{INSTANCE}/enter?path=%2F&level=read&om-scheme=default"),
          f"View: its / , declaring no page at read (W4.8): {frame.status} {frame.location!r}")
    # Manage opens on the dashboard's own Summary tab, drawn by the
    # dashboard and so in this window, whatever the frames: the plugin's
    # status, with its settings on the Settings tab after it.
    frame = ada.get(dash(f"/plugins/{INSTANCE}?level=admin"))
    check(frame.status == 200 and 'id="status"' in frame.body
          and 'id="settings"' not in frame.body and "<iframe" not in frame.body
          and 'data-tab="summary" data-drawn class="here"' in frame.body,
          f"Manage: the dashboard's Summary tab first: {frame.status} {sentence(frame)}")
    frame = ada.get(dash(f"/plugins/{INSTANCE}?level=admin&tab=settings"))
    check(frame.status == 200 and 'id="settings"' in frame.body
          and 'id="status"' not in frame.body and "<iframe" not in frame.body,
          f"Manage: the dashboard's Settings tab, the form alone: {frame.status} {sentence(frame)}")
    frame = ada.get(dash(f"/plugins/{INSTANCE}?level=admin&tab=connections"))
    check(frame.status == 303
          and (frame.location or "").startswith(
              f"/plugins/{INSTANCE}/enter?path=%2Fadmin%2Fconnections&level=admin&om-scheme=default"),
          f"Manage: its first admin page, the older plugin's admin pages read at admin: "
          f"{frame.status} {frame.location!r}")
    declared = reports.until(lambda r: r["registered"] and r.get("pages"))
    check([(path, levels) for path, levels in declared.get("pages", [])]
          == [("/admin/connections", [3]), ("/admin/accounts", [3])],
          f"its report carries them as pages at admin, in order (W4.8): {declared.get('pages')}")
    opened = ada.get(dash(f"/plugins/{INSTANCE}/enter?level=read"))
    check(opened.status == 303, f"/plugins/{INSTANCE}/enter?level=read: {opened.status} {sentence(opened)}")
    prefix = f"http://{PLUGIN_HOST}/.meridian/enter?code="
    check((opened.location or "").startswith(prefix), f"to the plugin's host: {opened.location!r}")
    enter_path = (opened.location or "")[len(f"http://{PLUGIN_HOST}"):]

    viewer = Browser()  # the plugin's host: none of the dashboard's cookies
    status, _, location, cookies = on_plugin_host(viewer, enter_path)
    check(status == 303 and location == "/", f"the code redeemed: {status} to {location!r}")
    check(any(c.startswith("meridian_plugin_session=") and "Domain" not in c for c in cookies),
          f"a host-only session: {cookies}")
    again, _, _, _ = on_plugin_host(Browser(), enter_path)
    check(again == 401, f"the same code again: {again}")

    kit = Browser()  # nobody: the kit is the same files for everybody
    status, body, _, _ = on_plugin_host(kit, "/.meridian/ui/0.3.0/meridian.css")
    check(status == 200 and "--space-4" in body,
          f"the UI kit is served on the plugin's own host: {status} {body[:120]!r}")
    # Any 0.x is the newest 0.x the image carries: a page that pinned the
    # first kit still gets one. Another major is none.
    status, pinned, _, _ = on_plugin_host(kit, "/.meridian/ui/0.1.0/meridian.css")
    check(status == 200 and pinned == body,
          f"a page that pinned 0.1.0 gets the same kit: {status} {pinned[:120]!r}")
    status, _, _, _ = on_plugin_host(kit, "/.meridian/ui/1.0.0/meridian.css")
    check(status == 404, f"and another major none: {status}")

    say("E: the plugin is told who she is and the level chosen, by its sidecar, and nothing else")
    viewer.cookies["meridian_session"] = ada.cookies.get("meridian_session", "")
    status, body, _, cookies = on_plugin_host(viewer, "/holdings?page=2")
    check(status == 200, f"the page: {status} {body[:200]}")
    seen = json.loads(body) if status == 200 else {}
    caller = seen.get("caller") or {}
    check(seen.get("path") == "/holdings?page=2", f"the path: {seen.get('path')!r}")
    check(seen.get("callers") == 1, f"one Meridian-Caller: {seen.get('callers')}")
    check(caller.get("audience") == INSTANCE, f"for this instance: {caller.get('audience')!r}")
    check(caller.get("display_name") == "Ada Park", f"naming her: {caller.get('display_name')!r}")
    check(caller.get("lifetime_ns") == 60 * 1_000_000_000, f"for 60 seconds: {caller.get('lifetime_ns')}")
    check(caller.get("level") == 1, f"at read, the level chosen: {caller}")
    check(caller.get("access") == {"read": [account], "write": []},
          f"holding what she was granted, cut to it: {caller.get('access')}")
    check(seen.get("cookie") is None, f"no cookie reached the plugin: {seen.get('cookie')!r}")
    check(not cookies, f"and the plugin set none: {cookies}")
    status, _, _, _ = on_plugin_host(viewer, "/admin/accounts")
    check(status == 403, f"its admin page is refused under View, where it is declared: {status}")

    say("F: the sidecar admits only what the dashboard signed, once")
    check(front_door() == 401, "with no assertion, refused")
    check(front_door(seen.get("raw") or "x") == 403, "an assertion already used, refused")

    say("G: the plugin links the accounts it reaches, acting for her under Manage (W2.8, W6.4)")
    said = report(manager)
    check(said.get("accounts") == "published", f"the accounts are published: {said}")
    check(said.get("sync") == "published",
          f"and its sync state, though nobody linked it, since it describes the connection: {said}")
    page = admin_until(ada, lambda page: "2 external accounts not linked" in unlinked_said(page))
    check(f'<a href="{VIEW}">2 external accounts not linked</a>' in unlinked_said(page),
          f"the dashboard counts both on the plugin's line, leading to its view: {unlinked_said(page)!r}")
    check("External accounts" not in page.body and "/admin/links" not in page.body
          and 'name="external_account_id"' not in page.body,
          "and lists and links none itself")
    status, body, _, _ = on_plugin_host(manager, "/admin/accounts")
    check(status == 200 and "Account links" in body, f"its admin page, under Manage: {status} {body[:200]}")
    status, body, _, _ = on_plugin_host(manager, "/accounts")
    read = json.loads(body) if status == 200 else {}
    check(read.get("ok") and any(a["account_id"] == account for a in read.get("accounts", [])),
          f"the plugin reads the deployment's accounts for her: {status} {body[:300]}")
    mine = next((a for a in read.get("accounts", []) if a["account_id"] == account), {})
    check((mine.get("custodian"), mine.get("account_type"), mine.get("owner"), mine.get("note"))
          == ("Fidelity", "Roth IRA", "Fund I", "Made by the e2e."),
          f"each with its custodian, type, owner and note, to tell them apart: {mine}")
    status, body, _, _ = on_plugin_host(viewer, "/accounts")
    refused = json.loads(body) if status == 200 else {}
    check(refused.get("code") == "PERMISSION_DENIED" and "View (read)" in refused.get("detail", ""),
          f"the same read under View is refused: {refused}")
    itself = link(manager, external_account_id="ext-e2e", account_id=account, as_itself=True)
    check(itself.get("code") == "PERMISSION_DENIED" and "admin of the plugin" in itself.get("detail", ""),
          f"as itself, the plugin is refused: {itself}")
    unreported = link(manager, external_account_id="ext-nobody-reported", account_id=account)
    check(unreported.get("code") == "PERMISSION_DENIED" and "reported" in unreported.get("detail", ""),
          f"an account it did not report is refused at the sidecar: {unreported}")
    both = link(manager, external_account_id="ext-e2e", account_id=account,
                new_account_name="Two at once")
    check(both.get("code") == "ABORTED" and "not both" in both.get("detail", ""),
          f"naming both an account and a new one is refused by the conductor: {both}")
    linked = link(manager, external_account_id="ext-e2e", account_id=account)
    check(linked.get("ok") and linked.get("account_id") == account
          and linked.get("plugin_instance_id") == INSTANCE,
          f"linked to her account, for this plugin: {linked}")
    created = link(manager, external_account_id="ext-e2e-roth", new_account_name="E2E Roth",
                   new_account_custodian="E2E Brokerage", new_account_type="Roth IRA")
    check(created.get("ok") and created.get("account_id") not in (None, "", account),
          f"linked to a new account, made in the same step, by a deployment admin: {created}")
    # The dashboard reads the records again within 30 seconds; the link was
    # the plugin's, so nothing here asked it to read them sooner.
    page = admin_until(ada, lambda page: row_id(page, "Accounts", "E2E Roth") is not None
                       and "not linked" not in unlinked_said(page), seconds=45)
    check(row_id(page, "Accounts", "E2E Roth") == created.get("account_id"),
          "the new account is the deployment's, under the name given")
    row = account_row(page, created.get("account_id") or "")
    check("<td>E2E Brokerage</td><td>Roth IRA</td>" in row,
          f"with the custodian and type the plugin sent (W6.4): {row[:400]}")
    check("not linked" not in unlinked_said(page), f"none waits: {unlinked_said(page)!r}")
    removed = link(manager, external_account_id="ext-e2e-roth")
    check(removed.get("ok") and removed.get("account_id") == "", f"and unlinked again: {removed}")
    page = admin_until(ada, lambda page: "1 external account not linked" in unlinked_said(page),
                       seconds=45)
    check("1 external account not linked" in unlinked_said(page),
          f"so one waits again, and its account stays: {unlinked_said(page)!r}")
    check(row_id(page, "Accounts", "E2E Roth") is not None, "records outlive a link")

    say("H: linked, its sync state is shown against its account on its overview (W2.1)")
    # The sidecar reads the plugin's links again within 30 seconds of a
    # change, so the state is reported again until it arrives with the account.
    deadline = time.monotonic() + 45
    while True:
        said = report(manager)
        time.sleep(1)
        shown = sync_on(ada.get(dash(VIEW)))
        row = shown.split('data-id="ext-e2e"', 1)[-1].split("</tr>", 1)[0]
        if ("Needs sign-in" in row and "not linked" not in row) or time.monotonic() > deadline:
            break
        time.sleep(2)
    check(said.get("sync") == "published", f"the sync state is published: {said}")
    check("not linked" not in row, f"now against the account it is linked to: {row[:300]}")
    check("Needs sign-in" in shown, f"the state: {shown[:300]}")
    check("Sign in again at the venue" in shown, f"and what to do: {shown[:300]}")
    check("the daily sign-in has lapsed" in shown, "with the connector's own words")

    say("I: a command is sent for her only under Open, on what she may write (W4.9, W6.9)")
    refused = write(viewer)
    check(not refused.get("ok") and refused.get("code") == "PERMISSION_DENIED"
          and "View (read)" in refused.get("detail", ""),
          f"under View, the sidecar refuses: {refused}")
    refused = write(manager)
    check(not refused.get("ok") and "Manage (admin)" in refused.get("detail", ""),
          f"under Manage, too: admin reaches no account's data: {refused}")
    administer(ada, "/admin/access-groups",
               {"access_group_id": access_group, "name": "Plugin page readers",
                "entries": f"{INSTANCE} write"})
    offered = buttons(ada)
    check(offered == ["Manage", "Open", "View"], f"holding admin and write, the home offers three: {offered}")
    opener, why = session_at(ada, "write")
    check(opener is not None, f"an Open session: {why}")
    status, caller, _ = claims_on(opener) if opener else (0, {}, {})
    check(caller.get("level") == 2 and caller.get("access") == {"read": [account], "write": [account]},
          f"at write, with the read set and the write set: {caller}")
    # The sidecar reads the plugin's write scope again within 30 seconds.
    deadline = time.monotonic() + 45
    written = write(opener) if opener else {}
    while opener and not written.get("ok") and time.monotonic() < deadline:
        time.sleep(3)
        written = write(opener)
    check(written.get("ok"), f"once she writes, under Open, it is recorded: {written}")

    say("K: a required secret set in the dashboard reaches the running plugin (W6.11)")
    settings_reach_the_running_plugin(ada, viewer, reports)
    ada.get(dash("/admin"))

    say("M: Bea, granted the plugin's admin alone, configures it and sees no account's data")
    page = administer(ada, "/admin/user-groups",
                      {"user_group_id": "", "name": "Plugin admins", "logins": "local|bea"})
    bea_group = row_id(page, "User", "Plugin admins")
    page = administer(ada, "/admin/access-groups",
                      {"access_group_id": "", "name": "Plugin page admins",
                       "entries": f"{INSTANCE} admin"})
    admin_group = row_id(page, "Access", "Plugin page admins")
    check(None not in (bea_group, admin_group), f"groups listed: {bea_group} {admin_group}")
    refused = ada.post(dash("/admin/permissions"),
                       {"user_group_id": bea_group, "account_group_id": account_group,
                        "access_group_id": admin_group, "form_token": form_token(page)})
    check(refused.status == 400 and "names no account group" in refused.body,
          f"admin alone is granted on no account group (W6.8): {refused.status} {sentence(refused)}")
    administer(ada, "/admin/permissions",
               {"user_group_id": bea_group, "account_group_id": "", "access_group_id": admin_group})
    bea = Browser()
    signed = sign_in(bea, BEA, PASSWORD)
    check(signed.status == 303, f"Bea signs in: {signed.status}")
    offered = buttons(bea)
    check(offered == ["Manage"], f"her home offers Manage alone: {offered}")
    check(bea.get(dash("/admin")).status == 403, "the deployment's settings are not hers")
    tabs_page = bea.get(dash(f"{VIEW}?tab=settings"))
    check(tabs_page.status == 200 and 'data-setting="poll_minutes"' in tabs_page.body,
          f"its Settings tab is: {tabs_page.status} {sentence(tabs_page)}")
    saved = bea.post(dash(f"/admin/plugins/{INSTANCE}/settings"),
                     {"form_token": form_token(tabs_page), "value.poll_minutes": "20"})
    check(saved.status == 303, f"and she sets them: {saved.status} {sentence(saved)}")
    beas, why = session_at(bea, "admin")
    check(beas is not None, f"a Manage session: {why}")
    status, caller, _ = claims_on(beas) if beas else (0, {}, {})
    check(caller.get("level") == 3 and caller.get("deployment_admin") is False
          and caller.get("access") == {"read": [], "write": []},
          f"at admin, no account, and not a deployment admin: {caller}")
    relinked = link(beas, external_account_id="ext-e2e", account_id=account)
    check(relinked.get("ok"), f"she links to any existing account, whatever she may read: {relinked}")
    named = link(beas, external_account_id="ext-e2e-roth", new_account_name="Bea's own")
    check(named.get("code") == "PERMISSION_DENIED" and "only a deployment admin" in named.get("detail", ""),
          f"and is refused naming a new one: {named}")
    refused = write(beas)
    check(not refused.get("ok") and "Manage (admin)" in refused.get("detail", ""),
          f"nothing is sent for her on an account: {refused}")
    check(session_at(bea, "read")[0] is None, "and she has no View")

    say("N: a permission naming All accounts reaches an account no other group lists (W6.6)")
    page = administer(ada, "/admin/accounts", {"account_id": "", "name": "Ungrouped account"})
    ungrouped = row_id(page, "Accounts", "Ungrouped account")
    page = administer(ada, "/admin/access-groups",
                      {"access_group_id": "", "name": "Everything readers",
                       "entries": f"{INSTANCE} read"})
    everything = row_id(page, "Access", "Everything readers")
    administer(ada, "/admin/permissions",
               {"user_group_id": bea_group, "account_group_id": "all-accounts",
                "access_group_id": everything})
    offered = buttons(bea)
    check(offered == ["Manage", "View"], f"Bea finds View beside Manage: {offered}")
    beaview, why = session_at(bea, "read")
    status, caller, _ = claims_on(beaview) if beaview else (0, {}, {})
    check(ungrouped is not None and ungrouped in caller.get("access", {}).get("read", []),
          f"and reads the ungrouped account through it: {ungrouped} {caller.get('access')}")

    say("O: Ada's link to All plugins (admin) withdrawn, she configures the plugin no more")
    page = ada.get(dash("/admin"))
    linked_to_all = permission_of(page, "all-plugins-admin")
    check(linked_to_all is not None, "the claim linked her group to All plugins (admin)")
    administer(ada, "/admin/permissions/withdraw", {"permission_id": linked_to_all or ""})
    offered = buttons(ada)
    check(offered == ["Open", "View"], f"her home offers Open and View, and no Manage: {offered}")
    check(session_at(ada, "admin")[0] is None, "a Manage session is refused")
    status, _, _, _ = on_plugin_host(manager, "/admin/accounts")
    check(status == 403, f"and the one she held ends at its next request: {status}")
    settings = ada.get(dash(f"{VIEW}?tab=settings"))
    check('data-setting="api_key"' not in settings.body, "the Settings tab is gone for her")
    posted = ada.post(dash(f"/admin/plugins/{INSTANCE}/settings"),
                      {"form_token": form_token(settings), "value.poll_minutes": "30"})
    check(posted.status == 403, f"and setting them refused: {posted.status}")

    say("L: the secret is in no page and no report")
    pages = [page for page in PAGES if SECRET in page]
    check(not pages, f"in {len(pages)} of the {len(PAGES)} pages fetched")
    carried, total = reports.any_carry(SECRET)
    check(total > 0 and carried == 0, f"in {carried} of the {total} reports heard")

    say("J: signing out of the dashboard ends the plugin's session")
    home = ada.get(dash("/"))
    ada.post(dash("/sign-out"), {"form_token": form_token(home)})
    status, _, location, _ = on_plugin_host(viewer, "/")
    check(status == 303 and "/plugins/" in (location or "") and "/enter" in (location or ""),
          f"after sign-out: {status} to {location!r}")

    if FAILURES:
        say("")
        say(f"e2e-plugin-page FAILED: {len(FAILURES)}")
        for failure in FAILURES:
            say(f"  - {failure}")
        sys.exit(1)


if __name__ == "__main__":
    main()
