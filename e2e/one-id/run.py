"""`make e2e-one-id`: one ID end to end on the plugin harness (contract v24;
decisions/037; the one-ID spec's requirement 26), on e2e-orders' images and
stand-ins (e2e/orders/stand_in.py) against the harness's platform stand-in
(deploy/harness/platform.py), its v24 routes. SIMULATED ONLY: no broker, no
brokerage and no trading API of any kind is reached.

1. Set up as e2e-orders does: an account granted to the desk, each role's
   grant.
2. Nothing matched, asked of the platform (W3.4): a security its source
   holds is matched, the deployment's record kept under its INS- ID; one it
   confirms is minted; one it cannot confirm is a provisional LCL- record
   awaiting its staff; a licensed-only one (CUSIP) and a symbol-only one
   are kept local, provisional, nothing sent; a source thinner than the
   minimum is not found, nothing minted -- the street keeps it a
   placeholder (W2.4).
3. The licence-list filter: what reached the platform -- only schemes on
   the list, never an LCL- ID, never open_meridian_local, never the CUSIP;
   CUSIP added to the platform's list and pulled again, the licensed-only
   record is sent with no change in the deployment (W3.25's backfill).
4. The escape hatch (W11.36): an order on a local record is proposed and
   counted on the home page; turned off, a new one is refused
   REFUSAL_REASON_LOCAL_RECORD_NOT_ALLOWED; on again.
5. A person completes the provisional record's description; the
   platform's source then confirms it and a person's ask resolves it
   (W3.8): the INS- record carries the LCL- ID under open_meridian_local,
   the person's value still in force, the platform's offered.
6. An order opened on the local record before the resolution, placed and
   filled after it: the fill lands on the INS- record's position (W11's
   invariant, W9.9).
7. The upgrade's migration (W3.26): records kept before v24 -- two LCL-
   currencies for one code and a third -- sent and answered, INCOMPLETE
   until made; made from the Provisional tab: a record that is no line
   refuses the change whole, the exact lines made with CHF held back
   (keeping its LCL- ID, HELD_BACK), the duplicate GBP line made as one --
   both records becoming the one INS- record -- and the held-back line made
   later.
8. A platform without v24's routes (404): a new security is a provisional
   record, the platform reached for nothing (the build plan's step 4).

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import re
import subprocess
import sys
import time

NAME = "e2e-one-id"
PROJECT = f"meridian-core-{NAME}"
PEOPLE = "ben=Ben Ito"
LOG = f".{NAME}.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/orders/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/orders"))
INSTANCES = ("pm", "desk", "comp", "venue")
CODE = "SIM-ACCT-1"
CUSIP = "037833100"
PENDING = "BBG00PENDING01"
# Kept before v24 (W3.26), as the stand-in platform answers their codes.
KEPT_BEFORE_V24 = (("LCL-01J8XQ4M7K0000000000GBP1", "GBP", "Pound sterling, a v20 statement"),
                   ("LCL-01J8XQ4M7K0000000000GBP2", "GBP", "GBP cash, another custodian"),
                   ("LCL-01J8XQ4M7K0000000000CHF1", "CHF", "Swiss franc, a v20 statement"))
GBP = f"INS-01JGBP{'0' * 19}1"
CHF = f"INS-01JCHF{'0' * 19}1"


class Failed(Exception):
    pass


def log(text):
    with open(LOG, "a", encoding="utf-8") as kept:
        kept.write(text + "\n")


def compose(*args, check=True, env=None):
    done = subprocess.run(COMPOSE + list(args), env=env or ENV, capture_output=True, text=True,
                          timeout=900)
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


def page(instance, level, path):
    said = runner("page", "--instance", instance, "--level", level, path)
    status, _, body = said.partition("\n")
    must(status.strip() == "200", f"{instance} {path} answered {said[-600:]}")
    return json.loads(body)


def refusal(answer):
    return answer.get("refused") or {}


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


def heard():
    return platform("/e2e/heard")["heard"]


def escalations():
    return [h["body"] for h in heard() if h["path"].endswith("/escalate-instrument")]


def orders():
    return page("pm", "write", "/orders")


def order_of(listed, order_id):
    for order in listed["orders"]:
        if order["order_id"] == order_id:
            return order
    raise Failed(f"{order_id} is not listed: {listed}")


def store_book():
    return compose("run", "--rm", "-T", "store", "book").stdout


def run():
    started = time.time()
    compose("down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    for instance in INSTANCES:
        runner("ready", "--instance", instance)

    # 1. Set up.
    account = runner("account", "One ID fund", "--instance", "desk").strip().splitlines()[-1]
    runner("grant", "--instance", "pm", "--level", "write", "--role", "portfolio")
    for role in ("oms", "ems"):
        runner("grant", "--instance", "desk", "--level", "write", "--role", role, "--accounts", account)
    runner("grant", "--instance", "desk", "--level", "admin", "--role", "ems")
    runner("grant", "--instance", "comp", "--level", "write", "--role", "compliance", "--accounts", account)
    runner("grant", "--instance", "venue", "--level", "write", "--role", "connectivity")
    runner("grant", "--instance", "venue", "--level", "admin", "--role", "connectivity")
    runner("attribute", account, "waits=false", "note=The fund's compliance is post-trade.")

    # 2. Nothing matched, asked of the platform.
    aapl = page("venue", "write", "/resolve?symbol=AAPL&figi=BBG000B9XRY4")
    must(aapl["instrument_id"].startswith("INS-01JAAPL") and not aapl["provisional"],
         f"step 2, a match is kept under the platform's ID: {aapl}")
    minted = page("venue", "write", "/resolve?symbol=NEW1&figi=BBG00NEWMINT1")
    must(minted["instrument_id"].startswith("INS-01JMINT") and minted["minted"] and not minted["provisional"],
         f"step 2, minted from the platform's source: {minted}")
    pending = page("venue", "write", f"/resolve?symbol=ZZQ&figi={PENDING}&description=ZZQ+Holdings")
    must(pending["instrument_id"].startswith("LCL-") and pending["provisional"],
         f"step 2, the source cannot confirm it: a provisional local record: {pending}")
    licensed = page("venue", "write", f"/resolve?symbol=MUNI1&cusip={CUSIP}&description=Commonwealth+5%25+2041")
    must(licensed["instrument_id"].startswith("LCL-") and licensed["provisional"],
         f"step 2, a licensed-only record is kept local: {licensed}")
    symbol_only = page("venue", "write", "/resolve?symbol=SYM1")
    must(symbol_only["instrument_id"].startswith("LCL-") and symbol_only["provisional"],
         f"step 2, a symbol-only record is local and provisional: {symbol_only}")
    thin = page("venue", "write", "/resolve?symbol=THIN&thin=1")
    must(not thin["found"] and not thin["instrument_id"],
         f"step 2, thinner than the minimum: not found, nothing minted: {thin}")
    listing = runner("core-page", "/admin/instruments")
    must("Provisional (" in listing and pending["instrument_id"] in listing,
         "step 2, the Instruments page lists the provisional records")
    must(f'data-id="{pending["instrument_id"]}" data-reason="3"' in listing,
         "step 2, the record waiting for the platform's staff says so (PROVISIONAL_REASON_AWAITING_STAFF)")

    # 3. The licence-list filter.
    sent = escalations()
    flat = json.dumps(sent)
    must(sent, "step 3, the platform was asked to match or mint")
    must("LCL-" not in flat and "open_meridian_local" not in flat and CUSIP not in flat,
         f"step 3, never an LCL- ID, open_meridian_local or a licensed scheme: {flat[:600]}")
    schemes = {i["scheme"] for body in sent for i in body.get("identifiers", [])}
    must(schemes <= {"figi", "share_class_figi", "composite_figi", "iso4217", "ticker", "lei",
                     "caip19", "sec_series", "sec_class"},
         f"step 3, only schemes on the licence list: {schemes}")
    must(all(body.get("request_key") for body in sent), "step 3, each request carries its key")
    platform("/e2e/licence", {"schemes": ["figi", "lei", "ticker_and_venue", "caip19", "iso4217",
                                          "sec_series", "sec_class", "cusip"]})
    compose("restart", "conductor")
    until(180, lambda: CUSIP in json.dumps(escalations()),
          "step 3, the licensed-only record is sent once the platform lists CUSIP")

    # 4. The escape hatch. A person completes the local record first, as
    # the book needs its class and currency in force (W9.1); it stays the
    # deployment's local record until the platform answers for it.
    runner("instrument", pending["instrument_id"], "asset_class=equity", "currency=USD",
           "description=ZZQ Holdings, as our statement names it", "source=e2e",
           "note=named as the statement does")
    local = page("pm", "write", f"/propose?account={account}&instrument={pending['instrument_id']}&quantity=5&key=l1")
    must(local.get("order_id"), f"step 4, an order on a local record is proposed: {local}")
    runner("core-page", "/", "--expect", 'data-open-order-count="1"')
    runner("core-form", "/admin/trading-instructions", "/admin/trading-instructions/local-records",
           "allows=off", "note=Orders on the platform's records only")
    refused = refusal(page("pm", "write",
                           f"/propose?account={account}&instrument={pending['instrument_id']}&quantity=5&key=l2"))
    must(refused.get("reason") == "REFUSAL_REASON_LOCAL_RECORD_NOT_ALLOWED",
         f"step 4, off, a new order on a local record is refused: {refused}")
    runner("core-form", "/admin/trading-instructions", "/admin/trading-instructions/local-records",
           "allows=on", "note=The escape hatch, back on")

    # 5 and 6. A person's value; the order opened before the resolution.
    page("desk", "write", f"/release?order={local['order_id']}")
    platform("/e2e/confirm", {"figis": [PENDING]})
    runner("instrument", pending["instrument_id"], "--ask-platform")
    resolved = until(90, lambda: (lambda said: said if "open_meridian_local" in said else None)(
        runner("core-page", f"/admin/instruments/{pending['instrument_id']}")),
        "step 5, the provisional record resolved")
    ins = re.search(r"INS-01JMINT\d{19}", resolved)
    must(ins, "step 5, the record it became is the platform's INS- record")
    ins = ins.group(0)
    became = runner("core-page", f"/admin/instruments/{ins}")
    must("ZZQ Holdings, as our statement names it" in became,
         "step 5, the person's value stays in force on the INS- record")
    must(pending["instrument_id"] in became, "step 5, it carries the LCL- ID under open_meridian_local")
    runner("connectivity", "available", "--instance", "venue")
    runner("connectivity", "link", "--instance", "venue", "--ems", "desk", "--user", "person:local|harness")
    mapped = page("desk", "admin", f"/map?account={account}&code={CODE}&connectivity=venue")
    must(mapped == {"mapped": CODE}, f"step 6, mapped: {mapped}")
    page("desk", "write", f"/claim?order={local['order_id']}")
    until(60, lambda: (lambda p: p if p.get("placement_id") else None)(page(
        "desk", "write", f"/place?order={local['order_id']}&quantity=5&connectivity=venue&code={CODE}")),
        "step 6, placed")
    until(60, lambda: (lambda o: o if o["state"] == "ORDER_STATE_FILLED" else None)(
        order_of(orders(), local["order_id"])), "step 6, the order on the local record was not filled")
    book = until(60, lambda: (lambda b: b if any(
        line.startswith("position|One ID fund|") and ins in line for line in b.splitlines()) else None)(
        store_book()), "step 6, the fill landed on the INS- record's position")
    must(not any(line.startswith("position|One ID fund|") and pending["instrument_id"] in line
                 and line.split("|")[4] not in ("0", "")
                 for line in book.splitlines()),
         "step 6, nothing held under the resolved LCL- ID")

    # 7. The upgrade's migration (W3.26): records kept before v24, as a v20
    # deployment held them -- two LCL- currencies for one code, a duplicate
    # line, and a third its admin holds back -- sent and answered, then made
    # from the Provisional tab.
    seed = ("INSERT INTO instrument (instrument_id, asset_class, currency, description, "
            "lifecycle_state, version, valid_from_ns, record_time_ns) VALUES "
            + ", ".join(f"('{lcl}', 'ASSET_CLASS_CASH', '{code}', '{words}', "
                        "'INSTRUMENT_LIFECYCLE_STATE_ACTIVE', 1, 0, 1)"
                        for lcl, code, words in KEPT_BEFORE_V24)
            + "; INSERT INTO instrument_identifier (instrument_id, scheme, value, source, "
            "valid_from_ns) VALUES "
            + ", ".join(f"('{lcl}', 'iso4217', '{code}', '', 0)" for lcl, code, _ in KEPT_BEFORE_V24)
            + ";")
    compose("exec", "-T", "postgres", "psql", "-U", "meridian", "-d", "meridian",
            "-v", "ON_ERROR_STOP=1", "-c", seed)
    gbp1, gbp2, chf = (lcl for lcl, _, _ in KEPT_BEFORE_V24)
    tab = until(240, lambda: (lambda said: said if f'data-uncertain="{gbp1},{gbp2}"' in said
                              and f'name="line" value="{chf}"' in said else None)(
        runner("core-page", "/admin/instruments")),
        "step 7, the migration's answers drawn as lines: the GBP pair one uncertain line, CHF exact")
    must(f'data-id="{gbp1}" data-reason="2"' in tab,
         "step 7, a line answered but not made is PROVISIONAL_REASON_INCOMPLETE (ruled 2026-10-15)")
    runner("core-form", "/admin/instruments", "/admin/instruments/migration",
           f"line={licensed['instrument_id']}", "note=not a line",
           "--expect-refused", "no line")
    must(licensed["instrument_id"] in runner("core-page", "/admin/instruments"),
         "step 7, a record that is no line refuses the change whole")
    # The exact lines, CHF held back by its box.
    runner("core-form", "/admin/instruments", "/admin/instruments/migration",
           f"line={chf}", f"hold={chf}", "note=CHF waits for its custodian's statement")
    held = until(60, lambda: (lambda said: said if f'data-id="{chf}" data-reason="5"' in said else None)(
        runner("core-page", "/admin/instruments")), "step 7, CHF held back, keeping its LCL- ID")
    must(f'data-uncertain="{chf}"' in held, "step 7, a held-back line is made later on its own")
    # The duplicate: both GBP records become the one INS- record.
    runner("core-form", "/admin/instruments", "/admin/instruments/migration",
           f"group={gbp1},{gbp2}", "note=one pound sterling, kept twice before v24")
    gbp = until(60, lambda: (lambda said: said if gbp1 in said and gbp2 in said else None)(
        runner("core-page", f"/admin/instruments/{GBP}")),
        "step 7, the duplicate line made: both LCL- records carried on the INS- record")
    must("open_meridian_local" in gbp, "step 7, each LCL- ID under open_meridian_local")
    for lcl in (gbp1, gbp2):
        must(GBP in runner("core-page", f"/admin/instruments/{lcl}"),
             f"step 7, {lcl} answers the record it became")
    # The held-back line, made later.
    runner("core-form", "/admin/instruments", "/admin/instruments/migration",
           f"group={chf}", "note=the custodian confirmed CHF")
    until(60, lambda: (lambda said: said if chf in said else None)(
        runner("core-page", f"/admin/instruments/{CHF}")), "step 7, the held-back line made later")
    listing = runner("core-page", "/admin/instruments")
    must(f'data-id="{chf}"' not in listing and f'data-id="{gbp1}"' not in listing,
         "step 7, made lines leave the provisional records")

    # 8. A platform without v24's routes.
    old = dict(ENV, HARNESS_PLATFORM_V24="0")
    compose("up", "-d", "--force-recreate", "platform", env=old)
    away = page("venue", "write", "/resolve?symbol=AWAY&figi=BBG00AWAY0001")
    must(away["instrument_id"].startswith("LCL-") and away["provisional"],
         f"step 7, a platform without v24's routes is unreachable: a provisional record: {away}")
    listing = runner("core-page", "/admin/instruments")
    must(away["instrument_id"] in listing, "step 7, listed provisional")

    print(f"{NAME} OK in {int(time.time() - started)}s: a match kept under the platform's INS- ID, "
          "a mint from its source, a record it cannot confirm and a licensed-only one kept local and "
          "provisional, a source thinner than the minimum not found; only listed schemes sent, never "
          "an LCL- ID or open_meridian_local, CUSIP sent once listed; the escape hatch counted, off "
          "refusing, on again; a person's value kept through the resolution, the LCL- ID under "
          "open_meridian_local; an order opened before it filled onto the INS- record; the "
          "migration's lines made from the Provisional tab, two LCL- currencies for one code as one "
          "duplicate line and a held-back line made later; a platform without v24's routes "
          "unreachable")


if __name__ == "__main__":
    open(LOG, "w").close()
    try:
        run()
        compose("down", "-v", "--remove-orphans", check=False)
    except (Failed, subprocess.TimeoutExpired) as failed:
        print(f"{NAME} FAILED: {failed}", file=sys.stderr)
        print(f"the whole run is in {LOG}; the deployment is left up as {PROJECT}", file=sys.stderr)
        sys.exit(1)
