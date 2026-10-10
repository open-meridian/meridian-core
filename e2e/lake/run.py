"""`make e2e-lake` and `make e2e-recorded`: the lake's 1a end to end on the
plugin harness (contract v18, W10; plans/the-lake-prices-the-book, "Done
when"), core's stand-ins on the Python SDK (e2e/lake/stand_in.py): `coin`, a
dgm serving `daily`, `ticks` and `fx` from an exchange file; `book`, a
reading plugin; and `other`, a reading plugin never entitled. Beside the
harness's admin, Ben, who holds read on `book`.

1. The datasets: each launched catalogue's datasets are listed by
   `dashboard__list_datasets`; the admin licenses `coin:daily` at the Data
   sources page, its terms one person's, and the tool then warns that two
   people hold read on a plugin entitled to it; a licence through the tool
   without a note is refused naming the note, and with one is recorded.
2. Entitled: `book` to all three datasets, the broker's configuration
   written again with them and reloaded (the runner's `entitle`).
3. Recorded and heard: `book` follows BTC and EUR's cash instrument; `coin`
   records two daily closes and a bar, a BTC price in USDC on the token's
   own instrument and an FX rate; `book` hears the latest close (heard
   latest value first per key, the second standing for the first) and the
   rate, reads the first close, and the second on its venue ID in dollars'
   cash instrument, and the USDC price on USDC's.
4. Restated: `coin` records the second day again, changed; `book` hears
   version 2, reads it, and reads version 1 as of before it.
5. Refused: `other`, not entitled, is answered not entitled; a read naming
   2026-02-30 is refused by the sidecar naming business_date.
6. Wants: `book` reads a day `coin` has not recorded, twice -- one ask,
   coalesced -- and `coin` records it against the want; a standing want on
   `ticks` is withdrawn once nobody has read it in two cadences.
7. Priority: set at the page for closes; the tool refuses a change against
   what it read before (the stale guard), and takes one against what it
   reads now.
8. `store lake` prints each row version kept, numbered with no holes, the
   restatement as version 2, the want asked once and answered, the standing
   want withdrawn, and the priority's changes.

`make e2e-recorded` runs the same with STAND_IN_EXCHANGES naming RECORDED, a
developer's recorded exchanges (plans/the-lake-prices-the-book, Q11).

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import subprocess
import sys
import time

NAME = os.environ.get("E2E_LAKE_NAME", "e2e-lake")
PROJECT = f"meridian-core-{NAME}"
PEOPLE = "ben=Ben Ito"
LOG = f".{NAME}.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/lake/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/lake"),
           STAND_IN_EXCHANGES=os.path.abspath(os.environ.get("STAND_IN_EXCHANGES",
                                                             "e2e/lake/exchanges")))
FIVE = ["dashboard__list_datasets", "dashboard__set_dataset_licence",
        "dashboard__set_dataset_entitlement", "dashboard__list_source_priorities",
        "dashboard__set_source_priority"]
DAY_1, DAY_2, DAY_0 = "2026-10-08", "2026-10-09", "2026-10-07"


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


def runner(*args, check=True):
    done = compose("run", "--rm", "-T", "runner", *args, check=check)
    return done if not check else done.stdout


def must(condition, why):
    if not condition:
        raise Failed(why)


def page(instance, level, path):
    """A stand-in's page, as JSON."""
    said = runner("page", "--instance", instance, "--level", level, path)
    status, _, body = said.partition("\n")
    must(status.strip() == "200", f"{instance} {path} answered {said[-600:]}")
    return json.loads(body)


def until(seconds, attempt, what):
    deadline = time.monotonic() + seconds
    last = None
    while True:
        try:
            answer = attempt()
        except Failed as failed:
            answer, last = None, failed
        if answer:
            return answer
        if time.monotonic() > deadline:
            raise Failed(f"{what}, in {seconds} seconds" + (f": {last}" if last else ""))
        time.sleep(2)


