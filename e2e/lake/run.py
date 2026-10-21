"""`make e2e-lake` and `make e2e-recorded`: the lake's 1a end to end on the
plugin harness (contract v18, W10; plans/the-lake-prices-the-book, "Done
when"), core's stand-ins on the Python SDK (e2e/lake/stand_in.py): `coin`, a
a data plugin serving `daily`, `ticks` and `fx` from an exchange file; `book`, a
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
8. Trades and quotes (contract v19, the lake's 1b): `desk`, an ems plugin
   entitled to `coin:live`, follows BTC; `coin` records three trades and
   three quotes; `desk` hears every trade (never conflated) and the quotes;
   `coin` records a late print and a withdrawal, which `desk` catches up by
   the watermark it last saw, the late print included; the latest quotes are
   one per asset, the second dollar quote standing for the first; a quote
   nothing recorded is wanted, standing, of the streamed dataset.
9. Identity (decisions/034): a symbol and a FIGI resolve to two records, and
   both together are ambiguous -- nothing minted, the conflict listed with
   reason ambiguous; a symbol the admin ends on its record answers that
   record as of a date before, and a new record as of a date after.
10. Restarted: `coin` streams the standing want of `live`; restarted, it
   holds nothing, hears the same want again from the lake with no reader
   asking anew, and streams again; `desk` catches up both streams by
   watermark.
11. `store lake` prints each row version kept, numbered with no holes, the
   restatement as version 2, the want asked once and answered, the standing
   want withdrawn, the priority's changes, and the trades and quotes.
12. Contract v20: a restarted data plugin's last trades and its datasets'
   licences; a currency the deployment does not hold pulled from the
   harness's master, never minted; a master slower than a resolve waits, or
   stopped, mints nothing, and a later resolve finds the pulled record.
13. Contract v27 (ruling 484; W10.6, W10.7): a provisional LCL- record, which
   the platform cannot yet confirm, read by `book` on a day nothing
   recorded is wanted of `coin` -- the want sent, never ASKED_SOURCE alone --
   and `coin` records it against the want; the platform then confirms it
   and a person's ask resolves it to its INS- record, which `book` reads the
   LCL- row through (the replacement followed).

`make e2e-recorded` runs the same with STAND_IN_EXCHANGES naming RECORDED, a
developer's recorded exchanges (plans/the-lake-prices-the-book, Q11).

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import re
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
# Contract v27 (ruling 484): a FIGI the platform stand-in's source cannot
# confirm until told to (deploy/harness/platform.py, /e2e/confirm).
PENDING = "BBG00LAKEPEND1"


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


def platform(path, body=None):
    """The platform stand-in, from inside its own container."""
    script = ("import json,sys,urllib.request;"
              f"r=urllib.request.Request('http://127.0.0.1:8000{path}',"
              + ("data=sys.stdin.read().encode(),method='POST',headers={'Content-Type':'application/json'})"
                 if body is not None else "method='GET')")
              + ";print(urllib.request.urlopen(r).read().decode())")
    done = subprocess.run(COMPOSE + ["exec", "-T", "platform", "python", "-c", script], env=ENV,
                          input=json.dumps(body) if body is not None else None,
                          capture_output=True, text=True, timeout=60)
    log(f"$ platform {path}\n{done.stdout}{done.stderr}")
    if done.returncode != 0:
        raise Failed(f"the platform stand-in {path}: {done.stderr[-400:]}")
    return json.loads(done.stdout)


def datasets_listed():
    return {d["dataset"]: d for d in call("dashboard__list_datasets", {}, "unchanged")["data"]["datasets"]}


def run():
    started = time.time()
    compose("--profile", "runner", "down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    for instance in ("coin", "book", "other", "desk"):
        runner("ready", "--instance", instance)
    runner("grant", "--instance", "book", "--level", "read")
    runner("grant", "--instance", "other", "--level", "read")
    runner("grant", "--instance", "desk", "--level", "read")
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
    read = call("dashboard__list_source_priorities", {}, "unchanged")["data"]["priorities"]
    close = [p for p in read if p["kind"] == "close"]
    must(close and close[0]["datasets"] == ["coin:daily"] and close[0]["note"],
         f"step 7, the priority as listed: {read}")
    call("dashboard__set_source_priority",
         {"data_type": "meridian.v1.Price", "kind": "close", "datasets": ["coin:daily"],
          "against_updated_at_ns": close[0]["updated_at_ns"], "note": "Confirmed."}, "made")
    runner("entitle", "--instance", "book", "coin:ticks", "--withdraw", "note=Not needed.")

    # 8. Trades and quotes (contract v19).
    held = until(60, lambda: (lambda d: d if "coin:live" in d else None)(datasets_listed()),
                 "step 8, coin:live was not listed")
    runner("entitle", "--instance", "desk", "coin:live", "note=The desk works orders on it.")
    page("desk", "read", f"/follow?subject={btc}")
    time.sleep(3)
    traded = page("coin", "admin", "/trades")
    must(traded["trades"] == 3 and traded["quotes"] == 3, f"step 8, what coin recorded: {traded}")

    def heard_trades():
        heard = page("desk", "read", "/heard")
        return heard if len(heard["trades"]) >= 3 and heard["quotes"] else None
    heard = until(60, heard_trades, "step 8, desk did not hear the three trades")
    keys = [t["row_key"] for t in heard["trades"]]
    must(keys == ["BTC-USD-T-1", "BTC-USD-T-2", "BTC-USD-T-3"],
         f"step 8, every trade heard, in order, none dropped for a later one: {keys}")
    must(all(t["venue_id"].startswith("VEN-") and t["instrument_id"] for t in heard["trades"]),
         f"step 8, a trade names its venue and its price's cash instrument: {heard['trades']}")
    seen = max(t["sequence"] for t in heard["trades"])
    late = page("coin", "admin", "/late")
    must(late == {"recorded": 1, "restated": 1}, f"step 8, the late print and the withdrawal: {late}")
    caught = page("desk", "read", f"/catch-up?subject={btc}&after={seen}")
    caught_keys = sorted((t["row_key"], t["version"], t["cancelled"]) for t in caught["trades"])
    must(("BTC-USD-T-9", 1, False) in caught_keys and ("BTC-USD-T-1", 2, True) in caught_keys,
         f"step 8, caught up after the watermark, the late print included: {caught}")
    must(not [k for k in caught_keys if k[0] == "BTC-USD-T-2"],
         f"step 8, nothing seen before the watermark again: {caught}")
    quotes = page("desk", "read", f"/quotes?subject={btc}")["quotes"]
    must(sorted(q["row_key"] for q in quotes) == ["BTC-USD-Q-2", "BTC-USDC-Q-1"],
         f"step 8, the latest quote per asset: {quotes}")
    must(any(q["instrument_id"] == usdc for q in quotes), f"step 8, a USDC quote on USDC's instrument: {quotes}")
    wanted = page("desk", "read", f"/quotes-wanted?subject={usdc}")
    must("UNANSWERED_REASON_ASKED_SOURCE" in wanted["unanswered"], f"step 8, a quote wanted: {wanted}")
    until(60, lambda: [w for w in page("coin", "admin", "/wants")["wants"]
                       if w["dataset"] == "coin:live" and w["standing"]],
          "step 8, coin heard no standing want of the streamed dataset")

    # 9. Identity: an ambiguous resolve listed with its reason, never minted;
    # a symbol ended on its record answers each record for its own dates.
    identity = page("coin", "admin", "/identity")
    must(identity["symbol"] and identity["figi"] and identity["symbol"] != identity["figi"],
         f"step 9, a symbol and a FIGI on two records: {identity}")
    must(identity["both"] == {"found": False, "miss_reason": "MISS_REASON_AMBIGUOUS", "minted": False},
         f"step 9, both together ambiguous, nothing minted: {identity}")
    listed = call("dashboard__list_instruments_to_complete", {"include_complete": True}, "unchanged")["data"]
    flagged = [c for c in listed["conflicts"]
               if sorted(c["instrument_ids"]) == sorted([identity["symbol"], identity["figi"]])]
    must(flagged and flagged[0]["reason"] == "MISS_REASON_AMBIGUOUS",
         f"step 9, the conflict listed with reason ambiguous: {listed['conflicts']}")
    before = time.time_ns()
    old = page("coin", "admin", f"/resolve-as-of?scheme=symbol&value=ZZOLD&source=coinbase&as_of={before}")
    must(old["found"] and old["minted"], f"step 9, the old symbol's record: {old}")
    held = call("dashboard__read_instrument", {"instrument_id": old["instrument_id"]}, "unchanged")["data"]
    left = before + 1_000_000_000
    ended = call("dashboard__complete_instruments", {"completions": [{
        "instrument_id": old["instrument_id"], "against_version": held["version"],
        "note": "The symbol was freed by a delisting.",
        "values": [{"identifier": {"scheme": "symbol", "value": "ZZOLD", "source": "coinbase",
                                   "valid_until_ns": left}, "source": "the venue's notice"}]}]}, "made")
    window = [i for i in ended["results"][0]["record"]["identifiers"] if i["value"] == "ZZOLD"]
    must(window and window[0].get("valid_until_ns") == left,
         f"step 9, the symbol's window closed, never deleted: {ended}")
    after = left + 1_000_000_000
    new = page("coin", "admin", f"/resolve-as-of?scheme=symbol&value=ZZOLD&source=coinbase&as_of={after}")
    must(new["found"] and new["instrument_id"] != old["instrument_id"],
         f"step 9, given to another record after it left: {new}")
    then = page("coin", "admin", f"/resolve-as-of?scheme=symbol&value=ZZOLD&source=coinbase&as_of={before}")
    must(then["instrument_id"] == old["instrument_id"],
         f"step 9, as of a date before, the record that held it: {then}")

    # 10. A restarted data plugin streams again: the lake delivers its standing want
    # again, the same want, and nothing new is asked.
    def streaming():
        wants = page("coin", "admin", "/wants")
        live = [w for w in wants["wants"] if w["dataset"] == "coin:live" and w["standing"]]
        return wants if live and wants["streamed"] > 0 else None
    before = until(60, streaming, "step 10, coin did not stream the standing want")
    want_id = [w for w in before["wants"] if w["dataset"] == "coin:live" and w["standing"]][0]["want_id"]
    compose("restart", "coin")
    runner("ready", "--instance", "coin")

    def resumed():
        # The reader keeps asking, as it would, which asks nothing new.
        page("desk", "read", f"/quotes-wanted?subject={usdc}")
        wants = page("coin", "admin", "/wants")
        return wants if (wants["started"] != before["started"] and want_id in wants["streaming"]
                         and wants["streamed"] > 0) else None
    after = until(150, resumed, "step 10, the restarted coin did not hear its standing want again")
    must(all(w["want_id"] == want_id for w in after["wants"] if w["dataset"] == "coin:live"),
         f"step 10, the same want delivered again, none new: {after['wants']}")
    caught = page("desk", "read", f"/catch-up?subject={usdc}&after=0")["trades"]
    starts = {t["row_key"].split("-")[1] for t in caught if t["row_key"].startswith("STREAM-")}
    must({str(before["started"]), str(after["started"])} <= starts,
         f"step 10, both streams caught up by watermark: {sorted(starts)}")

    # 12. Contract v20. A restarted data plugin reads the last trade its own
    # instance recorded of a subject, per dataset and venue, to backfill
    # the gap from the venue's history; a reader is refused it (W10.6).
    latest = page("coin", "admin", f"/latest?subject={usdc}")
    must(latest["trades"] and all(t["dataset"] == "coin:live" for t in latest["trades"]),
         f"step 12, its own last trades: {latest}")
    venues = [t["venue_id"] for t in latest["trades"]]
    must(len(venues) == len(set(venues)), f"step 12, one per dataset and venue: {latest}")
    for last in latest["trades"]:
        latest_at = max(t["valid_from_ns"] for t in caught if t["venue_id"] == last["venue_id"])
        # The stream records on, so it is at least the last one caught up.
        must(last["valid_from_ns"] >= latest_at,
             f"step 12, the last recorded at its venue: {last} against {latest_at}")
    again = page("desk", "read", f"/catch-up?subject={usdc}&after=0")["trades"]
    for last in latest["trades"]:
        later = [t for t in again if t["venue_id"] == last["venue_id"]
                 and t["valid_from_ns"] > last["valid_from_ns"]]
        recorded_after = [t for t in later if t["sequence"] < last["sequence"]]
        must(not recorded_after, f"step 12, nothing recorded before it is later: {recorded_after}")
    must(latest["watermark"], f"step 12, with the watermark it was read at: {latest}")
    refused = page("desk", "read", f"/latest?subject={usdc}")["refused"]
    must("declares no dataset" in refused, f"step 12, a reader is refused its own read: {refused}")
    # A data plugin learns its own datasets' licences (W10.9).
    licences = page("coin", "admin", "/licences")
    must({"coin:daily", "coin:live"} <= set(licences["datasets"])
         and {l["dataset"] for l in licences["licences"]} >= {"coin:daily", "coin:live"},
         f"step 12, its own datasets' licences: {licences}")
    # A currency the deployment does not hold is pulled from the master,
    # found and never minted (W3.1).
    franc = page("coin", "admin", "/resolve-as-of?scheme=iso4217&value=CHF")
    must(franc["found"] and not franc["minted"],
         f"step 12, the franc found, not minted: {franc}")
    record = call("dashboard__read_instrument", {"instrument_id": franc["instrument_id"]},
                  "unchanged")["data"]
    # From contract v24 kept under the master's own INS- key (W3.1; the
    # one-ID spec's requirement 19), not under a new LCL- key carrying it.
    must(record["description"] == "Swiss franc" and record["currency"] == "CHF"
         and record["instrument_id"] == "INS-01JCHF00000000000000000001"
         and franc["instrument_id"] == "INS-01JCHF00000000000000000001"
         and not record["provisional"],
         f"step 12, the master's record kept as the deployment's: {record}")
    # A master slower than a resolve waits mints nothing (the 2026-10-11
    # ruling): the resolve is a miss, and once the pull brings the record a
    # later resolve finds it. JPY is answered nine seconds late, past the
    # resolve's bounded ask of eight and within the conductor's ten-second request (contract v24, the one-ID spec's
    # requirement 15; until then the sidecar's five-second call); a stopped
    # master is a name that resolves nowhere.
    started_at = time.time()
    yen = page("coin", "admin", "/resolve-as-of?scheme=iso4217&value=JPY")
    must(not yen["found"] and not yen["minted"] and not yen["instrument_id"],
         f"step 12, a slow master: a miss, nothing minted: {yen}")
    must(time.time() - started_at < 9.5, f"step 12, the resolve waited on the master past its "
         f"eight-second bound: {time.time() - started_at:.1f}s")
    yen = {}
    for _ in range(20):
        time.sleep(1)
        yen = page("coin", "admin", "/resolve-as-of?scheme=iso4217&value=JPY")
        if yen["found"]:
            break
    must(yen.get("found") and not yen.get("minted"),
         f"step 12, the pulled yen found by a later resolve, not minted: {yen}")
    # An absent master mints nothing either; back, the next pull finds it.
    compose("stop", "platform")
    krona = page("coin", "admin", "/resolve-as-of?scheme=iso4217&value=SEK")
    must(not krona["found"] and not krona["minted"],
         f"step 12, an absent master: a miss, nothing minted: {krona}")
    compose("start", "platform")
    for _ in range(60):
        time.sleep(2)
        krona = page("coin", "admin", "/resolve-as-of?scheme=iso4217&value=SEK")
        if krona["found"]:
            break
    must(krona.get("found") and not krona.get("minted"),
         f"step 12, the master back, the krona pulled, not minted: {krona}")

    # 13. Contract v27 (ruling 484): a provisional subject is wanted of its
    # data plugin, answered by the identifiers its record carries, and
    # followed to the INS- record it becomes.
    local = page("coin", "admin", f"/resolve-as-of?scheme=figi&value={PENDING}")
    local_id = local["instrument_id"]
    must(local_id.startswith("LCL-"), f"step 13, a record the platform cannot confirm is local: {local}")
    asked = page("book", "read", f"/prices?subject={local_id}&date={DAY_0}&kind=close&dataset=coin:daily")
    must(not asked["prices"] and any(u["reason"] == "UNANSWERED_REASON_ASKED_SOURCE"
                                     for u in asked["unanswered"]),
         f"step 13, a provisional subject not recorded is asked of its source: {asked}")
    until(60, lambda: [w for w in page("coin", "admin", "/wants")["wants"]
                       if local_id in w["subjects"] and w["business_date"] == DAY_0],
          "step 13, coin heard no want naming the provisional subject")
    until(60, lambda: page("book", "read",
                           f"/prices?subject={local_id}&date={DAY_0}&kind=close&dataset=coin:daily")["prices"],
          "step 13, coin did not record the provisional subject's day against the want")
    platform("/e2e/confirm", {"figis": [PENDING]})
    runner("instrument", local_id, "--ask-platform")
    resolved = until(90, lambda: (lambda said: said if "open_meridian_local" in said else None)(
        runner("core-page", f"/admin/instruments/{local_id}")),
        "step 13, the provisional record did not resolve")
    became = re.search(r"INS-01JMINT\d{19}", resolved)
    must(became, "step 13, the record it became is the platform's INS- record")
    followed = until(60, lambda: page("book", "read",
                                      f"/prices?subject={became.group(0)}&date={DAY_0}&kind=close"
                                      "&dataset=coin:daily")["prices"],
                     "step 13, the INS- record did not read the row recorded under the LCL- ID")
    must(followed[0]["amount"] and followed[0]["version"] == 1,
         f"step 13, the LCL- row read through the INS- record: {followed}")

    # 11. The store.
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
    live_rows = [line.split("|") for line in lines if line.startswith("row|coin:live|")]
    live_sequences = sorted(int(row[3]) for row in live_rows)
    must(live_sequences == list(range(1, len(live_sequences) + 1)) and len(live_sequences) > 8,
         f"step 10, live's trades and quotes numbered from its head with no holes: {live_sequences}")

    took = int(time.time() - started)
    print(f"{NAME} OK in {took}s: on the plugin harness, a data plugin's three datasets listed by the "
          "deployment admin's tool, licensed at the Data sources page and through the tool with a "
          "note, the one-person warning given for two readers; a reader entitled and the broker "
          "reloaded with its datasets; closes, a bar and an FX rate recorded and heard, a price on "
          "a venue ID in dollars' cash instrument and a USDC price on USDC's; a restatement heard "
          "and read as version 2, version 1 read as of before it; a reader not entitled refused, "
          "and an invalid date refused naming business_date; a day not recorded asked once for two "
          "reads and recorded against the want, a standing want withdrawn; the priority set at the "
          "page, a stale change refused by the tool and a current one taken; trades heard every "
          "one by an ems plugin and a loss caught up by watermark, a late print included, the "
          "latest quote per asset and a standing want of a streamed dataset; an ambiguous resolve "
          "listed with its reason and nothing minted, a symbol ended on its record answering each "
          "record for its own dates; a restarted data plugin streaming again on its standing want, "
          "delivered again by the lake, reading its own last trade per venue for the backfill, "
          "a reader refused it, and its own datasets' licences; a currency the deployment did not "
          "hold pulled from the master and kept, found and not minted; a provisional record wanted "
          "of its data plugin, recorded against the want and read through the INS- record it "
          "became; and `store lake` printing each version numbered with no "
          "holes, the want and the priority's changes")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError, IndexError, TypeError) as failed:
        compose("logs", "--no-color", check=False)
        compose("--profile", "runner", "down", "-v", "--remove-orphans", check=False)
        print(f"{NAME} FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("--profile", "runner", "down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
