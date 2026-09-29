"""A person makes a plugin, puts it in their deployment, and opens its page.

Run as a pod by e2e/cluster/run.py, section P, beside the real CLI in the
pod's other container: results 2 and 3 of plans/a-person-reaches-a-plugin.
The CLI makes the reference plugin (`meridian plugin new`), connects,
uploads it -- built by the node's own docker daemon, which the pod is given,
as a person's machine gives it theirs -- and launches it with no role and no
tag. This container is the person's browser: it confirms the CLI's
connection, and once the run says the plugin is up, signs in to the
dashboard as its administrator and opens the plugin's page from home.

The page is on the plugin's own host, `{instance}.plugins.localhost`, which
a browser sends to loopback by itself, and the forwarder here takes loopback
to the dashboard. That the page can be opened at all is ruling 19 of
spec/deployment-dashboard-and-access: the reference plugin declares nothing
to be granted, and an administrator opens any plugin's page.

Then acting-for, on a copy of the plugin declaring `custody`, which holds
the step its button takes (recording a statement): the administrator grants
herself read on an account through it and the button is refused, since she
may write nothing through it; the grant becomes write, and the same button
records the statement. What decides is her access, not the plugin's role.

Last, the upload's path as the first terminal path with a permission behind
it (kernel/terminal-sessions left this to it): the terminal's session is
what it takes, the browser's cookie in its place is refused, and once her
permission is withdrawn her terminal session -- still live -- is refused as
somebody who may not, rather than as nobody.

Then the live loop (spec/live-plugin-development), by the real CLI on this
development deployment: `meridian plugin dev` in the background on a copy of
the scaffold, saves made to it and seen running, crashing and mended; the
page read back with `open --print` and opened by `open`'s one-time link in a
browser signed in nowhere; `logs` and `events`; `dev --release` putting the
code in the catalogue as a version; and the instance made live again for
the run's check that turning development off removes it.

Prints one line per check and exits non-zero if any failed.
"""

import json
import os
import re
import shutil
import socket
import sys
import threading
import time
import urllib.error
import urllib.request

from playwright.sync_api import sync_playwright

UPSTREAM = os.environ["E2E_DASHBOARD"].removeprefix("http://").rstrip("/")
HOST = os.environ["E2E_HOST"]
PORT = 80
DASHBOARD = f"http://{HOST}"
BY = os.environ["E2E_BY"]  # password or redirect
NAME = os.environ.get("E2E_NAME", "")
PASSWORD = os.environ.get("E2E_PASSWORD", "")
INSTANCE = os.environ["E2E_INSTANCE"]
CUSTODY = os.environ["E2E_CUSTODY_INSTANCE"]
LIVE = os.environ.get("E2E_LIVE_INSTANCE", "reference-live")
SHARED = "/shared"

failures = []


def check(held, said):
    print(f"{'ok' if held else 'FAILED'}: {said}", flush=True)
    if not held:
        failures.append(said)


def said_by(name, seconds=600):
    """What the CLI wrote to /shared/<name>, once it has finished."""
    path = f"{SHARED}/{name}"
    text = ""
    for _ in range(seconds):
        text = open(path).read() if os.path.exists(path) else ""
        if "exit=" in text:
            break
        time.sleep(1)
    for line in text.splitlines():
        print(f"  | {line}", flush=True)
    return text


# ── The forwarder ─────────────────────────────────────────────────────────
# Loopback, on the port the dashboard believes is its own, to its Service.
# Both loopbacks, because `localhost` is either, and a browser tries both.


