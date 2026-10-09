"""`make e2e-archive`: an edge plugin's older records move to the archive, end
to end on the plugin harness (contract v16;
plans/an-edge-plugins-older-records-move-to-the-archive, "What the tests
prove"; spec/an-edge-plugins-older-records-move-to-the-archive).

Core's stand-in twice, each a custody plugin built at v16 declaring two kinds
of raw record, activity and responses, with the window settings the SDK
declares for each (e2e/plugin-page/archiving.py): `custody`, given the
harness's archive, and `keeper`, given none. The harness's admin, a
deployment admin, granted write on custody's role. Each step drives the
dashboard's pages as a person does, or the stand-in's own page, where its
helper does what an SDK's does:

1. The Summary of a plugin at the edge draws its raw records: per kind what
   storage holds, from its heartbeat, and no archive yet, records past their
   window kept.
2. An archive allowed on its Manage page, with a bound, and recorded; past
   its window `archived` is then a choice the conductor accepts.
3. A hold of 2,190 days over custody set on the deployment's Settings; a
   window set below it is refused naming the setting and the hold; a
   write-once hold is refused, since the harness's archive is local and
   cannot lock.
4. A unit past its window archived by the plugin as itself: in storage no
   more, in the archive, its move recorded naming the window, and the
   Summary drawing the archive's span and the bytes the unit uses of it
   against the bound (StoredSpan.bytes); a row's raw record resolves to
   "archived, restorable".
5. A restore asked for by a person with write, through the plugin's own
   route: read back from the restore area, recorded for that person; then
   returned after the restore period, recorded naming its rule.
6. A deletion of a unit received inside the hold refused by the sidecar,
   REFUSAL_REASON_WITHIN_HOLD, the unit kept; a window's deletion of the
   archived unit refused, an admin's act; nothing recorded for either.
7. A plugin allowed no archive keeps its records: a move to the archive it
   reports anyway is refused by the conductor, naming record_kind.
8. `store moves` prints every move, hold and archive change the conductor
   recorded, each its own record, and nothing else.

The single-role plugins' runs -- harness-check, e2e-activity, e2e-tickets,
e2e-access-per-role -- pass unmodified beside this: a plugin built before
v16 declares no kinds and keeps today's behaviour.

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import re
import subprocess
import sys
import time

PROJECT = "meridian-core-archive"
LOG = ".e2e-archive.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/archive/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/plugin-page"))
ARCHIVED = "activity/ACC-1/2019-03"
RECENT = "activity/ACC-1/recent"


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


def runner(*args, instance=None, check=True):
    extra = ["--instance", instance] if instance else []
    done = compose("run", "--rm", "-T", "runner", *args, *extra, check=check)
    return done if not check else done.stdout


def said(text):
    """A page's JSON body, after the status line the runner prints."""
    return json.loads(text.split("\n", 1)[1] if "\n" in text else text)


def post(path, level="admin", instance=None, **fields):
    return said(runner("form", "--level", level, "--page", "/", "--post", path,
                       *[f"{k}={v}" for k, v in fields.items()], instance=instance))


def get(path, level="admin", instance=None):
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


def summary(until_said, seconds=60):
    return runner("summary", "--until", until_said, "--seconds", str(seconds))


