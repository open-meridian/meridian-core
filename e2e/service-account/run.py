"""`make e2e-service-account`: a service account runs a plugin's autonomous
work, end to end on the plugin harness (contract v21; plans/a-service-
account-runs-a-plugins-autonomous-work, "What the tests prove"; W4.15,
W6.32 to W6.34, W9.7).

Core's stand-in as a custody plugin and an operations plugin whose work runs
as its role's service account (e2e/service-account/stand-in.yaml). Beside
the harness's admin -- the deployment admin's capabilities, All plugins
(admin), and write on operations -- Cat, the account's owner, who holds
nothing until step 7 grants her write on operations. As for `make
e2e-activity`, the custody side's statement and reinvestment stand behind a
break the operations side records on the fund's units, open in the book.

1. The admin creates "The sweep" on Settings, Service accounts, owned by Cat:
   prepare only, active, granted nothing; `dashboard__list_service_accounts`
   reads its row. Cat reads her own row on her Service accounts page.
2. With no account chosen, the role's work is refused, saying so; a role's
   admin cannot choose an account not granted on the role, nor a level
   above its grant.
3. Granted write on operations and chosen for it, set to prepare only: the
   work's close of the cleared break is prepared, not made -- the book's
   break still open -- shown to the admin, who may make it, and to Cat, its
   owner; `dashboard__read_prepared` names the account as who prepared it.
   The admin discards it.
4. Set to act, at may make: the close is made, the break closed as
   cleared, the book's entry naming the service account and its owner,
   never a person.
5. The levels bind it: at prepare only (the plugin admin's level) an act is
   prepared though the account is set to act; at may not it is refused by
   name; an act naming no check tool is made set to act and refused set to
   prepare only; a core tool and a read are refused by name; and its act's
   route, saving its own settings with the account's assertion, is refused
   -- it administers nothing.
6. Under a second pair of eyes, set to act, it prepares; Cat, granted write
   and its owner, is refused making it; the admin makes it.
7. Cat disables it: its waiting prepared change lapses, its own record, and
   its next call is refused; Cat cannot re-enable it; the admin does.
8. Its page lists what it did -- made, prepared and refused, by name and
   outcome -- and its record, each change its own; its row's last refusal.

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import subprocess
import sys
import time

PROJECT = "meridian-core-service-account"
LOG = ".e2e-service-account.log"
PEOPLE = "cat=Cat Ruiz"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/service-account/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/plugin-page"))
ACCOUNT = "Sweep Brokerage"


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
        raise Failed(f"compose {' '.join(args)[:200]}: {(done.stdout + done.stderr).strip()[-600:]}")
    return done


def runner(*args, instance=None, person=None, check=True):
    extra = (["--instance", instance] if instance else []) + (["--as", person] if person else [])
    done = compose("run", "--rm", "-T", "runner", *args, *extra, check=check)
    return done.stdout if check else done


def store(name):
    return compose("run", "--rm", "-T", "store", name).stdout.splitlines()


def said(text):
    """A page's JSON body, after the status line the runner prints."""
    return json.loads(text.split("\n", 1)[1] if "\n" in text else text)


def post(instance, path, level="write", **fields):
    return said(runner("form", "--level", level, "--page", "/", "--post", path,
                       *[f"{k}={v}" for k, v in fields.items()], instance=instance))


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


def call(client, tool, arguments, person=None):
    done = runner("mcp", "call", tool, json.dumps(arguments), "--client", client,
                  person=person, check=False)
    try:
        return json.loads(done.stdout)
    except ValueError:
        raise Failed(f"{tool} through {client}: no answer: {(done.stdout + done.stderr)[-400:]}")


def sa(*args, person=None, check=True):
    return runner("service-account", *args, instance="operations", person=person, check=check)


def breaks():
    # break|<account>|<instrument>|<side>|<category>|<state>|<first>|<last>|<recorded by>|
    # <cause>|<resolution>: its state the sixth, how it was resolved the last.
    return [line.split("|") for line in store("book") if line.startswith("break|")]


