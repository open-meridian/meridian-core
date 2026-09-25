"""A person connects a terminal: W6.13 and W6.14 on a real cluster.

Run as a pod by e2e/cluster/run.py, section T. The loopback redirect needs
the terminal and the browser on one machine, and a pod is one: its
containers share 127.0.0.1. So this container holds

- a forwarder from 127.0.0.1:<port> to the dashboard's Service, on the port
  the dashboard believes is its own address, so that a provider sending the
  browser back to that address reaches it from in here too;
- headless Chromium, which signs in and confirms as a person would;
- and the terminal: either a stand-in for `meridian connect` written here
  (E2E_TERMINAL=python, what core's CI runs, having no CLI binary), or the
  real CLI in the pod's other container (E2E_TERMINAL=cli, what
  meridian-cli's `make e2e-up` runs), told what to do through files in a
  volume they share.

What it proves, on whichever of the three ways in the deployment has:

- a terminal's sign-in is the deployment's own, done in a browser, and ends
  in a confirmation rather than a browser session;
- the terminal holds a session afterwards, and the dashboard holds it too:
  the admin page lists it;
- the session is nobody on a browser page;
- signing out with that session ends it -- which a made-up one could not,
  so it is the session and not the request that did it;
- a deployment admin ends a person's terminal sessions from the admin page.

A request whose answer depends on the person's permissions arrives with the
first terminal path that has one, the plugin upload; kernel/terminal-sessions
says so.

Prints one line per check and exits non-zero if any failed.
"""

import base64
import hashlib
import json
import os
import secrets
import socket
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

from playwright.sync_api import sync_playwright

UPSTREAM = os.environ["E2E_DASHBOARD"].removeprefix("http://").rstrip("/")
PORT = int(os.environ["E2E_PORT"])
DASHBOARD = f"http://127.0.0.1:{PORT}"
BY = os.environ["E2E_BY"]  # password or redirect
NAME = os.environ.get("E2E_NAME", "")
PASSWORD = os.environ.get("E2E_PASSWORD", "")
TERMINAL = os.environ.get("E2E_TERMINAL", "python")
SHARED = "/shared"

failures = []


def check(held, said):
    print(f"{'ok' if held else 'FAILED'}: {said}", flush=True)
    if not held:
        failures.append(said)


# ── The forwarder ─────────────────────────────────────────────────────────


def forward():
    host, _, port = UPSTREAM.partition(":")
    listening = socket.create_server(("127.0.0.1", PORT))

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


threading.Thread(target=forward, daemon=True).start()


def reachable():
    try:
        return urllib.request.urlopen(f"{DASHBOARD}/healthz", timeout=5).status == 200
    except (urllib.error.URLError, OSError):
        return False


for _ in range(120):
    if reachable():
        break
    time.sleep(1)
else:
    print("FAILED: the dashboard never answered through the forwarder", flush=True)
    sys.exit(1)


# ── The terminal ──────────────────────────────────────────────────────────