def forward(family, address):
    host, _, port = UPSTREAM.partition(":")
    try:
        listening = socket.create_server((address, PORT), family=family)
    except OSError:
        return  # no IPv6 in this pod: IPv4 alone serves

    def pipe(source, sink):
        try:
            while data := source.recv(65536):
                sink.sendall(data)
        except OSError:
            pass
        finally:
            for end in (source, sink):
                try:
                    end.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def serve(client):
        # A new pod's first seconds are refused by the cluster's policy
        # engine, so the first connection is retried rather than trusted.
        for _ in range(30):
            try:
                upstream = socket.create_connection((host, int(port or 80)), timeout=5)
                break
            except OSError:
                time.sleep(1)
        else:
            client.close()
            return
        upstream.settimeout(None)
        threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
        threading.Thread(target=pipe, args=(upstream, client), daemon=True).start()

    while True:
        client, _ = listening.accept()
        threading.Thread(target=serve, args=(client,), daemon=True).start()


for family, address in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
    threading.Thread(target=forward, args=(family, address), daemon=True).start()

for _ in range(120):
    try:
        if urllib.request.urlopen(f"{DASHBOARD}/healthz", timeout=5).status == 200:
            break
    except (urllib.error.URLError, OSError):
        pass
    time.sleep(1)
else:
    print("FAILED: the dashboard never answered through the forwarder", flush=True)
    sys.exit(1)


# ── The administrator's forms ─────────────────────────────────────────────


def sentence(body):
    found = re.search(r"<p[^>]*>(.*?)</p>", body, re.S)
    return found.group(1) if found else body[:200]


def row_id(body, section, name):
    """The identifier the conductor gave the row named `name` in a table."""
    table = body.split(f"<h2>{section}</h2>", 1)[-1].split("</table>", 1)[0]
    found = re.search(r'<tr data-id="([^"]+)" data-name="' + re.escape(name) + '"', table)
    return found.group(1) if found else None


def administer(context, action, fields, patience=0):
    """Post an admin form with the signed-in browser's cookies, again for up
    to `patience` seconds while it is refused: a plugin is known to the
    conductor once its sidecar's first report arrives. The admin page."""
    deadline = time.monotonic() + patience
    while True:
        admin = context.request.get(f"{DASHBOARD}/admin").text()
        token = re.search(r'name="form_token" value="([^"]+)"', admin)
        done = context.request.post(
            f"{DASHBOARD}{action}",
            form=dict(fields, form_token=token.group(1) if token else ""),
            max_redirects=0,
        )
        if done.status == 303 or time.monotonic() > deadline:
            break
        time.sleep(2)
    check(done.status == 303, f"POST {action}: {done.status} {sentence(done.text())}")
    return context.request.get(f"{DASHBOARD}/admin").text()


def statement(page, instance):
    """Press the page's one button: what the plugin says came of it."""
    page.goto(f"http://{instance}.plugins.{HOST}/")
    page.wait_for_load_state()
    button = page.locator("button:has-text('Open an empty statement for me')")
    if not button.count():
        return f"no statement button: {sentence(page.inner_text('body'))}"
    button.click()
    page.wait_for_load_state()
    notice = page.locator("p > strong")
    return notice.last.inner_text() if notice.count() else page.content()[:300]


# ── The terminal: the real CLI, in the pod's other container ────────────


ASKED = [0]


def cli(command, seconds=300):
    """Run a command in the CLI's container, on the session `meridian
    connect` kept there; what it said, ending with its exit."""
    ASKED[0] += 1
    at = f"{SHARED}/ask/{ASKED[0]}"
    os.makedirs(f"{SHARED}/ask", exist_ok=True)
    with open(f"{at}.writing", "w") as asking:
        asking.write(command + "\n")
    os.rename(f"{at}.writing", f"{at}.sh")
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if os.path.exists(f"{at}.out"):
            with open(f"{at}.out") as said:
                return said.read()
        time.sleep(0.1)
    return f"(no answer within {seconds}s)"


def said_json(text):
    """The JSON object a `--json` command printed, from among its lines."""
    for line in text.splitlines():
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                pass
    return {}


def dev_events(name):
    """What `meridian plugin dev --json` has printed so far: one event a line."""
    try:
        with open(f"{SHARED}/{name}") as printed:
            lines = printed.read().splitlines()
    except FileNotFoundError:
        return []
    events = []
    for line in lines:
        try:
            events.append(json.loads(line))
        except ValueError:
            pass
    return events


