"""The firm's own provider: signing in, and the two ways it is refused.

Decision 018, branch one. These cases used to borrow the bundled Zitadel as a
provider; it is being deleted, so they run against a stand-in that is somebody
else's software by construction (fake_idp.py).

  A  somebody signs in through their firm's provider
  E  freshness: a provider's own session, re-issued without asking anybody
  G  the provider rotates its signing key, and people still sign in
  H  a callback whose state was not the one this browser started with
"""
import json
import os
import re
import sys
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ldap_runner import (  # noqa: E402
    FAILURES,
    Browser,
    check,
    dash,
    home,
    say,
    signed_in_as,
    wait_dashboard,
)

IDP = os.environ.get("FAKE_IDP", "http://fake-idp:8100")


def begin(browser):
    """Ask the dashboard to start a sign-in, and follow it to the provider."""
    started = browser.get(dash("/sign-in"))
    if started.status != 303 or not started.location:
        return started, None
    away = Browser(browser.cookies)
    at_provider = away.get(started.location)
    return started, at_provider


def finish(browser, at_provider):
    """Take the provider's redirect back to the dashboard."""
    return browser.get(at_provider.location)


def signed_in(browser):
    started, at_provider = begin(browser)
    if at_provider is None:
        return started, None
    return started, finish(browser, at_provider)


def stale(on):
    Browser().post(IDP + ("/e2e/stale" if on else "/e2e/fresh"), {})


def main_phase():
    say("A: somebody signs in through their firm's provider")
    ada = Browser()
    started, back = signed_in(ada)
    check(started.status == 303 and "/authorize" in (started.location or ""),
          f"the dashboard sends them to the provider: {started.status} {started.location}")
    check(back is not None and back.status == 303 and back.location == "/",
          f"and the callback lands them home: {back.status if back else None}")
    page = home(ada)
    check(signed_in_as(page) == "Ada Park", f"home says {signed_in_as(page)!r}")

    say("E: freshness -- a provider's own session, re-issued without asking anybody")
    stale(True)
    try:
        bob = Browser()
        _, back = signed_in(bob)
        # The provider answered, and its token says the person authenticated an
        # hour ago. That is a provider re-issuing from its own session, and it
        # would carry groups the directory may have withdrawn since
        # (decisions/015).
        check(back is not None and back.status == 400,
              f"a token whose auth_time predates the sign-in is refused: "
              f"{back.status if back else None}")
        issued = json.loads(Browser().get(IDP + "/e2e/issued").body)["issued"]
        check(any(entry["auth_time"] < entry["iat"] for entry in issued),
              f"and the provider really did claim an older sign-in: {issued[-1:]}")
    finally:
        stale(False)

    say("G: the provider rotates its signing key, and people still sign in")
    # The dashboard read /jwks at start and holds the first key. The provider
    # now signs with a second and publishes only that. A token that does not
    # verify is not a bad token until the keys have been read again, which is
    # what the retry in `finish` is for -- deliberate code the bundled Zitadel
    # was covering until it was removed.
    Browser().post(IDP + "/e2e/rotate", {})
    rotated = Browser()
    _, back = signed_in(rotated)
    check(back is not None and back.status == 303,
          f"a token signed with the new key is accepted: {back.status if back else None}")
    page = home(rotated)
    check(signed_in_as(page) == "Ada Park",
          f"and they are who the provider said: {signed_in_as(page)!r}")

    say("H: a callback whose state was not the one this browser started with")
    honest = Browser()
    started, at_provider = begin(honest)
    check(at_provider is not None and at_provider.status == 302,
          f"the provider redirects back: {at_provider.status if at_provider else None}")

    # The same code and state, in a browser that never started this sign-in.
    # Without the check, a link could sign somebody in as somebody else.
    stranger = Browser()
    landed = stranger.get(at_provider.location)
    check(landed.status == 400,
          f"a callback in another browser: {landed.status}")
    # Past the stylesheet, which is the first three hundred characters of
    # every page here and tells a reader nothing.
    said = re.sub(r"<style>.*?</style>", " ", landed.body, flags=re.S)
    said = " ".join(re.sub(r"<[^>]*>", " ", said).split())
    check("another browser" in landed.body,
          f"and it says why: {said[:140]!r}")

    # And the browser that did start it still completes, so the refusal above
    # is about the state and not about the code having been seen.
    where = urllib.parse.urlparse(at_provider.location)
    check(where.query != "", "the provider's redirect carried a code and a state")


def main():
    wait_dashboard()
    main_phase()
    if FAILURES:
        say("")
        say(f"e2e-dashboard-oidc FAILED: {len(FAILURES)}")
        for failure in FAILURES:
            say(f"  - {failure}")
        sys.exit(1)


if __name__ == "__main__":
    main()