def run():
    started = time.time()
    compose("down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    runner("ready")
    runner("ready", instance="keeper")
    runner("grant", "--level", "write", "--role", "custody")

    # 1. The Summary draws its raw records, storage's from its heartbeat.
    page = summary("374, 2010-01-01 to ")
    must("No archive allowed: records past their window are kept." in page,
         "step 1, the Summary says no archive is allowed and records past their window are kept")
    must('data-kind="responses"' in page, "step 1, one line a kind")

    # 2. An archive allowed with a bound; archived then a choice.
    allowed = runner("archive", "allow", "--bound-gib", "50")
    must("Archive allowed: 0 bytes of at most 50 GiB used." in allowed,
         f"step 2, the archive allowed, none of its bound used: {allowed}")
    runner("settings", "activity_past_window=archived")

    # 3. The hold; a window below it refused; write-once refused here.
    runner("hold", "2190", "--role", "custody")
    runner("settings", "activity_window_days=30", "--expect-refused",
           "activity_window_days: 30 is below the hold of 2,190 days")
    runner("hold", "2190", "--role", "custody", "--write-once", "--expect-refused", "cannot lock")

    # 4. A unit past its window archived, as the plugin itself.
    moved = post("/archive", unit=ARCHIVED)
    must(moved.get("ok"), f"step 4, the unit archived: {moved}")
    listed = get("/archive/list")
    must(listed["index"][ARCHIVED]["where"] == "archive", f"step 4, in storage no more: {listed}")
    page = summary("214, 2019-03-01 to 2019-03-31")
    must('data-move="Archived"' in page and "activity_window_days 2555" in page,
         "step 4, the Summary draws the move, naming the window that made it")
    until("step 4, storage's count no longer holding the unit",
          lambda: "160, 2010-01-01 to " in summary("Raw records"))
    until("step 4, the bytes the unit uses of the archive, against the bound",
          lambda: re.search(r'data-used="[1-9][0-9]*"', summary("Raw records"))
          and "KiB of at most 50 GiB used." in summary("Raw records"))
    resolved = get(f"/record?key={ARCHIVED}%237", level="read")
    must(resolved["resolves"] == "archived, restorable",
         f"step 4, a row's raw record resolves to archived, restorable: {resolved}")

    # 5. A restore for a person with write, read back; then returned.
    restored = post("/archive/restore", level="write", kind="activity", unit=ARCHIVED)
    must(restored.get("ok"), f"step 5, the restore recorded: {restored}")
    back = get(f"/archive/read?unit={ARCHIVED}", level="write")
    must(back.get("records") == 214, f"step 5, the unit read back: {back}")
    page = summary('data-move="Restored"')
    must("Harness Admin" in page, "step 5, the restore recorded for the person who asked")
    returned = post("/archive/return", unit=ARCHIVED)
    must(returned.get("ok"), f"step 5, the return recorded: {returned}")
    summary("restore period 7 days")

    # 6. Deletions the rules refuse, nothing recorded for either.
    inside = post("/archive/delete", unit=RECENT)
    must(not inside.get("ok") and inside.get("code") == "FAILED_PRECONDITION"
         and inside.get("reason") == "REFUSAL_REASON_WITHIN_HOLD",
         f"step 6, a deletion inside the hold refused with its code: {inside}")
    must(get("/archive/list")["index"][RECENT]["where"] == "storage", "step 6, the unit kept")
    by_window = post("/archive/delete", unit=ARCHIVED)
    must(not by_window.get("ok") and by_window.get("code") == "PERMISSION_DENIED",
         f"step 6, a window's deletion of an archived unit refused: {by_window}")

    # 7. A plugin allowed no archive keeps its records.
    kept = post("/archive", instance="keeper", unit=ARCHIVED)
    must(not kept.get("ok") and "kept" in kept.get("detail", ""),
         f"step 7, with no archive its records past their window are kept: {kept}")
    forced = post("/archive", instance="keeper", unit=ARCHIVED, force="1")
    must(not forced.get("ok") and forced.get("code") == "INVALID_ARGUMENT"
         and "allowed no archive" in forced.get("detail", ""),
         f"step 7, a move to an archive it was not allowed refused naming record_kind: {forced}")

    # 8. What the conductor recorded, and nothing else.
    store = compose("run", "--rm", "-T", "store", "moves").stdout.split("\n")
    moves = [line for line in store if line.startswith("move|")]
    must(len(moves) == 3, f"step 8, three moves recorded: {moves}")
    must(any(line.startswith(f"move|custody|activity|{ARCHIVED}|archived|214|") and
             line.endswith("|activity_window_days 2555|") for line in moves),
         f"step 8, the archiving names its window and no person: {moves}")
    must(any(f"|{ARCHIVED}|restored|" in line and line.endswith("|local|harness") for line in moves),
         f"step 8, the restore names the person: {moves}")
    holds = [line for line in store if line.startswith("hold|")]
    # Set in a browser: no delegation, no client and no note (contract v17).
    must(holds == ["hold|custody|2190|false|local|harness|||"], f"step 8, one hold recorded: {holds}")
    archives = [line for line in store if line.startswith("archive|")]
    must(archives == ["archive|custody|true|53687091200|local|harness|||"],
         f"step 8, one archive allowed: {archives}")

    took = int(time.time() - started)
    print(f"e2e-archive OK in {took}s: on the plugin harness, a custody plugin built at v16 shows "
          "per kind what its storage holds on its Summary; a deployment admin allows it an archive "
          "with a bound and sets a hold, a window below the hold is refused naming the setting and "
          "a write-once hold refused on a local archive; a unit past its window is archived by the "
          "plugin, in storage no more, its move recorded naming the window, the archive's span and the bytes it uses against the bound "
          "drawn, and a row's record resolves to archived, restorable; a person with write restores "
          "it and reads it back, the restore recorded for them, and its return recorded; a deletion "
          "inside the hold is refused with its code and a window's deletion of an archived unit "
          "refused, both kept; a plugin allowed no archive keeps its records; and store moves "
          "prints three moves, one hold and one archive")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError, IndexError) as failed:
        compose("logs", "--no-color", check=False)
        compose("down", "-v", "--remove-orphans", check=False)
        print(f"e2e-archive FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