def run():
    started = time.time()
    compose("--profile", "runner", "down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    for instance in ["custody", "operations"]:
        runner("ready", instance=instance)

    # The break the sweep closes, as `make e2e-activity` makes it.
    account = runner("account", ACCOUNT).strip()
    runner("form", "--level", "admin", "--page", "/admin/accounts", "--post", "/admin/accounts/link",
           "--from-page", "offered", "external_account_id=ext-e2e", f"account_id={account}",
           "--expect", f"Linked ext-e2e to {account}", instance="custody")
    until("the statement was not complete",
          lambda: any(line.startswith(f"statement|{ACCOUNT}|stand-in|") and "|complete|" in line
                      for line in store("street")))
    runner("instrument", "--identifier", "symbol (stand-in): MMF", "asset_class=fund",
           "currency=USD", "source=the stand-in statement")
    runner("grant", "--level", "write", instance="operations")
    activity = post("custody", "/activity", level="admin")
    must(activity.get("ok"), f"the reinvestment: {activity}")
    opened = post("operations", "/open", account=account)
    must(opened.get("ok"), f"the opening balance: {opened}")
    recorded = post("operations", "/break", account=account)
    must(recorded.get("ok"), f"the break: {recorded}")
    break_id = recorded["break_id"]
    must([b[5] for b in breaks()] == ["1"], f"the break open: {breaks()}")
    sweep = dict(account_id=account, break_id=break_id, statement_id="STMT-e2e-next",
                 as_of_date="2026-10-01", reason="The difference was gone at the next statement")

    # 1. Created, prepare only, granted nothing; read through its tool and by
    # its owner.
    the_sweep = sa("create", "The sweep", "--owner", "cat",
                   "--purpose", "Closes breaks whose difference has gone").strip()
    must(the_sweep.startswith("SA-"), f"step 1, an SA- ID: {the_sweep}")
    listed = sa("list")
    must(f"{the_sweep}\tThe sweep\tprepare_only\tactive" in listed, f"step 1, its row: {listed}")
    # Cat reads her own row on her page, which signs her in, so the
    # deployment knows her name.
    runner("core-page", "/service-accounts", "--expect", f'data-id="{the_sweep}"', person="cat")
    runner("mcp", "connect", "--covers", "deployment_admin", "--covers", "operations:operations:write",
           "--client", "Admin's agent")
    rows = call("Admin's agent", "dashboard__list_service_accounts", {})
    row = [r for r in rows["data"]["service_accounts"] if r["service_account_id"] == the_sweep]
    must(row and row[0]["setting"] == "prepare_only" and row[0]["owner"] == "Cat Ruiz"
         and row[0]["state"] == "active", f"step 1, read through its tool: {rows}")

    # 2. None chosen: refused, saying so; never an account not granted, nor
    # above its grant.
    refused = post("operations", "/sweep", tool="close_cleared_breaks", **sweep)
    must(not refused["ok"] and refused["code"] == "FAILED_PRECONDITION" and refused["fields"] == ["role"]
         and "runs as no service account" in refused["detail"], f"step 2, none chosen: {refused}")
    sa("runs-as", the_sweep, "--role", "operations", "--expect-refused", "is not granted on operations")
    sa("grant", the_sweep, "--role", "operations", "--level", "read")
    sa("runs-as", the_sweep, "--role", "operations", "--expect-refused", "runs at read at most")

    # 3. Granted write, chosen, prepare only: prepared, not made.
    sa("grant", the_sweep, "--role", "operations", "--level", "write")
    sa("runs-as", the_sweep, "--role", "operations")
    prepared = post("operations", "/sweep", tool="close_cleared_breaks", **sweep)
    must(prepared["ok"] and prepared["outcome"] == "prepared"
         and prepared["prepared_call_id"].startswith("PC-"), f"step 3, prepared: {prepared}")
    first = prepared["prepared_call_id"]
    must([b[5] for b in breaks()] == ["1"], f"step 3, nothing made: {breaks()}")
    runner("prepared", "--until", first)
    runner("prepared", "--until", first, person="cat")
    shown = call("Admin's agent", "dashboard__read_prepared", {"prepared_call_id": first})["data"]
    must(shown["prepared_by"] == the_sweep
         and shown["prepared_by_display_name"] == "The sweep, a service account owned by Cat Ruiz"
         and shown["state"] == "PREPARED_CALL_STATE_PREPARED",
         f"step 3, named as who prepared it: {shown}")
    runner("prepared-act", "discard", first, "--note", "Not this one.")

    # 4. Set to act, at may make: made, the entry naming the account and its
    # owner.
    sa("set", the_sweep, "--setting", "act")
    made = post("operations", "/sweep", tool="close_cleared_breaks", **sweep)
    must(made["ok"] and made["outcome"] == "made" and json.loads(made["answer"])["outcome"] == "made",
         f"step 4, made: {made}")
    closed = breaks()
    by = f"service account {the_sweep} owned by local|cat"
    must(closed[0][5] == "3" and closed[0][-1] == "cleared", f"step 4, the break closed: {closed}")
    entries = [line for line in store("book") if line.startswith(f"entry|{ACCOUNT}|break-closed|")]
    must(entries and entries[-1].endswith(f"|{by}"), f"step 4, the entry names it: {entries}")

    # 5. The levels bind it.
    runner("agents", "operations__record_sweep_note", "--level", "prepare_only", "--plugin",
           "--note", "Its notes are prepared.", instance="operations")
    held = post("operations", "/sweep", tool="record_sweep_note", note="swept")
    must(held["ok"] and held["outcome"] == "prepared", f"step 5, prepare only binds it: {held}")
    runner("agents", "operations__record_sweep_note", "--level", "may_not", "--plugin",
           "--note", "No agent notes.", instance="operations")
    refused = post("operations", "/sweep", tool="record_sweep_note", note="swept")
    must(not refused["ok"] and refused["code"] == "PERMISSION_DENIED"
         and refused["fields"] == ["tool_name"] and "may not" in refused["detail"],
         f"step 5, may not, by name: {refused}")
    made = post("operations", "/sweep", tool="note_without_check", note="no check")
    must(made["ok"] and made["outcome"] == "made", f"step 5, no check tool, set to act: {made}")
    sa("set", the_sweep, "--setting", "prepare_only")
    refused = post("operations", "/sweep", tool="note_without_check", note="no check")
    must(not refused["ok"] and refused["code"] == "FAILED_PRECONDITION"
         and "cannot be prepared" in refused["detail"], f"step 5, no check tool, prepare only: {refused}")
    sa("set", the_sweep, "--setting", "act")
    for tool, why in [("dashboard__make_prepared", "a core tool"), ("check_sweep_note", "a read")]:
        refused = post("operations", "/sweep", tool=tool, note="x")
        must(not refused["ok"] and refused["code"] == "INVALID_ARGUMENT"
             and refused["fields"] == ["tool_name"], f"step 5, {why} refused by name: {refused}")
    administering = post("operations", "/sweep", tool="save_own_settings_as_it")
    answer = json.loads(administering.get("answer") or "{}")
    must(administering["ok"] and answer.get("outcome") == "refused"
         and "administers nothing" in answer.get("detail", ""),
         f"step 5, it administers nothing: {administering}")

    # 6. Under a second pair of eyes: prepared; its owner is refused making
    # it, another person makes it.
    runner("grant", "--to", "cat", "--role", "operations", "--level", "write", instance="operations")
    runner("agents", "operations__record_sweep_note", "--level", "may_make", "--eyes", "--plugin",
           "--note", "Two people for a note.", instance="operations")
    gated = post("operations", "/sweep", tool="record_sweep_note", note="under the gate")
    must(gated["ok"] and gated["outcome"] == "prepared", f"step 6, the gate prepares it: {gated}")
    runner("prepared-act", "make", gated["prepared_call_id"], "--expect-refused", "its owner",
           person="cat")
    runner("prepared", "--until", gated["prepared_call_id"])
    runner("prepared-act", "make", gated["prepared_call_id"], "--note", "Read and agreed.")
    made = call("Admin's agent", "dashboard__read_prepared",
                {"prepared_call_id": gated["prepared_call_id"]})["data"]
    must(made["state"] == "PREPARED_CALL_STATE_MADE", f"step 6, made by the admin: {made}")

    # 7. Its owner disables it: its waiting change lapses, its next call is
    # refused; she cannot re-enable it.
    runner("agents", "operations__record_sweep_note", "--level", "prepare_only", "--plugin",
           "--note", "Prepared again.", instance="operations")
    waiting = post("operations", "/sweep", tool="record_sweep_note", note="waiting")
    must(waiting["ok"] and waiting["outcome"] == "prepared", f"step 7, waiting: {waiting}")
    sa("disable", the_sweep, person="cat")
    lapsed = call("Admin's agent", "dashboard__read_prepared",
                  {"prepared_call_id": waiting["prepared_call_id"]})["data"]
    must(lapsed["state"] == "PREPARED_CALL_STATE_LAPSED"
         and lapsed["history"][-1]["step"] == "lapsed"
         and "was disabled" in lapsed["history"][-1]["note"], f"step 7, lapsed: {lapsed}")
    refused = post("operations", "/sweep", tool="note_without_check", note="after")
    must(not refused["ok"] and refused["code"] == "PERMISSION_DENIED" and refused["fields"] == ["role"]
         and "disabled" in refused["detail"], f"step 7, its next call refused: {refused}")
    done = sa("enable", the_sweep, "--expect-refused", "nothing more", person="cat")
    must("refused" in done, f"step 7, its owner cannot re-enable it: {done}")
    sa("enable", the_sweep)
    must(f"{the_sweep}\tThe sweep\tact\tactive" in sa("list"), "step 7, re-enabled by the admin")

    # 8. What it did, and its record; its row's last refusal.
    runner("core-page", f"/service-accounts/{the_sweep}",
           "--expect", 'data-outcome="made"', "--expect", 'data-outcome="prepared"',
           "--expect", 'data-outcome="refused"', "--expect", "state: disabled",
           "--expect", "setting: act", "--expect", "operations:operations", person="cat")
    rows = call("Admin's agent", "dashboard__list_service_accounts", {})
    row = [r for r in rows["data"]["service_accounts"] if r["service_account_id"] == the_sweep][0]
    must(row["last_refusal"] and row["last_used_ns"] and row["runs_for"] == ["operations:operations"],
         f"step 8, its row: {row}")

    took = int(time.time() - started)
    print(f"e2e-service-account OK in {took}s: on the plugin harness, a deployment admin creates "
          "a service account owned by a person, prepare only and granted nothing, read through "
          "its tool and by its owner; with none chosen the role's work is refused saying so, and "
          "no account not granted, nor a level above its grant, is chosen; granted and chosen, "
          "its close of a cleared break is prepared, not made, shown to who may make it and its "
          "owner, named as who prepared it; set to act, the close is made, the book's entry "
          "naming the service account and its owner; the levels bind it -- prepare "
          "only prepares, may not refuses by name, an act with no check tool is made set to act "
          "and refused set to prepare only, a core tool and a read are refused by name, and it "
          "administers nothing; under a second pair of eyes its owner is refused making what it "
          "prepared and another person makes it; its owner disables it, its waiting change lapses "
          "and its next call is refused, and only a deployment admin re-enables it; its page "
          "lists what it did and each change to it")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError, IndexError, TypeError) as failed:
        compose("logs", "--no-color", check=False)
        compose("--profile", "runner", "down", "-v", "--remove-orphans", check=False)
        print(f"e2e-service-account FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("--profile", "runner", "down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