class StandIn:
    """`meridian connect` and `meridian sign-out`, as the CLI does them.

    The listener, the PKCE pair and the exchange, and nothing else: the real
    CLI's own tests hold it to the same, and meridian-cli's e2e runs it here
    in this one's place.
    """

    def __init__(self):
        self.session = None

    def connect(self):
        verifier = base64.urlsafe_b64encode(secrets.token_bytes(32)).rstrip(b"=").decode()
        challenge = base64.urlsafe_b64encode(
            hashlib.sha256(verifier.encode()).digest()
        ).rstrip(b"=").decode()
        state = secrets.token_urlsafe(16)
        listening = socket.create_server(("127.0.0.1", 0))
        back = f"http://127.0.0.1:{listening.getsockname()[1]}/callback"
        url = f"{DASHBOARD}/terminal/authorize?" + urllib.parse.urlencode({
            "redirect_uri": back,
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "state": state,
        })
        returned = {}

        def wait():
            while True:
                client, _ = listening.accept()
                line = client.recv(8192).decode(errors="replace").split("\r\n", 1)[0]
                target = line.split(" ")[1] if line.startswith("GET ") else ""
                query = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(target).query))
                client.sendall(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                client.close()
                if query.get("state") == state:
                    returned.update(query)
                    return

        waiting = threading.Thread(target=wait, daemon=True)
        waiting.start()

        def finish():
            waiting.join(timeout=120)
            if "code" not in returned:
                return None
            data = urllib.parse.urlencode({
                "code": returned["code"],
                "code_verifier": verifier,
                "redirect_uri": back,
            }).encode()
            reply = json.loads(
                urllib.request.urlopen(f"{DASHBOARD}/terminal/token", data=data).read()
            )
            self.session = reply["session"]
            return reply["subject"]

        return url, finish

    def sign_out(self):
        request = urllib.request.Request(
            f"{DASHBOARD}/terminal/sign-out",
            method="POST",
            headers={"Authorization": f"Bearer {self.session}"},
        )
        return urllib.request.urlopen(request).status == 204


class RealCli:
    """The real `meridian`, in the pod's other container, told through files.

    It runs `connect`, then waits for `sign-out-now` to run `sign-out`, then
    waits for `connect-again` to connect once more (e2e/cluster/run.py
    writes its script). Its session is read back from the sessions file it
    keeps, which is what proves it kept one.
    """

    def __init__(self):
        self.round = 0

    def connect(self):
        self.round += 1
        said = f"{SHARED}/connect-{self.round}.out"
        if self.round > 1:
            open(f"{SHARED}/connect-again", "w").close()
        url = None
        for _ in range(120):
            text = open(said).read() if os.path.exists(said) else ""
            url = next(
                (w for w in text.split() if w.startswith(f"{DASHBOARD}/terminal/authorize?")),
                None,
            )
            if url:
                break
            time.sleep(1)

        def finish():
            for _ in range(120):
                text = open(said).read() if os.path.exists(said) else ""
                if "exit=" in text:
                    break
                time.sleep(1)
            for line in text.splitlines():
                print(f"  | {line}", flush=True)
            held = f"{SHARED}/config/meridian/sessions/127.0.0.1_{PORT}.json"
            if "exit=0" not in text or not os.path.exists(held):
                return None
            mode = os.stat(held).st_mode & 0o777
            check(mode == 0o600, f"the sessions file is its owner's alone: {oct(mode)}")
            return json.load(open(held))["subject"]

        return url, finish

    def sign_out(self):
        open(f"{SHARED}/sign-out-now", "w").close()
        said = f"{SHARED}/sign-out.out"
        for _ in range(60):
            text = open(said).read() if os.path.exists(said) else ""
            if "exit=" in text:
                for line in text.splitlines():
                    print(f"  | {line}", flush=True)
                return "exit=0" in text and "Signed out" in text
            time.sleep(1)
        return False


terminal = RealCli() if TERMINAL == "cli" else StandIn()


# ── The person ────────────────────────────────────────────────────────────


def signed_in_at_the_form(page):
    page.fill("#name", NAME)
    page.fill("#password", PASSWORD)
    page.click("button[type=submit]")
    page.wait_for_load_state()


def connect(browser, round_name):
    """Sign in to the terminal's request in a fresh browser and confirm."""
    url, finish = terminal.connect()
    check(url is not None, f"{round_name}: the terminal asked to be signed in")
    if url is None:
        return None
    context = browser.new_context()
    page = context.new_page()
    page.goto(url)
    if BY == "password":
        check(
            "Sign in to connect a terminal" in page.content(),
            f"{round_name}: the deployment's own sign-in, for a terminal",
        )
        signed_in_at_the_form(page)
    check(
        "Connect a terminal" in page.content() and "Only connect it" in page.content(),
        f"{round_name}: signed in, and asked to confirm",
    )
    check(
        not any(c["name"] == "meridian_session" for c in context.cookies()),
        f"{round_name}: and no browser session was made",
    )
    page.click("button[value=connect]")
    page.wait_for_load_state()
    subject = finish()
    check(subject is not None, f"{round_name}: the terminal holds a session, as {subject}")
    context.close()
    return subject


def admin_page(browser):
    """A deployment admin's own browser, on the admin page."""
    context = browser.new_context()
    page = context.new_page()
    page.goto(f"{DASHBOARD}/sign-in")
    if BY == "password":
        signed_in_at_the_form(page)
    page.goto(f"{DASHBOARD}/admin")
    return context, page


def terminal_sessions(page):
    """How many terminal sessions the admin page lists, all told."""
    page.reload()
    if "Nobody holds a terminal session" in page.content():
        return 0
    rows = page.locator("h2:has-text('Terminal sessions') + table tr")
    return sum(int(rows.nth(i).locator("td").nth(2).inner_text()) for i in range(1, rows.count()))


with sync_playwright() as playwright:
    browser = playwright.chromium.launch()
    admin, page = admin_page(browser)
    check("Administer this deployment" in page.content(), "the administrator's own browser is on the admin page")
    check(terminal_sessions(page) == 0, "nobody holds a terminal session yet")

    subject = connect(browser, "first")
    check(terminal_sessions(page) == 1, "the admin page lists the terminal's session")

    if isinstance(terminal, StandIn):
        # The session on a browser page is nobody.
        request = urllib.request.Request(
            f"{DASHBOARD}/", headers={"Authorization": f"Bearer {terminal.session}"}
        )
        home = urllib.request.urlopen(request).read().decode()
        check('href="/sign-in"' in home, "the terminal's session is nobody on a browser page")

    check(terminal.sign_out(), "the terminal signs out")
    check(
        terminal_sessions(page) == 0,
        "and its session is gone from the admin page, so it was that session that ended",
    )

    connect(browser, "second")
    check(terminal_sessions(page) == 1, "a second connection is listed")
    page.click("button:has-text('End them')")
    page.wait_for_load_state()
    check("Ended 1 terminal session" in page.content(), "the administrator ends that person's terminal sessions")
    check(terminal_sessions(page) == 0, "and none is listed")

    admin.close()
    browser.close()

print(flush=True)
if failures:
    print(f"terminal FAILED: {len(failures)}", flush=True)
    sys.exit(1)
print("terminal OK", flush=True)