def call(tool, arguments, outcome):
    done = runner("mcp", "call", tool, json.dumps(arguments), check=False)
    try:
        answer = json.loads(done.stdout)
    except ValueError:
        raise Failed(f"{tool}: no answer: {(done.stdout + done.stderr)[-400:]}")
    must(answer.get("outcome") == outcome,
         f"{tool} answered {answer.get('outcome')}, not {outcome}: {done.stdout[-700:]}")
    return answer


def datasets_listed():
    return {d["dataset"]: d for d in call("dashboard__list_datasets", {}, "read")["data"]["datasets"]}


def run():
    started = time.time()
    compose("down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    for instance in ("coin", "book", "other"):
        runner("ready", "--instance", instance)
    runner("grant", "--instance", "book", "--level", "read")
    runner("grant", "--instance", "other", "--level", "read")
    runner("grant", "--instance", "book", "--level", "read", "--to", "ben")
    runner("mcp", "connect", "--covers", "deployment_admin")
    listed = runner("mcp", "list").split()
    for name in FIVE:
        must(name in listed, f"step 1, {name} is listed to the deployment admin: {listed}")

    # 1. The datasets, licensed.
    held = until(90, lambda: (lambda d: d if {"coin:daily", "coin:ticks", "coin:fx"} <= set(d) else None)(
        datasets_listed()), "step 1, the three datasets were not listed")
    daily = held["coin:daily"]
    must(daily["catalogue_entry"]["modes"] == ["pull", "push"], f"step 1, daily's modes: {daily}")
    must(daily["catalogue_entry"]["venue_id"].startswith("VEN-"), f"step 1, daily's venue: {daily}")
    must(daily["licence"]["set_by_the_deployment"] is False, f"step 1, the default licence: {daily}")
    runner("licence", "coin:daily", "kept=true", "retention_days=0", "derived_use=true",
           "display=true", "personal_use=true", "note=The exchange's retail terms.")
    refused = call("dashboard__set_dataset_licence",
                   {"dataset": "coin:fx", "kept": True, "retention_days": 3650, "derived_use": True,
                    "display": True, "personal_use": False}, "refused")
    must(any(f.get("path") == "note" for f in refused.get("fields", [])),
         f"step 1, a licence without a note is refused naming it: {refused}")
    call("dashboard__set_dataset_licence",
         {"dataset": "coin:fx", "kept": True, "retention_days": 3650, "derived_use": True,
          "display": True, "personal_use": False, "note": "Public domain; ten years kept."}, "made")

    # 2. Entitled, the broker reloaded with each.
    for dataset in ("coin:daily", "coin:ticks", "coin:fx"):
        runner("entitle", "--instance", "book", dataset, "note=The book's valuation reads it.")
    held = datasets_listed()
    must(held["coin:daily"]["licence"]["personal_use"] is True, f"step 1, licensed: {held['coin:daily']}")
    warning = held["coin:daily"].get("one_person_warning") or ""
    must("2 people" in warning, f"step 1, the one-person warning: {held['coin:daily']}")
    must(held["coin:fx"]["licence"]["retention_days"] == 3650, f"step 1, fx licensed: {held['coin:fx']}")
    must([e["instance"] for e in held["coin:ticks"]["entitlements"]] == ["book"],
         f"step 2, entitled: {held['coin:ticks']}")

    # 3. Recorded and heard.
    ids = page("coin", "admin", "/resolve")
    btc, eur, usdc = ids["btc"], ids["eur"], ids["usdc"]
    page("book", "read", f"/follow?subject={btc}&subject={eur}")
    time.sleep(3)
    recorded = page("coin", "admin", "/record")
    must(recorded == {"prices": 3, "bars": 1, "fx": 1, "restated": 0, "unchanged": 0},
         f"step 3, what coin recorded: {recorded}")

    def heard_closes():
        heard = page("book", "read", "/heard")["heard"]
        days = {h["business_date"] for h in heard if h["dataset"] == "coin:daily" and h["kind"] == "PRICE_KIND_CLOSE"}
        fx = [h for h in heard if h["dataset"] == "coin:fx"]
        return heard if DAY_2 in days and fx else None
    # Heard latest value first per key -- dataset, subjects, venue and kind --
    # so the second close stands for the first, which is read.
    heard = until(60, heard_closes, "step 3, book did not hear the latest close and the rate")
    first = page("book", "read", f"/prices?subject={btc}&date={DAY_1}&kind=close")["prices"]
    must(len(first) == 1 and first[0]["amount"] == "62431.27", f"step 3, the first close: {first}")
    rate = [h for h in heard if h["dataset"] == "coin:fx"][0]
    must(rate["amount"] == "1.0912", f"step 3, the rate as stated: {rate}")
    second = page("book", "read", f"/prices?subject={btc}&date={DAY_2}&kind=close")
    rows = second["prices"]
    must(len(rows) == 1 and rows[0]["version"] == 1 and rows[0]["amount"] == "62000.10",
         f"step 3, the second close: {second}")
    must(rows[0]["venue_id"].startswith("VEN-"), f"step 3, a price on a venue ID: {rows[0]}")
    must(rows[0]["instrument_id"] and rows[0]["instrument_id"] not in (usdc, btc),
         f"step 3, a dollar price names dollars' cash instrument: {rows[0]}")
    version_1_at = rows[0]["recorded_at_ns"]
    stable = page("book", "read", f"/prices?subject={btc}&date={DAY_2}&kind=last")["prices"]
    must(len(stable) == 1 and stable[0]["instrument_id"] == usdc and stable[0]["amount"] == "62004.90",
         f"step 3, a USDC price on USDC's cash instrument: {stable}")

    # 4. Restated.
    restated = page("coin", "admin", "/restate")
    must(restated == {"recorded": 0, "restated": 1, "unchanged": 0}, f"step 4, restated: {restated}")
    until(60, lambda: [h for h in page("book", "read", "/heard")["heard"]
                       if h["business_date"] == DAY_2 and h["version"] == 2],
          "step 4, book did not hear version 2")
    now = page("book", "read", f"/prices?subject={btc}&date={DAY_2}&kind=close")["prices"]
    must(len(now) == 1 and now[0]["version"] == 2 and now[0]["amount"] == "61990.55",
         f"step 4, the latest is version 2: {now}")
    then = page("book", "read", f"/prices?subject={btc}&date={DAY_2}&kind=close&as_of={version_1_at}")["prices"]
    must(len(then) == 1 and then[0]["version"] == 1 and then[0]["amount"] == "62000.10",
         f"step 4, as of before it, version 1: {then}")

    # 5. Refused.
    other = page("other", "read", f"/prices?subject={btc}&date={DAY_1}&dataset=coin:daily")
    must(not other["prices"] and any(u["reason"] == "UNANSWERED_REASON_NOT_ENTITLED"
                                     for u in other["unanswered"]),
         f"step 5, a reader not entitled is refused: {other}")
    invalid = page("book", "read", f"/invalid-date?subject={btc}")
    must(invalid["refused"] and "business_date" in invalid["refused"],
         f"step 5, an invalid date refused naming business_date: {invalid}")

    # 6. Wants: asked once for two reads, answered; a standing want withdrawn.
    asked = page("book", "read", f"/prices?subject={btc}&date={DAY_0}&kind=close&dataset=coin:daily")
    must(not asked["prices"] and any(u["reason"] == "UNANSWERED_REASON_ASKED_SOURCE"
                                     for u in asked["unanswered"]),
         f"step 6, a day not recorded is asked of its source: {asked}")
    # Read again at once: coalesced into the open want, or answered by it.
    again = page("book", "read", f"/prices?subject={btc}&date={DAY_0}&kind=close&dataset=coin:daily")
    must(again["prices"] or any(u["reason"] == "UNANSWERED_REASON_ASKED_SOURCE"
                                for u in again["unanswered"]),
         f"step 6, read again, asked or answered: {again}")
    until(60, lambda: page("book", "read",
                           f"/prices?subject={btc}&date={DAY_0}&kind=close&dataset=coin:daily")["prices"],
          "step 6, coin did not record the day wanted")
    page("book", "read", f"/prices?subject={btc}&dataset=coin:ticks")
    until(240, lambda: [w for w in page("coin", "admin", "/wants")["withdrawn"]
                        if w["dataset"] == "coin:ticks"],
          "step 6, the standing want on ticks was not withdrawn")

    # 7. Priority, and the stale guard.
    runner("priority", "meridian.v1.Price", "--kind", "close", "coin:daily",
           "note=The exchange's own close first.")
    stale = call("dashboard__set_source_priority",
                 {"data_type": "meridian.v1.Price", "kind": "close", "datasets": ["coin:daily"],
                  "against_updated_at_ns": 0, "note": "Read before the page's change."}, "refused")
    must("RECORD_CHANGED" in stale.get("reason", "") and
         any(f.get("path") == "against_updated_at_ns" for f in stale.get("fields", [])),
         f"step 7, a change against an older read is refused as changed: {stale}")
    read = call("dashboard__list_source_priorities", {}, "read")["data"]["priorities"]
    close = [p for p in read if p["kind"] == "close"]
    must(close and close[0]["datasets"] == ["coin:daily"] and close[0]["note"],
         f"step 7, the priority as listed: {read}")
    call("dashboard__set_source_priority",
         {"data_type": "meridian.v1.Price", "kind": "close", "datasets": ["coin:daily"],
          "against_updated_at_ns": close[0]["updated_at_ns"], "note": "Confirmed."}, "made")
    runner("entitle", "--instance", "book", "coin:ticks", "--withdraw", "note=Not needed.")

    # 8. The store.
    lines = compose("run", "--rm", "-T", "store", "lake").stdout.split("\n")
    log("store lake:\n" + "\n".join(lines))
    daily_rows = [line.split("|") for line in lines if line.startswith("row|coin:daily|")]
    sequences = sorted(int(row[3]) for row in daily_rows)
    must(sequences == list(range(1, len(sequences) + 1)) and len(sequences) >= 6,
         f"step 8, daily's prices and bars numbered from its head with no holes: {sequences}")
    daily_rows = [row for row in daily_rows if row[2] == "price"]
    second_close = [row for row in daily_rows if row[4] == "BTC-USD:1d:1791504000"]
    must(sorted(row[5] for row in second_close) == ["1", "2"],
         f"step 8, the second close kept as versions 1 and 2: {second_close}")
    wants = [line for line in lines if line.startswith("want|coin:daily|") and f"|{btc}|" in line]
    must(wants.count(f"want|coin:daily|asked|{btc}|0") == 1,
         f"step 8, two reads of the day not recorded asked once: {wants}")
    must(f"want|coin:daily|answered|{btc}|0" in wants, f"step 8, the want answered: {wants}")
    must(any(line.startswith("want|coin:ticks|withdrawn|") for line in lines),
         f"step 8, the standing want withdrawn: {lines}")
    must("priority|meridian.v1.Price|1|2" in lines, f"step 8, the priority's two changes: {lines}")

    took = int(time.time() - started)
    print(f"{NAME} OK in {took}s: on the plugin harness, a dgm's three datasets listed by the "
          "deployment admin's tool, licensed at the Data sources page and through the tool with a "
          "note, the one-person warning given for two readers; a reader entitled and the broker "
          "reloaded with its datasets; closes, a bar and an FX rate recorded and heard, a price on "
          "a venue ID in dollars' cash instrument and a USDC price on USDC's; a restatement heard "
          "and read as version 2, version 1 read as of before it; a reader not entitled refused, "
          "and an invalid date refused naming business_date; a day not recorded asked once for two "
          "reads and recorded against the want, a standing want withdrawn; the priority set at the "
          "page, a stale change refused by the tool and a current one taken; and `store lake` "
          "printing each version numbered with no holes, the want and the priority's changes")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError, IndexError, TypeError) as failed:
        compose("logs", "--no-color", check=False)
        compose("down", "-v", "--remove-orphans", check=False)
        print(f"{NAME} FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
