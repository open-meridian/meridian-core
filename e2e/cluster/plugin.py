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

Prints one line per check and exits non-zero if any failed.
"""

import os
import socket
import sys
import threading
import time
import urllib.error
import urllib.request

from playwright.sync_api import sync_playwright

UPSTREAM = os.environ["E2E_DASHBOARD"].removeprefix("http://").rstrip("/")
PORT = int(os.environ["E2E_PORT"])
DASHBOARD = f"http://localhost:{PORT}"
BY = os.environ["E2E_BY"]  # password or redirect
NAME = os.environ.get("E2E_NAME", "")
PASSWORD = os.environ.get("E2E_PASSWORD", "")
INSTANCE = os.environ["E2E_INSTANCE"]
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
    listed = said_by("list.out", 60)
    check(
        f"{INSTANCE}  reference-plugin 0.1.0  launched" in listed,
        "`meridian plugin list` shows it launched",
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
        # A pod's first seconds are refused by the cluster's policy engine,
        # and the plugin's sidecar may still be joining: once more if so.
        for _ in range(10):
            if "Reference plugin" in page.content():
                break
            time.sleep(3)
            page.goto(f"{DASHBOARD}/plugins/{INSTANCE}")
            page.wait_for_load_state()
        at = page.url
        check(
            at.startswith(f"http://{INSTANCE}.plugins.localhost:{PORT}/"),
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
        # Informational: the plugin holds no role, so recording a statement
        # is none of its steps, whoever asks.
        print(f"  the statement: {page.locator('strong').last.inner_text()}", flush=True)
    context.close()
    browser.close()

print(flush=True)
if failures:
    print(f"plugin FAILED: {len(failures)}", flush=True)
    sys.exit(1)
print("plugin OK", flush=True)
