"""`make e2e-orders`: the order path's slice A end to end on the plugin
harness (contract v22, W11; plans/an-order-reaches-a-venue-through-the-book,
"v22: core's make e2e-orders"), against core's stand-ins on the Python SDK
(e2e/orders/stand_in.py) and a simulated venue. SIMULATED ONLY: no broker,
no brokerage, no paper account, no trading API of any kind is reached.

1. Set up: an account granted to the desk's ems, another granted to none of
   the desk's roles, each role's grant, and the instrument completed, the
   platform asked about it: listed on XNAS, its settlement date from Friday
   9 October 2026 is Tuesday 13 October, T+1 past Columbus Day on the US
   securities settlement calendar (W3.20).
2. Waiting for compliance: a new account waits; a release stays proposed,
   awaiting check, until compliance answers, and is released by its verdict.
   The portfolio plugin's release is refused by its sidecar, naming no
   grant: the duties are apart (decisions/033).
3. A placement before the venue is linked to the desk: refused
   REFUSAL_REASON_CONNECTIVITY_NOT_ALLOWED, and recorded.
4. The venue made available and linked to the desk, the account mapped to a
   venue account code; claimed, placed, acknowledged and filled in two; the
   order filled; the position up by the fills, a lot at the fill's price and
   the security pending to the stated settlement date. The second
   connectivity plugin, never linked, heard no placement.
5. Child orders: with the account set not to wait, an order released at
   once, a child of 4 under it in the desk's TWAP, placed and filled; the
   parent's committed and reported filled count it.
6. Refusals: the desk's release of an order in an account outside its write
   scope, PERMISSION_DENIED with no code, nothing changed; the desk's
   quantity change, REFUSAL_REASON_ORDER_STATE.
7. A stop: a working placement of 200; a deployment admin's stop on the
   venue's plugin; the venue cancels at once, reporting the cancel as a fact
   naming the stop, then the venue's confirmation; the book refuses nothing,
   and the order is back to released. A new placement under the stop is
   sent to the venue, which holds it back. The desk heard the stop. A
   plugin's admin may not lift it from All accounts.
8. Unplaced facts: an execution naming a placement nobody holds, kept in
   the account its code maps to; one whose code nobody mapped, kept naming
   no account and counted on the venue's admin view.
9. Venue-originated fills (contract v23; W11.11, W11.23, W11.24): the venue
   reports two fills of orders placed in its own app, on no order and no
   placement; each waits unmatched in the account its code maps to, on the
   desk's Unmatched fills; the desk accepts one to a new order, filled with
   its placement at filled, routed at 0, naming the venue; and answers the
   other DK through its MCP tool, dk_execution. The venue hears each answer.
   A later fill of the DK'd venue order waits unmatched on its own, and the
   DK'd fill is answered no more. And the date read's spot date for
   EUR/USD traded Friday 9 October 2026 is Tuesday 13 October, Columbus Day
   counted on EUR alone.
10. `store book` prints the orders, placements, executions and positions,
   and `meridian-bor rebuild` reproduces it line for line -- after step 11.
11. Booking (contract v24; W11.1, W11.27, W11.29): a block across two funds
   in the allocation account, booked to them and handed off, each member
   filled by its allocation and the allocation account netting to zero; an
   overfill's excess booked to the error account with its cause; and
   operations' correction by replace after the hand-off, each fund holding
   the corrected allocation.
12. Execution costs (contract v27; W10.11, W11.17, ruling 510): the orders'
   account given its base currency, the portfolio plugin runs an
   execution_cost job on it; the analytics component reads the account's
   orders, placements, executions and the orders' history, and records an
   ExecutionCost row per execution per benchmark, keyed `EXE-...:arrival`:
   step 4's two fills measured at the order's proposal, its release, the
   placement's routing and the fill, each NO_QUOTE where the harness lake
   holds no mid; the same job again records nothing new.

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import re
import subprocess
import sys
import time

NAME = "e2e-orders"
PROJECT = f"meridian-core-{NAME}"
PEOPLE = "ben=Ben Ito"
LOG = f".{NAME}.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/orders/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/orders"))
INSTANCES = ("pm", "desk", "comp", "venue", "venue2", "ops")
CODE = "SIM-ACCT-1"
# The cash instruments of the platform's stand-in (deploy/harness/platform.py),
# by the IDs its EUR/USD spot-date rule names.
PLATFORM_EUR = "INS-01JEUR" + "0" * 19 + "1"
PLATFORM_USD = "INS-01JUSD" + "0" * 19 + "1"


class Failed(Exception):
    pass


def log(text):
    with open(LOG, "a", encoding="utf-8") as kept:
        kept.write(text + "\n")


def compose(*args, check=True):
    done = subprocess.run(COMPOSE + list(args), env=ENV, capture_output=True, text=True,
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
    """A stand-in's page, as JSON."""
    said = runner("page", "--instance", instance, "--level", level, path)
    status, _, body = said.partition("\n")
    must(status.strip() == "200", f"{instance} {path} answered {said[-600:]}")
    return json.loads(body)


