"""A person signs in to the dashboard in a real browser.

Run as a pod in the cluster under test, by e2e/cluster/run.py, against the
dashboard's Service. Every other sign-in in the suite is an HTTP client that
posts a form and copies a cookie by hand, which proves the server's half and
assumes the browser's: that the session cookie is kept and sent, that
`HttpOnly` keeps it from the page's scripts, that `SameSite` is what a browser
reads it as, that the sign-out form's token is carried by the form rather than
by a test. This is headless Chromium doing those things itself.

It runs in the cluster rather than beside it because the dashboard's own
Service name is the one address every cluster resolves the same way; a browser
outside would need the port-forward and the host names that differ between a
laptop and a CI runner.

Prints one line per check and exits non-zero if any failed.
"""

import os
import sys

from playwright.sync_api import sync_playwright

DASHBOARD = os.environ["E2E_DASHBOARD"].rstrip("/")
NAME = os.environ["E2E_NAME"]
PASSWORD = os.environ["E2E_PASSWORD"]
SESSION_COOKIE = "meridian_session"

failures = []


def check(held, said):
    print(f"{'ok' if held else 'FAILED'}: {said}", flush=True)
    if not held:
        failures.append(said)


def sign_in(page, password):
    page.goto(f"{DASHBOARD}/sign-in")
    page.fill("#name", NAME)
    page.fill("#password", password)
    page.click("button[type=submit]")
    page.wait_for_load_state()


with sync_playwright() as playwright:
    browser = playwright.chromium.launch()
    page = browser.new_page()

    sign_in(page, "not-the-password")
    check(
        page.url.endswith("/sign-in") and "not accepted" in page.content(),
        f"a wrong password for {NAME} is refused, on the form",
    )
    check(
        not any(c["name"] == SESSION_COOKIE for c in page.context.cookies()),
        "and the browser holds no session",
    )

    sign_in(page, PASSWORD)
    check(page.url == f"{DASHBOARD}/", f"{NAME} signs in and lands home: {page.url}")
    check(
        "You are a deployment admin" in page.content(),
        "home says they administer this deployment",
    )

    session = [c for c in page.context.cookies() if c["name"] == SESSION_COOKIE]
    check(len(session) == 1, "the browser kept one session cookie")
    if session:
        cookie = session[0]
        check(cookie["httpOnly"], "marked HttpOnly")
        check(cookie["sameSite"] == "Lax", f"read as SameSite={cookie['sameSite']}")
        # What HttpOnly is for, observed rather than read off a header: a
        # script on the page cannot see the session.
        check(
            SESSION_COOKIE not in page.evaluate("document.cookie"),
            "and no script on the page can read it",
        )

    # Sent back on its own: a fresh navigation carries the session because the
    # browser chose to send it, not because a test copied it into a header.
    page.goto(f"{DASHBOARD}/")
    check("You are a deployment admin" in page.content(), "a new page load is still signed in")

    # Sign out through the page's own form, whose token the browser carries.
    page.click("text=Sign out")
    page.wait_for_load_state()
    page.goto(f"{DASHBOARD}/")
    check(
        "You are a deployment admin" not in page.content()
        and "Signed in as" not in page.content(),
        "signing out through the form ends the session",
    )

    browser.close()

print(flush=True)
if failures:
    print(f"browser FAILED: {len(failures)}", flush=True)
    sys.exit(1)
print("browser OK", flush=True)
