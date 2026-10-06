"""`make e2e-access-per-role`: a person's access to a plugin is granted per
role, end to end on the plugin harness (contract v15; plans/access-is-granted-
per-role, "What the tests prove", the e2e; decisions/033).

Core's stand-in launched holding `custody` and `operations` (the plan's Q5),
registering as a plugin built before v15, so its role-less pages serve both
(step 3). Four people beside the harness's admin, who holds All plugins
(admin): Ada (write on operations, read on custody), Ben (write on custody,
read on operations), Cat (admin on custody alone) and Dan (admin on
operations alone). Each step drives the stand-in's page as a person does:

1. Write on one role, read on the other. In Open, Ada's opening balance
   (operations') is admitted and the holdings statement the stand-in sends
   for her (custody's) is refused by the sidecar, naming custody and her
   read on it; Ben is admitted the statement and refused the opening balance
   naming operations; under View neither is sent.
2. Admin per role. Cat links the plugin's external account under Manage;
   Dan is refused the link naming custody; each is refused setting a value
   that serves both roles; the harness's admin, All plugins (admin), does
   both.
3. Registration. Built before v15, the two-role plugin registers, and its
   pages serve both roles: Cat, administering custody alone, opens its
   Account links page under Manage.
7. The sidecar logs each act sent for a person with the roles it was
   admitted under.

The single-role plugins' runs -- harness-check, e2e-activity, e2e-tickets,
e2e-plugin-page, e2e-dashboard-accounts -- pass unmodified on v15 beside
this (step 7 of the plan). Steps 4 to 6 (a role added at launch, a
delegation narrowed by role, a tool refused by role) are held by core's
unit tests: the stand-in declares no tool.

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import re
import subprocess
import sys
import time

PROJECT = "meridian-core-access-per-role"
PEOPLE = "ada=Ada Park,ben=Ben Ito,cat=Cat Ruiz,dan=Dan Oyelaran"
LOG = ".e2e-access-per-role.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/access-per-role/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/plugin-page"))
ACCOUNT = "Both Brokerage"


class Failed(Exception):
    pass


def log(text):
    with open(LOG, "a", encoding="utf-8") as kept:
        kept.write(text + "\n")


def compose(*args, check=True):
    done = subprocess.run(COMPOSE + list(args), env=ENV, capture_output=True, text=True,
                          timeout=600)
    log(f"$ compose {' '.join(args)[:300]}\n{done.stdout}{done.stderr}")
    if check and done.returncode != 0:
        raise Failed(f"compose {' '.join(args)[:200]}: {done.stderr.strip()[-600:]}")
    return done


def runner(*args, person=None, check=True):
    extra = ["--as", person] if person else []
    done = compose("run", "--rm", "-T", "runner", *args, *extra, check=check)
    return done if not check else done.stdout


def said(text):
    """A page's JSON body, after the status line the runner prints."""
    return json.loads(text.split("\n", 1)[1] if "\n" in text else text)


def post(path, person=None, level="write", **fields):
    return said(runner("form", "--level", level, "--page", "/", "--post", path,
                       *[f"{k}={v}" for k, v in fields.items()], person=person))


def must(condition, why):
    if not condition:
        raise Failed(why)


def until(what, attempt, seconds=60):
    deadline = time.time() + seconds
    while True:
        found = attempt()
        if found:
            return found
        if time.time() > deadline:
            raise Failed(f"{what} within {seconds}s")
        time.sleep(1)


def refused_naming(done, *words):
    detail = done.get("detail", "")
    return done.get("ok") is False and done.get("code") == "PERMISSION_DENIED" and all(
        word in detail for word in words)