def refusal(answer):
    return answer.get("refused") or {}


def order_of(listed, order_id):
    for order in listed["orders"]:
        if order["order_id"] == order_id:
            return order
    raise Failed(f"{order_id} is not listed: {listed}")


def orders(instance="pm"):
    return page(instance, "write", "/orders")


def store_book():
    return compose("run", "--rm", "-T", "store", "book").stdout


def run():
    started = time.time()
    compose("--profile", "runner", "down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    for instance in INSTANCES:
        runner("ready", "--instance", instance)

    # 1. Set up. The account the orders are in is granted to the desk's
    # instance, which may map it to a venue account code (W6.30, W11.22).
    account = runner("account", "Orders fund", "--instance", "desk").strip().splitlines()[-1]
    elsewhere = runner("account", "Elsewhere", "--instance", "pm").strip().splitlines()[-1]
    # The portfolio plugin on All accounts; the desk and compliance on the
    # orders' account alone, so Elsewhere is outside the desk's write scope.
    # (The harness keeps one account group per person, so the narrower
    # grants share it.)
    runner("grant", "--instance", "pm", "--level", "write", "--role", "portfolio")
    for role in ("oms", "ems"):
        runner("grant", "--instance", "desk", "--level", "write", "--role", role,
               "--accounts", account)
    runner("grant", "--instance", "desk", "--level", "admin", "--role", "ems")
    runner("grant", "--instance", "comp", "--level", "write", "--role", "compliance",
           "--accounts", account)
    runner("grant", "--instance", "venue", "--level", "write", "--role", "connectivity")
    runner("grant", "--instance", "venue", "--level", "admin", "--role", "connectivity")
    runner("grant", "--instance", "venue", "--level", "admin", "--role", "connectivity",
           "--to", "ben")
    aapl = page("venue", "write", "/resolve?symbol=AAPL&figi=BBG000B9XRY4")["instrument_id"]
    must(aapl, "step 1, AAPL resolved")
    # From contract v24 the platform matched it (W3.4): the deployment's
    # record is the platform's, kept complete under its INS- ID, and a
    # person's ask refreshes what the platform offers and its venue.
    must(aapl.startswith("INS-"), f"step 1, AAPL is the platform's record: {aapl}")
    runner("instrument", aapl, "--ask-platform")
    # The platform named its listing venue, XNAS in the US: a settlement date
    # is worked out by the US equities rule, the rule and its calendar pulled
    # on the first miss (W3.18) and the read asked again.
    settled = until(90, lambda: (lambda said: said if said["outcome"] == "DATE_OUTCOME_COMPUTED"
                                 else None)(
        page("venue", "write", f"/settle?instrument={aapl}&trade=2026-10-09")),
        "step 1, AAPL's settlement date computed")
    must(settled["date"] == "2026-10-13" and settled["rule"] == "US/equity v2",
         f"step 1, AAPL settles T+1 on Tuesday 13 October by US/equity v2: {settled}")

    # 2. Waiting for compliance (W11.6; step 5's Q5).
    first = page("pm", "write", f"/propose?account={account}&instrument={aapl}&quantity=10&key=o1")
    must(first.get("state") == "ORDER_STATE_PROPOSED", f"step 2, proposed: {first}")
    order1 = first["order_id"]
    pm_release = refusal(page("pm", "write", f"/release?order={order1}"))
    must(pm_release.get("status") == "PERMISSION_DENIED" and "no grant" in pm_release.get("words", ""),
         f"step 2, the portfolio plugin may not release: {pm_release}")
    waiting = page("desk", "write", f"/release?order={order1}")
    must(waiting.get("state") == "ORDER_STATE_PROPOSED"
         and "ORDER_CONDITION_AWAITING_CHECK" in waiting.get("conditions", []),
         f"step 2, the release waits for compliance: {waiting}")
    checked = page("comp", "write", f"/check?order={order1}&verdict=pass")
    must(checked.get("state") == "ORDER_STATE_RELEASED", f"step 2, released by the verdict: {checked}")

    # 3. A placement at a connectivity plugin not linked to the desk.
    not_linked = refusal(page("desk", "write",
                              f"/place?order={order1}&quantity=10&connectivity=venue&code={CODE}"))
    must(not_linked.get("reason") == "REFUSAL_REASON_CONNECTIVITY_NOT_ALLOWED",
         f"step 3, refused: {not_linked}")

    # 4. Linked, mapped, placed and filled.
    runner("connectivity", "available", "--instance", "venue")
    runner("connectivity", "available", "--instance", "venue2")
    runner("connectivity", "link", "--instance", "venue", "--ems", "desk",
           "--user", "person:local|harness")
    mapped = page("desk", "admin", f"/map?account={account}&code={CODE}&connectivity=venue")
    must(mapped == {"mapped": CODE}, f"step 4, mapped: {mapped}")
    page("desk", "write", f"/claim?order={order1}")
    placed = until(60, lambda: (lambda p: p if p.get("placement_id") else None)(page(
        "desk", "write", f"/place?order={order1}&quantity=10&connectivity=venue&code={CODE}")),
        "step 4, placed")
    must(placed.get("state") == "PLACEMENT_STATE_QUEUED", f"step 4, queued: {placed}")
    filled = until(60, lambda: (lambda o: o if o["state"] == "ORDER_STATE_FILLED" else None)(
        order_of(orders(), order1)), "step 4, the order was not filled")
    must(filled["reported_filled"] == "10" and filled["committed"] == "0",
         f"step 4, filled: {filled}")
    heard_by_other = page("venue2", "admin", "/heard")["heard"]
    must(heard_by_other == [], f"step 4, the other connectivity plugin heard nothing: {heard_by_other}")
    book = store_book()
    position = [line for line in book.splitlines()
                if line.startswith("position|Orders fund|") and "AAPL" in line]
    must(position and position[0].split("|")[4] == "10",
         f"step 4, the position moved by 10: {position}")
    # Lots opened by the fills at their price: 4 and 6 at 227.41, source
    # LOT_SOURCE_EXECUTION (3).
    lots = [line for line in book.splitlines() if line.startswith("lot|Orders fund|")]
    must(any("909.64 USD" in line and line.endswith("|3") for line in lots)
         and any("1364.46 USD" in line and line.endswith("|3") for line in lots),
         f"step 4, a lot per fill at its price: {lots}")
    must(any(line.startswith("pending|Orders fund|") and "AAPL" in line for line in book.splitlines()),
         "step 4, the security pending to its settlement date")

    # 5. Child orders: released at once where the account does not wait.
    runner("attribute", account, "waits=false", "note=The desk's compliance is post-trade.")
    second = page("pm", "write", f"/propose?account={account}&instrument={aapl}&quantity=10&key=o2")
    order2 = second["order_id"]
    released = page("desk", "write", f"/release?order={order2}")
    must(released.get("state") == "ORDER_STATE_RELEASED", f"step 5, released at once: {released}")
    child = page("desk", "write", f"/child?parent={order2}&quantity=4&account={account}&instrument={aapl}")
    must(child.get("state") == "ORDER_STATE_RELEASED" and child.get("parent") == order2,
         f"step 5, the child released within its parent: {child}")
    parent = order_of(orders(), order2)
    must(parent["committed"] == "4" and parent["idle"] == "6", f"step 5, the parent committed: {parent}")
    page("desk", "write", f"/place?order={child['order_id']}&quantity=4&connectivity=venue&code={CODE}")
    parent = until(60, lambda: (lambda o: o if o["reported_filled"] == "4" else None)(
        order_of(orders(), order2)), "step 5, the child's fill is not the parent's")
    must(parent["state"] == "ORDER_STATE_WORKING", f"step 5, the parent working: {parent}")

    # 6. Refusals: the write scope, and an ems changing a quantity.
    third = page("pm", "write", f"/propose?account={elsewhere}&instrument={aapl}&quantity=5&key=o3")
    outside = refusal(page("desk", "write", f"/release?order={third['order_id']}"))
    must(outside.get("status") == "PERMISSION_DENIED"
         and outside.get("reason", "REFUSAL_REASON_UNSPECIFIED") == "REFUSAL_REASON_UNSPECIFIED",
         f"step 6, outside the desk's write scope, no code: {outside}")
    changed = refusal(page("desk", "write", f"/change?order={order2}&quantity=12&reason=more"))
    must(changed.get("reason") == "REFUSAL_REASON_ORDER_STATE",
         f"step 6, an ems changes no ordered quantity: {changed}")
    must(order_of(orders(), third["order_id"])["state"] == "ORDER_STATE_PROPOSED",
         "step 6, nothing changed")

    # 7. A stop on the venue's plugin.
    fourth = page("pm", "write", f"/propose?account={account}&instrument={aapl}&quantity=200&key=o4")
    order4 = fourth["order_id"]
    page("desk", "write", f"/release?order={order4}")
    page("desk", "write", f"/place?order={order4}&quantity=200&connectivity=venue&code={CODE}")
    until(60, lambda: page("venue", "write", "/heard")["working"], "step 7, the venue is working it")
    runner("instruction", "stop", "--plugin", "venue",
           "note=Halt the simulated venue; cancel its working placements.")
    stopped = until(60, lambda: (lambda o: o if o["state"] == "ORDER_STATE_RELEASED" else None)(
        order_of(orders(), order4)), "step 7, the stop's cancel was not recorded")
    must(stopped["committed"] == "0", f"step 7, nothing committed after the stop: {stopped}")
    until(30, lambda: "TRADING_INSTRUCTION_VALUE_STOP" in page("desk", "write", "/heard")["instructions"],
          "step 7, the desk heard the stop")
    held = page("desk", "write", f"/place?order={order4}&quantity=50&connectivity=venue&code={CODE}")
    must(held.get("state") == "PLACEMENT_STATE_QUEUED", f"step 7, recorded, not refused: {held}")
    until(30, lambda: [h for h in page("venue", "write", "/heard")["heard"]
                       if h["placement_id"] == held["placement_id"] and "held back" in h.get("done", "")],
          "step 7, the venue held the new placement back")
    runner("instruction", "active", "--all", "--as", "ben",
           "--expect-refused", "plugin's admin", "note=Lift it.")

    # 8. Unplaced facts.
    astray = page("venue", "write", f"/astray?code={CODE}")["unplaced"]
    must(astray == {"account_id": account, "reason": "UNPLACED_REASON_UNKNOWN_PLACEMENT"},
         f"step 8, kept in its account: {astray}")
    nobody = page("venue", "write", "/astray?code=NOBODY-MAPPED")["unplaced"]
    must(nobody == {"account_id": "", "reason": "UNPLACED_REASON_UNMAPPED_VENUE_ACCOUNT"},
         f"step 8, kept naming no account: {nobody}")
    runner("summary", "--instance", "venue", "--until", "Unplaced facts", "--seconds", "90")

    # 9. Venue-originated fills (contract v23).
    own = lambda quantity, order: page(
        "venue", "write",
        f"/originate?code={CODE}&instrument={aapl}&quantity={quantity}&side=buy&order={order}")
    first, second = own(5, "SIM-OWN-1"), own(3, "SIM-OWN-2")
    for fill, order in ((first, "SIM-OWN-1"), (second, "SIM-OWN-2")):
        must(fill["state"] == "EXECUTION_STATE_UNMATCHED" and fill["account_id"] == account
             and fill["venue_order_id"] == order and not fill["placement_id"],
             f"step 9, a fill on no placement waits unmatched in its mapped account: {fill}")
    ids = (first["execution_id"], second["execution_id"])
    waiting = page("desk", "write", f"/unmatched?account={account}")["fills"]
    must({f["execution_id"] for f in waiting} == set(ids),
         f"step 9, the desk lists the two unmatched fills: {waiting}")
    accepted = page("desk", "write", f"/accept?executions={ids[0]}&new=1")
    placed = accepted["placement"]
    must(accepted["order"]["state"] == "ORDER_STATE_FILLED"
         and placed["state"] == "PLACEMENT_STATE_FILLED" and placed["routed_at_ns"] == 0
         and placed["connectivity"] == "venue"
         and accepted["executions"][0]["state"] == "EXECUTION_STATE_MATCHED",
         f"step 9, accepted to a new order, filled, its placement at filled: {accepted}")
    runner("mcp", "connect", "--covers", "desk:ems:write", "--client", "desk agent")
    runner("mcp", "list", "--expect", "desk__accept_execution", "--expect", "desk__dk_execution",
           "--expect", "desk__read_unmatched_fills", "--client", "desk agent")
    runner("mcp", "call", "desk__dk_execution",
           json.dumps({"execution_ids": [ids[1]], "reason": "no_matching_order"}),
           "--expect-outcome", "made", "--client", "desk agent")
    heard = until(60, lambda: (lambda said: said if {(a.get("execution_id"), a.get("state"))
                                                     for a in said} >= {
        (ids[0], "EXECUTION_STATE_MATCHED"), (ids[1], "EXECUTION_STATE_DONT_KNOW")} else None)(
        page("venue", "write", "/answers")["answers"]), "step 9, the venue heard each answer")
    must(not any(a.get("not_ours") for a in heard), f"step 9, it heard only its own: {heard}")
    later = own(2, "SIM-OWN-2")
    must(later["state"] == "EXECUTION_STATE_UNMATCHED",
         f"step 9, a later fill of the DK'd venue order waits on its own: {later}")
    left = page("desk", "write", f"/unmatched?account={account}")["fills"]
    must([f["execution_id"] for f in left] == [later["execution_id"]],
         f"step 9, only the later fill waits: {left}")
    again = refusal(page("desk", "write", f"/dk?executions={ids[1]}&reason=other&text=again"))
    must(again.get("reason") == "REFUSAL_REASON_EXECUTION_STATE",
         f"step 9, a DK'd fill is answered no more: {again}")
    spot = until(90, lambda: (lambda said: said if said["outcome"] == "DATE_OUTCOME_COMPUTED"
                              else None)(
        page("venue", "write", f"/spot?pair={PLATFORM_EUR},{PLATFORM_USD}&trade=2026-10-09")),
        "step 9, EUR/USD's spot date computed")
    must(spot["date"] == "2026-10-13" and spot["rule"] == "EUR/USD v1",
         f"step 9, EUR/USD traded Friday 9 October spots Tuesday 13 October: {spot}")
    # The same read by the deployment's own currency records, as a fill's
    # value date asks it: from contract v24 the deployment keeps each under
    # the platform's own INS- ID (W3.1, requirement 19), matched directly.
    local = [page("venue", "write", f"/resolve?iso4217={code}")["instrument_id"]
             for code in ("EUR", "USD")]
    must(local == [PLATFORM_EUR, PLATFORM_USD],
         f"step 9, the deployment keeps each currency under the platform's ID: {local}")
    by_local = page("venue", "write", f"/spot?pair={local[0]},{local[1]}&trade=2026-10-09")
    must(by_local["outcome"] == "DATE_OUTCOME_COMPUTED" and by_local["date"] == "2026-10-13"
         and by_local["rule"] == "EUR/USD v1",
         f"step 9, EUR/USD's spot read by the deployment's own records gets its date: {by_local}")

    # 11. Booking (contract v24; W11.1, W11.27, W11.29; the task's "Done
    # when"): a block across two funds booked to them and handed off, each
    # member filled by its allocation; an overfill's excess booked to the
    # error account; and operations' correction by replace after the
    # hand-off.
    # Step 7's stop lifted, at the level that set it, so the venue fills.
    runner("instruction", "active", "--plugin", "venue", "note=The venue trades again.")
    funds = [runner("account", name, "--instance", "desk").strip().splitlines()[-1]
             for name in ("Fund A", "Fund B", "Block", "Errors")]
    fund_a, fund_b, block_account, errors = funds
    for marked, mark in ((block_account, "allocation"), (errors, "error")):
        runner("core-form", "/admin", "/admin/accounts/book", f"account_id={marked}",
               "lot_relief_default=0", f"account_mark={mark}", f"note=The firm's {mark} account")
    every = ",".join([account] + funds)
    for role in ("oms", "ems"):
        runner("grant", "--instance", "desk", "--level", "write", "--role", role, "--accounts", every)
    runner("grant", "--instance", "ops", "--level", "write", "--role", "operations", "--accounts", every)
    for held in funds:
        runner("attribute", held, "waits=false", "note=Post-trade compliance.")
    page("desk", "admin", f"/map?account={block_account}&code=SIM-BLOCK-1&connectivity=venue")
    members = []
    for fund, quantity, key in ((fund_a, 6, "b1"), (fund_b, 4, "b2")):
        member = page("pm", "write", f"/propose?account={fund}&instrument={aapl}&quantity={quantity}&key={key}")
        page("desk", "write", f"/release?order={member['order_id']}")
        members.append(member["order_id"])
    block = page("desk", "write",
                 f"/block?account={block_account}&instrument={aapl}&members={','.join(members)}")
    must(block.get("state") == "ORDER_STATE_RELEASED", f"step 11, the block released on creation: {block}")
    page("desk", "write", f"/claim?order={block['order_id']}")
    page("desk", "write", f"/place?order={block['order_id']}&quantity=10&connectivity=venue&code=SIM-BLOCK-1")
    until(60, lambda: (lambda o: o if o["state"] == "ORDER_STATE_FILLED" else None)(
        order_of(orders("desk"), block["order_id"])), "step 11, the block was not filled")
    fills = page("desk", "write", f"/executions?account={block_account}&order={block['order_id']}")["executions"]
    gross = {"6": "1364.46", "4": "909.64"}
    booked = page("desk", "write",
                  f"/book?order={block['order_id']}&executions={','.join(f['execution_id'] for f in fills)}"
                  f"&alloc={fund_a}:6:{gross['6']},{fund_b}:4:{gross['4']}&handoff=1")
    must(booked.get("state") == "BOOKING_STATE_HANDED_OFF", f"step 11, booked and handed off: {booked}")
    for member in members:
        must(order_of(orders(), member)["state"] == "ORDER_STATE_FILLED",
             f"step 11, each member filled by its allocation: {member}")
    book = store_book()
    held = lambda name: [line.split("|")[4] for line in book.splitlines()
                         if line.startswith(f"position|{name}|") and "AAPL" in line]
    must(held("Fund A") == ["6"] and held("Fund B") == ["4"],
         f"step 11, each fund holds its allocation: {held('Fund A')}, {held('Fund B')}")
    must(held("Block") in ([], ["0"]), f"step 11, the allocation account nets to zero: {held('Block')}")

    # The overfill: ten ordered and filled, two more from the venue's own
    # app accepted to it; the excess booked to the error account.
    page("desk", "admin", f"/map?account={account}&code=SIM-ACCT-OVER&connectivity=venue")
    over = page("pm", "write", f"/propose?account={account}&instrument={aapl}&quantity=10&key=o11")
    page("desk", "write", f"/release?order={over['order_id']}")
    page("desk", "write", f"/claim?order={over['order_id']}")
    page("desk", "write", f"/place?order={over['order_id']}&quantity=10&connectivity=venue&code=SIM-ACCT-OVER")
    fills = until(60, lambda: (lambda f: f if len(f) == 3 else None)(page(
        "desk", "write", f"/executions?account={account}&order={over['order_id']}")["executions"]),
        "step 11, the venue did not fill two more than placed")
    before_over = held("Orders fund")
    excess = page("desk", "write",
                  f"/book?order={over['order_id']}&executions={','.join(f['execution_id'] for f in fills)}"
                  f"&alloc={account}:10:2274.10,{errors}:2:454.82:error&handoff=1")
    must(excess.get("state") == "BOOKING_STATE_HANDED_OFF"
         and any(a["error_cause"] == "ERROR_CAUSE_OVERFILL" for a in excess["allocations"]),
         f"step 11, the excess booked to the error account with its cause: {excess}")
    book = store_book()
    must(held("Errors") == ["2"], f"step 11, the error account holds the excess: {held('Errors')}")

    # Operations corrects the block's booking by replace after the hand-off.
    corrected = page("ops", "write",
                     f"/correct?booking={booked['booking_id']}&alloc={fund_a}:5:1137.05,{fund_b}:5:1137.05"
                     "&reason=The+allocation+was+five+and+five")
    must(corrected.get("state") == "BOOKING_STATE_HANDED_OFF"
         and sorted(a["quantity"] for a in corrected["allocations"]) == ["5", "5"],
         f"step 11, replaced, a new version, the state kept: {corrected}")
    book = store_book()
    must(held("Fund A") == ["5"] and held("Fund B") == ["5"],
         f"step 11, each fund holds the corrected allocation: {held('Fund A')}, {held('Fund B')}")
    # The booking is the block's, in the allocation account it was booked in.
    listed = page("ops", "write", f"/bookings?account={block_account}")["bookings"]
    must([b["booking_id"] for b in listed] == [booked["booking_id"]],
         f"step 11, operations reads the booking: {listed}")
    del before_over

    # The harness routes a model id as a deployment admin does (v27 stage E):
    # the venue's plugin prices nothing, so routing to it is refused; back to
    # the built-in model is taken.
    runner("model-routing", "quoted_price", "venue", "note=Not a pricing plugin.",
           "--expect-refused", "pricing")
    runner("model-routing", "quoted_price", "built-in", "note=The built-in model prices it.")

    # 12. Execution costs from the order path (contract v27, ruling 510).
    runner("core-form", "/admin", "/admin/accounts/book", f"account_id={account}",
           "base_currency_code=USD", "lot_relief_default=0", "waits_for_compliance=no-wait",
           "note=The fund reports in dollars.")
    tca = page("pm", "write", f"/tca?account={account}")
    first_fills = [r for r in tca["rows"] if r["order_id"] == order1]
    benchmarks = sorted({r["benchmark"] for r in first_fills})
    must(len(first_fills) == 8 and benchmarks == [
        "TCA_BENCHMARK_ARRIVAL", "TCA_BENCHMARK_DECISION", "TCA_BENCHMARK_MID_AT_FILL",
        "TCA_BENCHMARK_PLACEMENT_ARRIVAL"],
         f"step 12, step 4's two fills, each against four benchmarks: {tca}")
    must(all(r["row_key"] == f"{r['execution_id']}:" + r["benchmark"][len("TCA_BENCHMARK_"):].lower()
             for r in first_fills),
         f"step 12, each row keyed by its execution and benchmark: {first_fills}")
    must(all(r["benchmark_at_ns"] > 0 and r["unanswered"] == "TCA_UNANSWERED_NO_QUOTE"
             for r in first_fills),
         f"step 12, each moment from the order path, no mid in the harness lake: {first_fills}")
    again = page("pm", "write", f"/tca?account={account}")
    must(sorted((r["row_key"], r["version"]) for r in again["rows"])
         == sorted((r["row_key"], r["version"]) for r in tca["rows"]),
         f"step 12, the same job again records nothing new: {again}")

    # 10. The book printed, and rebuilt from its journal line for line.
    before = store_book()
    for wanted in ("order|Orders fund|", "placement|Orders fund|", "execution|Orders fund|",
                   # The stop, lifted to active at step 11.
                   "unplaced||execution|2|venue|false", "instruction|control|1|1"):
        must(wanted in before, f"step 10, store book prints {wanted!r}: {before[-2000:]}")
    compose("stop", "bor")
    compose("run", "--rm", "--no-deps", "-T", "bor",
            "set -e; db=$(cat /secrets/postgres); "
            "export MERIDIAN_BOR_DATABASE_URL=postgres://meridian:$db@postgres:5432/meridian; "
            "exec meridian-bor rebuild")
    after = store_book()
    must(before == after, "step 10, meridian-bor rebuild did not reproduce the book")
    took = int(time.time() - started)
    print(f"{NAME} OK in {took}s: on the plugin harness against core's stand-ins and a simulated "
          "venue, an order proposed, held at proposed awaiting compliance and released by its "
          "verdict, the portfolio plugin's release refused by its sidecar; a placement before the "
          "venue was linked refused CONNECTIVITY_NOT_ALLOWED and recorded; linked and mapped, "
          "claimed, placed, acknowledged and filled in two, the position up by 10 with its lot and "
          "its pending settlement, the unlinked connectivity plugin hearing nothing; a child order "
          "released within its parent, its fill the parent's; a release outside the desk's write "
          "scope denied with no code; a stop's cancel reported by the venue at once and recorded, "
          "a placement under it held back by the venue, a plugin admin refused All accounts; two "
          "unplaced facts, one naming no account and counted on the venue's admin view; two "
          "venue-originated fills waiting unmatched, one accepted to a new order at filled, one "
          "answered DK by the ems's tool, the venue hearing both, a later fill waiting on its own; "
          "EUR/USD's spot date Tuesday 13 October, by the platform's IDs, the deployment's own; "
          "a block across two funds booked and handed off, each member filled by its allocation; "
          "an overfill's excess booked to the error account; a correction by replace after the "
          "hand-off; execution costs recorded by the analytics component from the order "
          "path's orders, placements, executions and history, once; and meridian-bor rebuild "
          "reproducing the book "
          "line for line")


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
