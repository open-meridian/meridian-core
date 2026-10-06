"""`make e2e-activity`: the custodian's activity explains a break, end to end
on the plugin harness (contract v14; plans/the-custodians-activity-explains-a-
break, "What the tests prove").

Core's stand-in as `custody` and `operations` (e2e/harness/plugins.json); the
harness's admin links the custody plugin's account through its own page,
completes the money market fund's record and holds write on operations. Each
step drives a plugin's page as a person does, or reads `store activity`,
`store street` and `store book`:

1. The stand-in custody reports a statement holding the fund (after the
   link), a sync status needing a person to sign in, saying how far back its
   history reaches, and the fund's income reinvested.
2. Sent again, the reinvestment is answered as recorded already, and the
   street keeps it once.
3. Operations' stand-in reads the activity with its `history_from`, and the
   sync status, within its scope.
4. The person records the fund's opening balance; operations' stand-in
   records a break on its units with the reinvestment as its candidate cause,
   under income reinvested, as itself; the book moves nothing.
5. Only when the person confirms does the book move: the lot the reinvestment
   bought, the break resolved.
6. Re-resolved (contract v15; W2.15, W2.16): the stand-in custody reports a
   reinvestment under a plan's own code, which resolves to nothing, then
   re-resolves it to the fund as a person's plan-code link would, answered
   as already recorded when sent again. The activity stays as first
   recorded; its re-resolution is kept beside it, heard by operations'
   stand-in with its cause and read beside the activity.

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import subprocess
import sys
import time

PROJECT = "meridian-core-activity"
LOG = ".e2e-activity.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/harness/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/plugin-page"))
ACCOUNT = "Activity Brokerage"
FUND = "local(symbol:MMF@stand-in)"


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


def runner(*args, instance=None):
    extra = ["--instance", instance] if instance else []
    return compose("run", "--rm", "-T", "runner", *args, *extra).stdout


def store(name):
    return compose("run", "--rm", "-T", "store", name).stdout.splitlines()


def said(text):
    """A page's JSON body, after the status line the runner prints."""
    return json.loads(text.split("\n", 1)[1] if "\n" in text else text)


def post(instance, path, level="write", **fields):
    return said(runner("form", "--level", level, "--page", "/", "--post", path,
                       *[f"{k}={v}" for k, v in fields.items()], instance=instance))


def get(instance, path, level="write"):
    return said(runner("page", "--level", level, path, instance=instance))


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


