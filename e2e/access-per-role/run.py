"""`make e2e-access-per-role`: a person's access to a plugin is granted per
role, end to end on the plugin harness (contract v15; plans/access-is-granted-
per-role, "What the tests prove", the e2e; decisions/033).

Core's stand-in launched twice holding `custody` and `operations` (the
plan's Q5): `both`, registering as a plugin built before v15, so its
role-less pages serve both (step 3); and `tools`, registering as one built at
v15, its pages naming both roles and three MCP tools each naming its own
(steps 5 and 6). Four people beside the harness's admin, who holds All
plugins (admin): Ada (write on operations, read on custody), Ben (write on
custody, read on operations), Cat (admin on custody alone) and Dan (admin on
operations alone), on each plugin. Each step drives the stand-in's page as a
person does, or an agent's client through the deployment's MCP surface:

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
   admitted under (read before step 4 launches `both` again).
4. A role added at launch. `both` launched again holding a third role, oms:
   its sidecar names the three when it admits the plugin, nobody's session
   carries an entry on oms -- Ada's and Ben's as before, Cat's and Dan's
   under Manage -- and only All plugins (admin) holds it, the admin's session
   under Manage carrying admin on it.
5. A delegation narrowed by role. Ada's agent, narrowed to (tools,
   operations, write), is listed open_balance and records an opening
   balance through it; it is not listed record_statement, and a call to it
   by name is refused naming custody. Cat's agent, covering everything, is
   listed open_balance only once Cat is granted write on operations after
   the delegation was made.
6. An MCP tool refused by role. record_statement, custody's at write, is
   listed to Ben's agent and not to Ada's, which covers everything, and her
   call to it by name is refused naming custody and her read on it;
   statement_from_operations, operations' at write, is listed to her, and the
   custody statement its route sends for her is refused by the sidecar,
   naming custody and her read on it: the sidecar is the last word on a
   tool's path as on a page's.

The single-role plugins' runs -- harness-check, e2e-activity, e2e-tickets,
e2e-plugin-page, e2e-dashboard-accounts -- pass unmodified on v15 beside
this (step 7 of the plan).

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
# Step 4: `both`'s sidecar holding a third role.
THIRD_ROLE = ["-f", "e2e/access-per-role/third-role.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/plugin-page"))
ACCOUNT = "Both Brokerage"


class Failed(Exception):
    pass


def log(text):
    with open(LOG, "a", encoding="utf-8") as kept:
        kept.write(text + "\n")


def compose(*args, check=True, files=()):
    done = subprocess.run(COMPOSE + list(files) + list(args), env=ENV, capture_output=True,
                          text=True, timeout=600)
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


def claims(person=None, level="write", instance=None):
    """The claims the stand-in was handed for a session opened at `level`."""
    extra = ["--instance", instance] if instance else []
    return said(runner("page", "--level", level, "/", *extra, person=person))["caller"]


def mcp(*args, person, client, check=True):
    """The runner's MCP client for `person`, as `client`: its output, and
    whether it exited 0."""
    done = runner("mcp", *args, "--client", client, person=person, check=False)
    if check and done.returncode != 0:
        raise Failed(f"mcp {' '.join(args)[:120]} as {person}: {(done.stdout + done.stderr)[-500:]}")
    return done.stdout + done.stderr, done.returncode == 0


def listed(person, client):
    return mcp("list", person=person, client=client)[0].split()


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

    # 7. Each act sent for a person is logged with its roles: read now,
    # before step 4 launches `both`'s sidecar again.
    logs = re.sub(r"\x1b\[[0-9;]*m", "", compose("logs", "--no-color", check=False).stdout)
    acts = [line for line in logs.splitlines() if "sent for a person" in line]
    for topic, role in [("record-opening-balance", "operations"), ("record-statement", "custody"),
                        ("link-external-account", "custody")]:
        must(any(topic in line and f'roles="{role}"' in line for line in acts),
             f"step 7: no {topic} sent for a person was logged with the role {role}")

    # 4. A role added at launch: nobody but All plugins (admin) holds it.
    compose("up", "-d", "--force-recreate", "sidecar-both", "both", files=THIRD_ROLE)
    runner("ready")
    admitted = until("both's sidecar did not admit it holding three roles", lambda: [
        line for line in re.sub(r"\x1b\[[0-9;]*m", "", compose(
            "logs", "--no-color", "sidecar-both", check=False, files=THIRD_ROLE).stdout).splitlines()
        if "admitted" in line and 'roles="custody,operations,oms"' in line], seconds=90)
    must(admitted, "step 4: the launch names its three roles")
    admin = until("the admin's session under Manage carried no entry on oms",
                  lambda: (lambda c: c if c["roles"].get("oms") else None)(claims(level="admin")))
    must(admin["roles"] == {"custody": "ACCESS_LEVEL_ADMIN", "operations": "ACCESS_LEVEL_ADMIN",
                            "oms": "ACCESS_LEVEL_ADMIN"},
         f"step 4, All plugins (admin) holds every role, oms included: {admin['roles']}")
    for person, level, held in [
            ("ada", "write", {"operations": "ACCESS_LEVEL_WRITE", "custody": "ACCESS_LEVEL_READ"}),
            ("ben", "write", {"custody": "ACCESS_LEVEL_WRITE", "operations": "ACCESS_LEVEL_READ"}),
            ("cat", "admin", {"custody": "ACCESS_LEVEL_ADMIN"}),
            ("dan", "admin", {"operations": "ACCESS_LEVEL_ADMIN"})]:
        roles = claims(person=person, level=level)["roles"]
        must(roles == held, f"step 4, {person}'s session carries nothing on oms: {roles}")

    # 5 and 6 on `tools`, built at v15: its external account linked to an
    # account of its own, the same grants, a row each.
    runner("ready", "--instance", "tools")
    second = runner("account", "Tools Brokerage").strip()
    runner("form", "--level", "admin", "--page", "/admin/accounts", "--post", "/admin/accounts/link",
           "--from-page", "offered", "external_account_id=ext-e2e", f"account_id={second}",
           "--expect", f"Linked ext-e2e to {second}", "--instance", "tools")
    for person, role, level in [("ada", "operations", "write"), ("ada", "custody", "read"),
                                ("ben", "custody", "write"), ("ben", "operations", "read")]:
        runner("grant", "--to", person, "--role", role, "--level", level, "--instance", "tools")

    # 5. A delegation narrowed by role.
    mcp("connect", "--covers", "tools:operations:write", person="ada", client="narrowed")
    tools = until("Ada's narrowed agent was not listed open_balance", lambda: (
        lambda names: names if "tools__open_balance" in names else None)(listed("ada", "narrowed")))
    must("tools__record_statement" not in tools,
         f"step 5, a delegation narrowed to operations lists no custody tool: {tools}")
    opened, _ = mcp("call", "tools__open_balance",
                    json.dumps({"account": second, "instrument_from": account}),
                    "--expect-outcome", "made", person="ada", client="narrowed")
    must('"entry_id"' in opened, f"step 5, the opening balance through the delegation: {opened}")
    answer, _ = mcp("call", "tools__record_statement", "{}", person="ada", client="narrowed",
                    check=False)
    must("No tool tools__record_statement is listed to this delegation: it serves custody" in answer,
         f"step 5, the statement is refused by role through the narrowed delegation: {answer[-400:]}")
    mcp("connect", "--covers", "everything", person="cat", client="everything")
    must("tools__open_balance" not in listed("cat", "everything"),
         "step 5, Cat holds nothing on tools' operations yet")
    runner("grant", "--to", "cat", "--role", "operations", "--level", "write", "--instance", "tools")
    until("a delegation covering everything did not reach a role granted after it",
          lambda: "tools__open_balance" in listed("cat", "everything"))

    # 6. An MCP tool refused by role.
    mcp("connect", "--covers", "tools:custody:write", person="ben", client="custody")
    until("Ben's agent was not listed record_statement",
          lambda: "tools__record_statement" in listed("ben", "custody"))
    mcp("connect", "--covers", "everything", person="ada", client="everything")
    hers = until("Ada's agent was not listed open_balance", lambda: (
        lambda names: names if "tools__open_balance" in names else None)(listed("ada", "everything")))
    must("tools__record_statement" not in hers and "tools__statement_from_operations" in hers,
         f"step 6, record_statement is not listed to the operations writer: {hers}")
    answer, _ = mcp("call", "tools__record_statement", "{}", person="ada", client="everything",
                    check=False)
    must("it serves custody at write, and through it the person holds read on custody" in answer,
         f"step 6, her call by name is refused naming custody and her read on it: {answer[-400:]}")
    sent, _ = mcp("call", "tools__statement_from_operations", "{}", "--expect-outcome", "refused",
                  person="ada", client="everything")
    must("custody's" in sent and "Ada Park holds read on custody" in sent,
         f"step 6, the sidecar refuses the custody command an operations tool sends: {sent[-400:]}")

    took = int(time.time() - started)
    print(f"e2e-access-per-role OK in {took}s: on the plugin harness, a plugin holding custody and "
          "operations, built before v15, registers with its pages serving both; a person writing "
          "operations and reading custody records an opening balance and is refused a holdings "
          "statement by the sidecar, naming custody and her read on it, and one writing custody "
          "the reverse, naming operations; nothing is sent under View; an admin of custody alone "
          "links its account under Manage and an admin of operations alone is refused the link "
          "naming custody; neither sets a value serving both roles, which All plugins (admin) "
          "does; each act is logged with the roles it was admitted under; launched again holding "
          "a third role, nobody but All plugins (admin) holds it; an agent narrowed to one role "
          "acts through that role's tool and is refused the other's, and one covering everything "
          "reaches a role granted after it; and a tool is listed by its role, refused by name "
          "otherwise, and the sidecar refuses a command a tool's route sends of a role not "
          "written")


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