def run():
    started = time.time()
    compose("down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    runner("ready")

    # The harness's admin, All plugins (admin), links the account under
    # Manage and completes the fund's record the opening balance names.
    account = runner("account", ACCOUNT).strip()
    runner("form", "--level", "admin", "--page", "/admin/accounts", "--post", "/admin/accounts/link",
           "--from-page", "offered", "external_account_id=ext-e2e", f"account_id={account}",
           "--expect", f"Linked ext-e2e to {account}")
    runner("instrument", "--identifier", "symbol (stand-in): MMF", "asset_class=fund",
           "currency=USD", "source=the stand-in statement", "--seconds", "120")
    recorded = post("/activity", level="admin")
    must(recorded.get("ok"), f"the reinvestment the opening balance names: {recorded}")

    # The grants, a row of the Access editor each.
    for person, role, level in [("ada", "operations", "write"), ("ada", "custody", "read"),
                                ("ben", "custody", "write"), ("ben", "operations", "read"),
                                ("cat", "custody", "admin"), ("dan", "operations", "admin")]:
        runner("grant", "--to", person, "--role", role, "--level", level)

    # 1. Write on one role, read on the other.
    opened = until("Ada's opening balance", lambda: (lambda d: d if d.get("ok") else None)(
        post("/open", person="ada", account=account)), seconds=60)
    must(opened.get("entry_id"), f"step 1, Ada's opening balance: {opened}")
    statement = post("/write", person="ada")
    must(refused_naming(statement, "custody's", "Ada Park holds read on custody"),
         f"step 1, Ada's statement is refused naming custody and her read on it: {statement}")
    written = post("/write", person="ben")
    must(written.get("ok") and written.get("holding_id"), f"step 1, Ben's statement: {written}")
    balance = post("/open", person="ben", account=account)
    must(refused_naming(balance, "operations'", "Ben Ito holds read on operations"),
         f"step 1, Ben's opening balance is refused naming operations: {balance}")
    viewed = post("/write", person="ben", level="read")
    must(viewed.get("ok") is False and "View (read)" in viewed.get("detail", ""),
         f"step 1, nothing is sent under View: {viewed}")

    # 2. Admin per role: the link is custody's.
    linked = runner("form", "--level", "admin", "--page", "/admin/accounts",
                    "--post", "/admin/accounts/link", "--from-page", "offered",
                    "external_account_id=ext-e2e", f"account_id={account}", person="cat")
    must(f"Linked ext-e2e to {account}" in linked, f"step 2, Cat links under Manage: {linked[-300:]}")
    refused = runner("form", "--level", "admin", "--page", "/admin/accounts",
                     "--post", "/admin/accounts/link", "--from-page", "offered",
                     "external_account_id=ext-e2e", f"account_id={account}", person="dan")
    must("The sidecar refused this: PERMISSION_DENIED" in refused
         and ("LinkExternalAccount is custody&#x27;s" in refused
              or "LinkExternalAccount is custody's" in refused),
         f"step 2, Dan is refused the link naming custody: {refused[-400:]}")
    for person in ("cat", "dan"):
        done = runner("settings", "poll_minutes=20", person=person, check=False)
        must(done.returncode != 0 and "serves custody and operations" in done.stdout + done.stderr,
             f"step 2, {person} is refused a setting serving both roles: "
             f"{(done.stdout + done.stderr)[-400:]}")
    runner("settings", "poll_minutes=20")

    # 3. Built before v15, its role-less pages serve both roles.
    page = runner("page", "--level", "admin", "/admin/accounts", person="cat")
    must(page.startswith("200"), f"step 3, Cat opens Account links under Manage: {page[:200]}")

    # 7. Each act sent for a person is logged with its roles.
    logs = re.sub(r"\x1b\[[0-9;]*m", "", compose("logs", "--no-color", check=False).stdout)
    acts = [line for line in logs.splitlines() if "sent for a person" in line]
    for topic, role in [("record-opening-balance", "operations"), ("record-statement", "custody"),
                        ("link-external-account", "custody")]:
        must(any(topic in line and f'roles="{role}"' in line for line in acts),
             f"step 7: no {topic} sent for a person was logged with the role {role}")

    took = int(time.time() - started)
    print(f"e2e-access-per-role OK in {took}s: on the plugin harness, a plugin holding custody and "
          "operations, built before v15, registers with its pages serving both; a person writing "
          "operations and reading custody records an opening balance and is refused a holdings "
          "statement by the sidecar, naming custody and her read on it, and one writing custody "
          "the reverse, naming operations; nothing is sent under View; an admin of custody alone "
          "links its account under Manage and an admin of operations alone is refused the link "
          "naming custody; neither sets a value serving both roles, which All plugins (admin) "
          "does; and each act is logged with the roles it was admitted under")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError, IndexError) as failed:
        compose("logs", "--no-color", check=False)
        compose("down", "-v", "--remove-orphans", check=False)
        print(f"e2e-access-per-role FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