def dev_said(name):
    try:
        with open(f"{SHARED}/{name}") as said:
            return said.read()
    except FileNotFoundError:
        return ""


def waited(check, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        found = check()
        if found:
            return found
        time.sleep(0.1)
    return None


def first(events, event, revision=None, after=0):
    return [e for e in events if e.get("event") == event
            and (e.get("revision") == revision if revision is not None else e.get("revision", 0) > after)]


# ── The person ────────────────────────────────────────────────────────────


def signed_in_at_the_form(page):
    page.fill("#name", NAME)
    page.fill("#password", PASSWORD)
    page.click("button[type=submit]")
    page.wait_for_load_state()


def confirm_the_connection(browser):
    """The CLI printed where to sign in; do so, and confirm."""
    url = None
    for _ in range(300):
        text = open(f"{SHARED}/connect.out").read() if os.path.exists(f"{SHARED}/connect.out") else ""
        url = next(
            (w for w in text.split() if w.startswith(f"{DASHBOARD}/terminal/authorize?")),
            None,
        )
        if url:
            break
        time.sleep(1)
    check(url is not None, "the CLI asked to be signed in")
    if url is None:
        return
    context = browser.new_context()
    page = context.new_page()
    page.goto(url)
    if BY == "password":
        signed_in_at_the_form(page)
    page.click("button[value=connect]")
    page.wait_for_load_state()
    context.close()
    check("exit=0" in said_by("connect.out", 120), "and it connected")


with sync_playwright() as playwright:
    browser = playwright.chromium.launch()
    confirm_the_connection(browser)

    uploaded = said_by("upload.out")
    check(
        "exit=0" in uploaded and "Uploaded to" in uploaded,
        "`meridian plugin upload` built the reference plugin here and put it in the catalogue",
    )
    launched = said_by("launch.out", 120)
    check(
        "exit=0" in launched and "roles: none" in launched and "tags:  none" in launched,
        "`meridian plugin launch` showed it asks for no role and no tag, and launched it",
    )
    check(
        "roles = [\"custody\"]" in said_by("declare-custody.out", 300),
        "a copy of it declares `custody` in its pyproject.toml",
    )
    uploaded = said_by("upload-custody.out")
    check("exit=0" in uploaded and "Uploaded to" in uploaded, "and is uploaded")
    check(
        "already there" in uploaded,
        "sending only what the registry lacks: the base it shares with the first is not sent again",
    )
    launched = said_by("launch-custody.out", 120)
    check(
        "exit=0" in launched and "roles: custody" in launched,
        "and launched, asking for `custody` and nothing more",
    )
    listed = said_by("list.out", 60)
    check(
        f"{INSTANCE}  reference-plugin 0.1.0  launched" in listed
        and f"{CUSTODY}  reference-custody 0.1.0  launched" in listed,
        "`meridian plugin list` shows both launched",
    )

    # The run says when the plugin's pod is ready: this container cannot
    # see the cluster, only the dashboard.
    for _ in range(600):
        if os.path.exists(f"{SHARED}/open-now"):
            break
        time.sleep(1)
    check(os.path.exists(f"{SHARED}/open-now"), "the plugin came up")

    context = browser.new_context()
    page = context.new_page()
    page.goto(f"{DASHBOARD}/sign-in")
    if BY == "password":
        signed_in_at_the_form(page)
    page.goto(f"{DASHBOARD}/")
    link = page.locator(f"a[href='/plugins/{INSTANCE}']")
    check(link.count() == 1, f"home links {INSTANCE} for its administrator, who holds no access on it")
    if link.count() == 1:
        link.click()
        page.wait_for_load_state()
        # The dashboard's frame, under its header; the page itself is read
        # through the frame's way in, which opens it in a window of its own.
        check(page.locator("iframe[data-plugin-frame]").count() == 1,
              "opened in the dashboard's frame")
        # A pod's first seconds are refused by the cluster's policy engine,
        # and the plugin's sidecar may still be joining: once more if so.
        for _ in range(10):
            page.goto(f"{DASHBOARD}/plugins/{INSTANCE}/enter")
            page.wait_for_load_state()
            if "Reference plugin" in page.content():
                break
            time.sleep(3)
        at = page.url
        check(
            at.startswith(f"http://{INSTANCE}.plugins.{HOST}/"),
            f"the page is on the plugin's own host: {at}",
        )
        content = page.content()
        check("<h1>Reference plugin</h1>" in content, "and it is the reference plugin's page")
        check(
            "Signed in as <strong>" in content and "<strong></strong>" not in content,
            "which the plugin was told the person asking by the dashboard, through its sidecar",
        )
        page.click("button:has-text('Open an empty statement for me')")
        page.wait_for_load_state()
        refused = page.locator("p > strong").last.inner_text()
        check(
            refused.startswith("Refused") and "holds no role" in refused,
            f"it may not record a statement, whoever asks, holding no role: {refused}",
        )

    # Acting-for, on the copy that holds the step: her access decides.
    admin = administer(context, "/admin/accounts", {"account_id": "", "name": "Custody copy account"})
    account = row_id(admin, "Accounts", "Custody copy account")
    admin = administer(
        context, "/admin/account-groups",
        {"account_group_id": "", "name": "Custody copy accounts", "account_ids": account or ""},
    )
    account_group = row_id(admin, "Account groups", "Custody copy accounts")
    admin = administer(
        context, "/admin/access-groups",
        {"access_group_id": "", "name": "Custody copy users", "entries": f"{CUSTODY} custody read"},
        patience=90,
    )
    access_group = row_id(admin, "Access groups", "Custody copy users")
    # The user group the wizard made her deployment admin through: the one
    # permission there is before hers.
    admins = re.search(r'<tr data-id="[^"]+" data-user-group="([^"]+)"', admin)
    check(
        None not in (account, account_group, access_group, admins),
        f"read on one account through {CUSTODY}, for her own user group: "
        f"{account} {account_group} {access_group} {admins and admins.group(1)}",
    )
    if admins:
        administer(
            context, "/admin/permissions",
            {"user_group_id": admins.group(1), "account_group_id": account_group or "",
             "access_group_id": access_group or ""},
        )

    # The permission reaches the plugin's sidecar after the dashboard says it
    # was granted, not with it: looked for again until it has.
    deadline = time.monotonic() + 60
    while True:
        page.goto(f"{DASHBOARD}/plugins/{CUSTODY}/enter")
        page.wait_for_load_state()
        shown = "<td>custody</td>" in page.content()
        if shown or time.monotonic() > deadline:
            break
        time.sleep(2)
    check(
        page.url.startswith(f"http://{CUSTODY}.plugins.{HOST}/") and shown,
        f"{CUSTODY}'s page shows what she may see through it: {page.url}"
        + ("" if shown else f" {sentence(page.inner_text('body'))}"),
    )
    said = statement(page, CUSTODY)
    check(
        said.startswith("Refused") and "may write nothing" in said,
        f"while she only reads, the sidecar refuses the statement she asked for: {said}",
    )

    administer(
        context, "/admin/access-groups",
        {"access_group_id": access_group or "", "name": "Custody copy users",
         "entries": f"{CUSTODY} custody write"},
    )
    # The sidecar reads the plugin's write scope again within 30 seconds.
    deadline = time.monotonic() + 90
    said = statement(page, CUSTODY)
    while not said.startswith("Opened statement") and time.monotonic() < deadline:
        time.sleep(5)
        said = statement(page, CUSTODY)
    check(said.startswith("Opened statement"), f"once she writes, it is recorded for her: {said}")

    # The live loop (spec/live-plugin-development, requirements 10 to 14),
    # by the real CLI on this development deployment: `plugin dev` in the
    # background on a copy of the scaffold, saves made to that copy as an
    # editor makes them, and what the person or an agent reads back with
    # `open --print`, `logs` and `events`.
    work = f"{SHARED}/live-plugin"
    shutil.copytree(f"{SHARED}/reference-plugin", work)
    cli(f"meridian plugin dev --dir {work} --instance {LIVE} --yes --json"
        f" > {SHARED}/dev.out 2> {SHARED}/dev.err & echo $! > {SHARED}/dev.pid")
    ready = waited(lambda: first(dev_events("dev.out"), "ready"), 300)
    check(bool(ready), f"`meridian plugin dev` launched {LIVE} live, sent it the directory, and it is"
          f" ready: {ready or sentence(dev_said('dev.err'))}")
    at = max((e.get("revision", 0) for e in ready or []), default=0)

    scaffold = f"{work}/src/reference_plugin"
    page_py = open(f"{scaffold}/page.py").read()
    main_py = open(f"{scaffold}/__main__.py").read()
    changed = page_py.replace('TITLE = "Reference plugin"', 'TITLE = "Reference plugin, changed live"')
    check(changed != page_py, "the page's title is there to change")

    def saved(path, text):
        """A save, and the revision it was sent as."""
        with open(path, "w") as out:
            out.write(text)
        sent = waited(lambda: first(dev_events("dev.out"), "sent", after=at), 30)
        return max((e["revision"] for e in sent), default=-1) if sent else -1

    saved_at = time.monotonic()
    revision = saved(f"{scaffold}/page.py", changed)
    running = waited(lambda: first(dev_events("dev.out"), "ready", revision), 60)
    took = time.monotonic() - saved_at
    check(bool(running), f"a save is sent as revision {revision}, and running within {took:.1f}s,"
          " the pod and its sidecar as they were")
    check(took < 3, f"under three seconds from saving to running (ruling 9): {took:.2f}s")
    print(f"  save to ready: {took:.2f}s", flush=True)
    at = revision

    printed = cli(f"meridian plugin open --instance {LIVE} --print /")
    check("exit=0" in printed and "<h1>Reference plugin, changed live</h1>" in printed,
          f"`plugin open --print /` shows the changed page as she is served it: {sentence(printed)}")

    linked = said_json(cli(f"meridian plugin open --instance {LIVE} --json"))
    url = linked.get("url", "")
    elsewhere = browser.new_context()
    other = elsewhere.new_page()
    other.goto(url or "about:blank")
    other.wait_for_load_state()
    check(other.url.startswith(f"http://{LIVE}.plugins.{HOST}/")
          and "changed live" in other.content(),
          f"`plugin open` gives a link another browser, signed in nowhere, opens the page by: {other.url}")
    other.goto(url or "about:blank")
    check("has been used" in other.content(), "once")
    elsewhere.close()

    revision = saved(f"{scaffold}/__main__.py", "raise RuntimeError('broken on purpose')\n" + main_py)
    crashed = waited(lambda: first(dev_events("dev.out"), "crashed", revision), 60)
    check(bool(crashed) and "broken on purpose" in (crashed or [{}])[0].get("traceback", ""),
          "a save that fails to start is reported as crashed, with its traceback")
    at = revision

    mended = saved(f"{scaffold}/__main__.py", main_py)
    check(bool(waited(lambda: first(dev_events("dev.out"), "ready", mended), 60)),
          "and mended by the next save, without anybody restarting anything")
    page.goto(f"{DASHBOARD}/plugins/{LIVE}/enter")
    page.wait_for_load_state()
    page.click("button:has-text('Open an empty statement for me')")
    page.wait_for_load_state()
    refused = waited(lambda: [e for e in said_json(cli(
        f"meridian plugin events --instance {LIVE} --since {mended - 1} --json")).get("events", [])
        if e.get("event") == "refused"], 30)
    check(bool(refused) and "no grant" in (refused or [{}])[0].get("reason", ""),
          "`plugin events` has what the sidecar refused it, where whoever is developing it looks")
    logs = cli(f"meridian plugin logs --instance {LIVE} --since {mended - 1}")
    check("exit=0" in logs and "serving its page" in logs,
          f"`plugin logs` has what it printed since: {sentence(logs)}")
    lapsed = cli(f"meridian plugin logs --instance {LIVE} --deployment http://nowhere.localhost")
    check("exit=3" in lapsed, f"and with no session, it says so and exits 3: {sentence(lapsed)}")

    # Released: the directory as it is, a version, run in place of the live
    # code. A version is never replaced, so it is a new one.
    pyproject = f"{work}/pyproject.toml"
    with open(pyproject) as held:
        declared = held.read()
    with open(pyproject, "w") as out:
        out.write(declared.replace('version = "0.1.0"', 'version = "0.1.1"'))
    released = cli(f"meridian plugin dev --release --dir {work} --instance {LIVE} --yes", 900)
    check("exit=0" in released and "Released reference-plugin 0.1.1" in released,
          f"`plugin dev --release` uploads it as 0.1.1 and runs that: {sentence(released)}")
    listed = cli("meridian plugin list")
    check(re.search(rf"{LIVE}\s+reference-plugin 0\.1\.1\s+launched", listed) is not None,
          "the catalogue holds 0.1.1, launched in the live instance's place")
    served = waited(lambda: "changed live" in cli(f"meridian plugin open --instance {LIVE} --print /"), 180)
    check(bool(served), "and the released version serves what was live")

    # Live again, for the run's last check: stopped, and developed again from
    # the version now recorded, with the directory sent over it.
    cli(f"kill -INT $(cat {SHARED}/dev.pid)")
    cli(f"meridian plugin stop {LIVE}")
    cli(f"meridian plugin dev --dir {work} --instance {LIVE} --yes --json"
        f" > {SHARED}/dev-again.out 2> {SHARED}/dev-again.err & echo $! > {SHARED}/dev.pid")
    again = waited(lambda: first(dev_events("dev-again.out"), "ready"), 300)
    check(bool(again) and "uploaded already" in dev_said("dev-again.err"),
          f"stopped and developed again, the recorded 0.1.1 runs live with the directory sent over it:"
          f" {sentence(dev_said('dev-again.err'))}")
    open(f"{SHARED}/live-done", "w").close()

    # The terminal's paths take the terminal's session, and nothing else: the
    # browser's cookie, which the CLI's upload and list above did without, is
    # nobody there.
    by_cookie = context.request.get(f"{DASHBOARD}/terminal/plugins")
    check(
        by_cookie.status == 401,
        f"her browser's session in place of the terminal's is refused: {by_cookie.status}",
    )

    # Her permission withdrawn. A deployment is never left without an
    # administrator, so first a second one, for a login nobody holds.
    admin = administer(
        context, "/admin/user-groups",
        {"user_group_id": "", "name": "Stand-in administrators", "directory_groups": "",
         "logins": "local|nobody-e2e"},
    )
    stand_in = row_id(admin, "User groups", "Stand-in administrators")
    admin = administer(
        context, "/admin/permissions",
        {"user_group_id": stand_in or "", "account_group_id": "", "access_group_id": "deployment-admin"},
    )
    hers = admins and re.search(
        r'<tr data-id="([^"]+)" data-user-group="' + re.escape(admins.group(1))
        + r'" data-account-group="" data-access-group="deployment-admin">',
        admin,
    )
    check(bool(hers), f"her permission to deployment admin is listed: {hers and hers.group(1)}")
    if hers:
        administer(context, "/admin/permissions/withdraw", {"permission_id": hers.group(1)})
    open(f"{SHARED}/list-again", "w").close()
    again = said_by("list-again.out", 120)
    check(
        "exit=0" not in again and "403" in again and "only a deployment admin" in again,
        "and her terminal session, still live, is refused as somebody who may not, not as nobody",
    )
    context.close()
    browser.close()

print(flush=True)
if failures:
    print(f"plugin FAILED: {len(failures)}", flush=True)
    sys.exit(1)
print("plugin OK", flush=True)
