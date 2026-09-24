"""Signing in against an account this deployment holds itself.

Decision 018, branch three: the firm has no directory at all. First run asks
for a login and a password, the Job hashes it, and the dashboard makes the
account at its next start from what was written. Nothing redirects anywhere
and nothing crosses the bus.

This is the branch that had two defects survive a year of green tests, both of
the same shape -- a value the wizard collected and nobody read. Neither was
caught because nothing signed anybody in on this route. This does.

Phases:
  main   -- the account first run made exists, and somebody uses it
  locked -- and enough wrong passwords stop it being usable at all
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# Shared with the LDAP suite, which is self-contained for the same reason this
# is: neither may depend on the Zitadel runner, which is being deleted.
from ldap_runner import (  # noqa: E402
    FAILURES,
    Browser,
    check,
    dash,
    form_token,
    home,
    is_admin_home,
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