def run():
    started = time.time()
    compose("down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    for instance in ["custody", "operations"]:
        runner("ready", instance=instance)

    # The account, linked through the custody plugin's own page; its read
    # woken by the link records the statement holding the fund.
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

    # 1. A sync status needing sign-in, and the reinvestment.
    reported = post("custody", "/report", level="admin")
    must(reported.get("sync") == "published", f"step 1, the sync status: {reported}")
    first = post("custody", "/activity", level="admin")
    must(first.get("ok") and first["activity_id"].startswith("ACT-")
         and not first["already_recorded"], f"step 1, the reinvestment: {first}")

    # 2. A repeat is recorded once.
    again = post("custody", "/activity", level="admin")
    must(again.get("already_recorded") and again.get("activity_id") == first["activity_id"],
         f"step 2: {again}")
    lines = until("store activity did not show the reinvestment and the sync status",
                  lambda: (lambda held: held if any(l.startswith("sync|") for l in held)
                           and any(l.startswith("activity|") for l in held) else None)(
                      store("activity")))
    activity = [line for line in lines if line.startswith("activity|")]
    must(activity == [f"activity|{ACCOUNT}|stand-in|e2e-reinvest-2026-09-30|3|{FUND}|"
                      "2026-09-30|3.27"], f"step 2, store activity: {activity}")
    must(f"sync|{ACCOUNT}|e2e|ext-e2e|3|2024-10-04" in lines, f"step 1, store activity: {lines}")

    # 3. Operations reads the activity and the sync status within its scope.
    read = get("operations", f"/activity?account={account}")
    must(read.get("ok") and read["history_from"] == "2024-10-04", f"step 3, read: {read}")
    must([a["activity_id"] for a in read["activities"]] == [first["activity_id"]]
         and read["activities"][0]["kind"] == "ACTIVITY_KIND_REINVESTMENT",
         f"step 3, the activity read: {read}")
    sync = get("operations", "/sync")
    must({"account_id": account, "external_account_id": "ext-e2e",
          "state": "SYNC_STATE_NEEDS_SIGN_IN", "history_from": "2024-10-04"} in sync["statuses"],
         f"step 3, the sync status read: {sync}")

    # 4. The opening balance, then a break explained by the reinvestment; the
    # book moves nothing.
    opened = post("operations", "/open", account=account)
    must(opened.get("ok"), f"step 4, the opening balance: {opened}")
    before = [line for line in store("book") if line.startswith("position|")]
    must(before and before[0].split("|")[4] == "500.00", f"step 4, the book: {before}")
    recorded = post("operations", "/break", account=account)
    must(recorded.get("ok") and recorded["category"] == "BREAK_CAUSE_CATEGORY_INCOME_REINVESTED"
         and recorded["activity_id"] == first["activity_id"], f"step 4, the break: {recorded}")
    book = store("book")
    breaks = [line for line in book if line.startswith("break|")]
    # break|<account>|<instrument>|<side>|<category>|<state>|...: open.
    must(len(breaks) == 1 and breaks[0].split("|")[5] == "1", f"step 4, the break kept: {breaks}")
    must([line for line in book if line.startswith("position|")] == before,
         "step 4: a break moved the book")

    # 5. The person confirms, and only then the book moves.
    confirmed = post("operations", "/confirm", account=account, break_id=recorded["break_id"])
    must(confirmed.get("ok"), f"step 5, confirmed: {confirmed}")
    book = store("book")
    after = [line for line in book if line.startswith("position|")]
    must(after and after[0].split("|")[4] == "503.27", f"step 5, the book: {after}")
    lots = [line for line in book if line.startswith("lot|")]
    must(any("|3.27|3.27|3.27 USD|2026-09-30|" in line for line in lots), f"step 5, the lot: {lots}")
    breaks = [line for line in book if line.startswith("break|")]
    must(breaks[0].split("|")[5] == "2", f"step 5, the break resolved: {breaks}")
    must([line for line in store("activity") if line.startswith("activity|")] == activity,
         "step 5: the activity changed")

    # 6. An activity under a plan's own code, re-resolved when the code is
    # linked; the first record stands, the re-resolution beside it.
    listening = post("operations", "/listen")
    must(listening.get("listening"), f"step 6, listening: {listening}")
    time.sleep(1)  # the stream open and the sidecar subscribed
    plan = post("custody", "/plan-activity", level="admin")
    must(plan.get("ok") and not plan["already_recorded"], f"step 6, the plan's activity: {plan}")
    re_resolved = post("custody", "/re-resolve", level="admin")
    must(re_resolved.get("ok") and re_resolved["activity_id"] == plan["activity_id"]
         and not re_resolved["already_recorded"], f"step 6, re-resolved: {re_resolved}")
    again = post("custody", "/re-resolve", level="admin")
    must(again.get("ok") and again["already_recorded"], f"step 6, sent again: {again}")
    lines = store("activity")
    must(f"activity|{ACCOUNT}|stand-in|e2e-oqkr-2026-09-30|3||2026-09-30|1.50" in lines,
         f"step 6, the activity as first recorded, unresolved: {lines}")
    must([line for line in lines if line.startswith("re-resolution|")]
         == [f"re-resolution|{ACCOUNT}|stand-in|e2e-oqkr-2026-09-30|1|{FUND}"],
         f"step 6, the re-resolution kept once beside it: {lines}")
    fund = read["activities"][0]["instrument_id"]
    heard = until("operations' stand-in did not hear the re-resolution",
                  lambda: (lambda said: said if said.get("re_resolutions") else None)(
                      get("operations", "/heard")))
    must(len(heard["re_resolutions"]) == 1, f"step 6, heard once: {heard}")
    one = heard["re_resolutions"][0]
    must(one["activity_id"] == plan["activity_id"] and one["account_id"] == account
         and one["instrument_id"] == fund and "plan-code links" in one["person"]
         and one["sequence"] > 0 and one["cause"] not in ("", "operations"),
         f"step 6, what operations heard: {one}")
    read = get("operations", f"/activity?account={account}")
    kept = [a for a in read["activities"] if a["external_activity_id"] == "e2e-oqkr-2026-09-30"]
    must(len(kept) == 1 and kept[0]["instrument_id"] == "", f"step 6, read as first recorded: {read}")
    must(read["re_resolutions"] == [{"activity_id": plan["activity_id"], "account_id": account,
                                     "instrument_id": fund, "person": one["person"]}],
         f"step 6, the re-resolution read beside it: {read}")

    took = int(time.time() - started)
    print(f"e2e-activity OK in {took}s: on the plugin harness, the stand-in custody reports a "
          "statement holding a money market fund, a sync status needing a person to sign in "
          "with how far back its history reaches, and the fund's income reinvested, recorded "
          "once however often it is sent; operations' stand-in reads the activity with its "
          "history_from and the sync status within its scope, records a break on the fund's "
          "units with the reinvestment as its cause under income reinvested, and the book "
          "moves nothing until the person confirms the lot it bought; an activity under a "
          "plan's own code is re-resolved to the fund once, kept beside the activity as first "
          "recorded, heard by operations and read with it")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError, IndexError) as failed:
        compose("logs", "--no-color", check=False)
        compose("down", "-v", "--remove-orphans", check=False)
        print(f"e2e-activity FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
