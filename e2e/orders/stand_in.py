"""Core's stand-ins for `make e2e-orders` (contract v22, W11): the order path's
plugins on the Python SDK, each answering its page as JSON so the harness's
runner drives it (`page --level ... PATH`), acting for the person the
session's `Meridian-Caller` header names. SIMULATED ONLY: nothing here
reaches a broker, a brokerage or any trading API; the venue is this file.

STAND_IN_ORDERS=pm is a `portfolio` plugin:

  /propose?account=&instrument=&quantity=[&cash=][&side=][&tif=][&key=]
                                  ProposeOrder (W11.1), for the person;
  /release?order=                 ReleaseOrder, which a portfolio plugin does
                                  not hold: the sidecar's refusal said back;
  /change?order=&quantity=        RequestOrderChange, a quantity change;
  /orders                         ListOrders, a line each.

STAND_IN_ORDERS=desk holds `oms` and `ems`:

  /release?order=, /claim?order=, /cancel?order=, /check... (none)
  /place?order=&quantity=&connectivity=&code=   CreatePlacement
  /child?parent=&quantity=&account=&instrument= a split child (W11.1)
  /map?account=&code=&connectivity=             MapVenueAccount, at admin
  /orders, /heard                               what it reads and hears

STAND_IN_ORDERS=comp is `compliance`: /check?order=&verdict=pass|block|refuse.

STAND_IN_ORDERS=venue is a connectivity plugin, the simulated venue. It hears
each placement naming its instance (matrix/directed.tsv) and works it: a
placement of at most 100 is acknowledged and filled in two, 40% then the
rest; a larger one is acknowledged and left working. Every fill states its
settlement date, the next weekday after its trade date, as a venue states
one. It honours the trading instructions as a connectivity plugin must: under
a pause or a stop on its instance or All accounts it sends nothing new, and a
stop cancels each placement it is working at once, reporting each cancel as a
fact naming the stop's entry (W11.9; v21's Q17), then the venue's
confirmation. It serves:

  /resolve?symbol=S               resolves its symbol (W3.1), minting the
                                  deployment's record, answering its ID;
  /heard                          the placements heard, by ID, and what it did;
  /astray?code=                   an execution naming a placement nobody
                                  holds, for the book to keep unplaced.
"""
import asyncio
import base64
import http.server
import json
import os
import threading
import time
import urllib.parse
from datetime import date, datetime, timedelta, timezone
from decimal import Decimal

import grpc
import meridian
from meridian import Interface, Page
from meridian.plugin.v1 import operations_pb2 as ops
from meridian.v1 import sidecar_pb2

KIND = os.environ.get("STAND_IN_ORDERS", "pm")
PORT = 8088
LOOP = asyncio.new_event_loop()
PRICE = "227.41"


def log(**said):
    print("ORDERS " + json.dumps(said, sort_keys=True, default=str), flush=True)


def decimal(text):
    """A decimal as the wire carries it: a 128-bit integer, its upper half
    signed, and its scale."""
    exact = Decimal(str(text))
    scale = max(0, -exact.as_tuple().exponent)
    units = int(exact.scaleb(scale))
    return ops.Decimal(high=units >> 64, low=units & ((1 << 64) - 1), scale=scale)


def number(message):
    units = (message.high << 64) | message.low
    if message.high < 0:
        units -= 1 << 128
    return str(Decimal(units).scaleb(-message.scale))


def usd(text):
    return ops.Money(amount=decimal(text), currency_code="USD")


def assertion(header):
    """The person a command is sent for, from the Meridian-Caller header."""
    if not header:
        return None
    padded = header + "=" * (-len(header) % 4)
    return sidecar_pb2.CallerAssertion.FromString(base64.urlsafe_b64decode(padded))


def refused(failed):
    """A refusal as the run reads it: the status, the reason code and the
    fields named, and the words."""
    said = {"status": failed.code().name, "words": failed.details()}
    for key, value in failed.trailing_metadata() or ():
        if key == "meridian-refusal-bin":
            refusal = sidecar_pb2.Refusal.FromString(value)
            said["reason"] = sidecar_pb2.RefusalReason.Name(refusal.reason)
            said["fields"] = list(refusal.fields)
    return {"refused": said}


def state(order):
    return ops.OrderState.Name(order.status.state)


def order_json(order):
    q = order.status.quantities
    return {
        "order_id": order.order_id, "account_id": order.account_id, "state": state(order),
        "conditions": [ops.OrderCondition.Name(c.condition) for c in order.status.conditions],
        "ordered": number(q.ordered), "reported_filled": number(q.reported_filled),
        "committed": number(q.committed), "idle": number(q.idle), "leaves": number(q.leaves),
        "parent": order.link.parent_order_id,
        "placements": [p.placement_id for p in order.placements],
    }


def one(query, name, default=""):
    return query.get(name, [default])[0]


class Plugin:
    def __init__(self, plugin):
        self.plugin = plugin
        self.ops = plugin._operations()
        self.heard = []

    async def call(self, operation, params):
        try:
            return await getattr(self.ops, operation)(params)
        except grpc.aio.AioRpcError as failed:
            raise Refusal(refused(failed)) from None

    async def orders(self, query):
        listed = await self.call("ListOrders", ops.ListOrdersParams(
            account_id=one(query, "account"), page_size=500))
        return {"orders": [order_json(o) for o in listed.orders],
                "placements": [{"placement_id": p.placement_id, "order_id": p.order_id,
                                "state": ops.PlacementState.Name(p.status.state),
                                "connectivity": p.connectivity_instance_id}
                               for p in listed.placements],
                "executions": len(listed.executions),
                "unplaced": [{"account_id": u.account_id, "reason": ops.UnplacedReason.Name(u.reason)}
                             for u in listed.unplaced_facts]}


class Refusal(Exception):
    def __init__(self, said):
        super().__init__(json.dumps(said))
        self.said = said


# ── portfolio ───────────────────────────────────────────────────────────────


class Pm(Plugin):
    async def answer(self, path, query, caller):
        if path == "/propose":
            params = ops.ProposeOrderParams(
                account_id=one(query, "account"),
                instrument_id=one(query, "instrument"),
                side=ops.Side.Value("SIDE_" + one(query, "side", "buy").upper()),
                order_type=ops.OrderType.Value("ORDER_TYPE_" + one(query, "type", "limit").upper()),
                time_in_force=ops.TimeInForce.Value(
                    "TIME_IN_FORCE_" + one(query, "tif", "day").upper()),
                idempotency_key=one(query, "key"),
                acting_for=caller)
            if one(query, "quantity"):
                params.quantity.CopyFrom(decimal(one(query, "quantity")))
            if one(query, "cash"):
                params.cash_quantity.CopyFrom(usd(one(query, "cash")))
            if params.order_type == ops.OrderType.ORDER_TYPE_LIMIT:
                params.limit_price.CopyFrom(usd("230.00"))
            done = await self.call("ProposeOrder", params)
            return order_json(done.order)
        if path == "/release":
            done = await self.call("ReleaseOrder", ops.ReleaseOrderParams(
                order_id=one(query, "order"), acting_for=caller))
            return order_json(done.order)
        if path == "/change":
            done = await self.call("RequestOrderChange", ops.RequestOrderChangeParams(
                order_id=one(query, "order"),
                change=ops.OrderChange(kind=ops.OrderChangeKind.ORDER_CHANGE_KIND_QUANTITY,
                                       quantity=decimal(one(query, "quantity"))),
                reason=one(query, "reason"), acting_for=caller))
            return order_json(done.order)
        if path == "/orders":
            return await self.orders(query)
        return {"page": "the portfolio stand-in"}


# ── oms and ems ─────────────────────────────────────────────────────────────


class Desk(Plugin):
    def __init__(self, plugin):
        super().__init__(plugin)
        self.instructions = []

    async def on_instruction(self, heard):
        held = heard.message.instruction
        self.instructions.append(ops.TradingInstructionValue.Name(held.value))

    async def answer(self, path, query, caller):
        order = one(query, "order")
        if path == "/release":
            done = await self.call("ReleaseOrder", ops.ReleaseOrderParams(
                order_id=order, acting_for=caller))
            return order_json(done.order)
        if path == "/claim":
            done = await self.call("SetWorkingCondition", ops.SetWorkingConditionParams(
                order_id=order, condition=ops.OrderCondition.ORDER_CONDITION_CLAIMED, set=True,
                acting_for=caller))
            return order_json(done.order)
        if path == "/cancel":
            done = await self.call("RequestOrderChange", ops.RequestOrderChangeParams(
                order_id=order,
                change=ops.OrderChange(kind=ops.OrderChangeKind.ORDER_CHANGE_KIND_CANCEL),
                reason="cancelled by the desk", acting_for=caller))
            return order_json(done.order)
        if path == "/change":
            done = await self.call("RequestOrderChange", ops.RequestOrderChangeParams(
                order_id=order,
                change=ops.OrderChange(kind=ops.OrderChangeKind.ORDER_CHANGE_KIND_QUANTITY,
                                       quantity=decimal(one(query, "quantity"))),
                reason=one(query, "reason"), acting_for=caller))
            return order_json(done.order)
        if path == "/place":
            done = await self.call("CreatePlacement", ops.CreatePlacementParams(
                order_id=order, connectivity_instance_id=one(query, "connectivity"),
                venue_account_code=one(query, "code"), quantity=decimal(one(query, "quantity")),
                limit_price=usd("230.00"), acting_for=caller))
            return {"placement_id": done.placement.placement_id,
                    "state": ops.PlacementState.Name(done.placement.status.state)}
        if path == "/child":
            done = await self.call("ProposeOrder", ops.ProposeOrderParams(
                account_id=one(query, "account"), instrument_id=one(query, "instrument"),
                side=ops.Side.SIDE_BUY, quantity=decimal(one(query, "quantity")),
                order_type=ops.OrderType.ORDER_TYPE_LIMIT, limit_price=usd("230.00"),
                time_in_force=ops.TimeInForce.TIME_IN_FORCE_DAY,
                link=ops.OrderLink(kind=ops.OrderLinkKind.ORDER_LINK_KIND_SPLIT,
                                   parent_order_id=one(query, "parent")),
                acting_for=caller))
            return order_json(done.order)
        if path == "/map":
            await self.call("MapVenueAccount", ops.MapVenueAccountParams(
                connectivity_instance_id=one(query, "connectivity"),
                venue_account_code=one(query, "code"), account_id=one(query, "account"),
                acting_for=caller))
            return {"mapped": one(query, "code")}
        if path == "/orders":
            return await self.orders(query)
        if path == "/heard":
            return {"instructions": self.instructions}
        return {"page": "the oms and ems stand-in"}


# ── compliance ──────────────────────────────────────────────────────────────


class Comp(Plugin):
    async def answer(self, path, query, caller):
        if path == "/check":
            done = await self.call("RecordCheck", ops.RecordCheckParams(
                order_id=one(query, "order"),
                verdict=ops.CheckVerdict.Value("CHECK_VERDICT_" + one(query, "verdict", "pass").upper()),
                rule="e2e", note="the compliance stand-in's verdict", acting_for=caller))
            return order_json(done.order)
        return {"page": "the compliance stand-in"}


# ── the simulated venue ─────────────────────────────────────────────────────


def settles(trade_date):
    """The next weekday: the date this simulated venue states."""
    day = trade_date + timedelta(days=1)
    while day.weekday() >= 5:
        day += timedelta(days=1)
    return day


class Venue(Plugin):
    def __init__(self, plugin):
        super().__init__(plugin)
        self.me = plugin.identity.instance_id
        self.worked = {}
        self.halted = None
        self.sent = 0

    def raw(self, key):
        return self.plugin.raw_record(key)

    def holds_back(self):
        return self.halted in ("TRADING_INSTRUCTION_VALUE_PAUSE", "TRADING_INSTRUCTION_VALUE_STOP")

    async def report(self, placement_id, event, **fields):
        return await self.call("ReportPlacement", ops.ReportPlacementParams(
            placement_id=placement_id, event=event, raw_record=self.raw(f"orders/{placement_id}/{event}"),
            **fields))

    async def fill(self, placement, quantity, cumulative):
        today = datetime.now(timezone.utc).date()
        await self.call("ReportExecution", ops.ReportExecutionParams(
            placement_id=placement.placement_id,
            venue_account_code=placement.venue_account_code,
            venue_execution_id=f"SIM-{placement.placement_id}:{cumulative}",
            kind=ops.ExecutionKind.EXECUTION_KIND_VENUE_REPORTED,
            source=ops.ExecutionSource.EXECUTION_SOURCE_PRIMARY,
            quantity=decimal(quantity), price=usd(PRICE),
            trade_time_ns=time.time_ns(), trade_date=today.isoformat(),
            finality=ops.Finality(level=ops.FinalityLevel.FINALITY_LEVEL_FINAL, at_ns=time.time_ns()),
            settlement_date=settles(today).isoformat(),
            raw_record=self.raw(f"fills/{placement.placement_id}/{cumulative}")))

    async def on_placement(self, heard):
        placement = heard.message.placement
        held = {"placement_id": placement.placement_id,
                "state": ops.PlacementState.Name(placement.status.state)}
        self.heard.append(held)
        if placement.connectivity_instance_id != self.me:
            held["done"] = "not ours: heard in error"
            return
        if placement.status.state != ops.PlacementState.PLACEMENT_STATE_QUEUED:
            return
        if placement.placement_id in self.worked:
            return
        if self.holds_back():
            held["done"] = "held back: trading paused or stopped"
            return
        self.worked[placement.placement_id] = placement
        self.sent += 1
        try:
            await self.report(placement.placement_id, ops.PlacementEvent.PLACEMENT_EVENT_ACKNOWLEDGED,
                              venue_order_id=f"SIM-ORD-{self.sent}", routed_at_ns=time.time_ns())
            quantity = Decimal(number(placement.quantity))
            if quantity <= 100:
                first = (quantity * Decimal("0.4")).quantize(Decimal(1))
                await self.fill(placement, first, first)
                await self.fill(placement, quantity - first, quantity)
                held["done"] = "filled"
            else:
                held["done"] = "working"
        except Refusal as failed:
            held["done"] = failed.said

    async def on_instruction(self, heard):
        held = heard.message.instruction
        target = held.target
        ours = target.all_accounts or target.instance_id == self.me
        if not ours:
            return
        self.halted = ops.TradingInstructionValue.Name(held.value)
        if held.value != ops.TradingInstructionValue.TRADING_INSTRUCTION_VALUE_STOP:
            return
        # A stop's cancels never wait (v21's Q17): sent at the venue at once,
        # each reported as a fact naming the stop's entry, then confirmed.
        for placement_id in list(self.worked):
            try:
                await self.call("RequestPlacementChange", ops.RequestPlacementChangeParams(
                    placement_id=placement_id,
                    change=ops.PlacementChange(kind=ops.PlacementChangeKind.PLACEMENT_CHANGE_KIND_CANCEL),
                    cause=ops.EntryMeta(entry_id=held.entry.entry_id)))
                await self.report(placement_id, ops.PlacementEvent.PLACEMENT_EVENT_CANCELLED)
            except Refusal as failed:
                log(cancel_refused=failed.said)
            self.worked.pop(placement_id, None)

    async def answer(self, path, query, caller):
        if path == "/resolve":
            # The venue's symbol, resolved at the edge by the fact's date
            # (W3.1, granted to connectivity in v22), minting the record.
            found = await self.call("ResolveIdentifier", ops.ResolveIdentifierParams(
                identifiers=[ops.Identifier(scheme="symbol", value=one(query, "symbol"), source="sim")]))
            return {"instrument_id": found.instrument_id}
        if path == "/heard":
            return {"heard": self.heard, "halted": self.halted, "working": sorted(self.worked)}
        if path == "/astray":
            today = datetime.now(timezone.utc).date()
            done = await self.call("ReportExecution", ops.ReportExecutionParams(
                placement_id="PLC-NOBODYHOLDSTHIS0000000000",
                venue_account_code=one(query, "code"),
                venue_execution_id=f"SIM-ASTRAY-{one(query, 'code')}",
                kind=ops.ExecutionKind.EXECUTION_KIND_VENUE_REPORTED,
                quantity=decimal("1"), price=usd(PRICE), trade_date=today.isoformat(),
                settlement_date=settles(today).isoformat(),
                raw_record=self.raw(f"fills/astray/{one(query, 'code')}")))
            return {"unplaced": {"account_id": done.unplaced.account_id,
                                 "reason": ops.UnplacedReason.Name(done.unplaced.reason)}}
        return {"page": "the simulated venue"}


# ── Serving the page ────────────────────────────────────────────────────────


def serve(served):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            parsed = urllib.parse.urlparse(self.path)
            caller = assertion(self.headers.get("Meridian-Caller"))
            future = asyncio.run_coroutine_threadsafe(
                served.answer(parsed.path, urllib.parse.parse_qs(parsed.query), caller), LOOP)
            try:
                body, status = json.dumps(future.result(timeout=60), sort_keys=True), 200
            except Refusal as failed:  # the sidecar's refusal, for the run to read
                body, status = json.dumps(failed.said, sort_keys=True), 200
            except Exception as failed:  # said on the page, for the run to quote
                body, status = json.dumps({"failed": f"{type(failed).__name__}: {failed}"}), 500
            data = body.encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, *_args):
            pass

    http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()


async def main():
    title, kind, levels = {
        "pm": ("Portfolio stand-in", Pm, ["write", "admin"]),
        "desk": ("Desk stand-in", Desk, ["write", "admin"]),
        "comp": ("Compliance stand-in", Comp, ["write", "admin"]),
        "venue": ("Simulated venue", Venue, ["write", "admin"]),
    }[KIND]
    roles = {"desk": ["oms", "ems"]}.get(KIND, [])
    plugin = await meridian.connect(
        interface=Interface(PORT, title, pages=[Page("/", title, levels=levels, roles=roles)]))
    served = kind(plugin)
    # Heard through the SDK's receive (W4.3): seeded by one read at start,
    # nothing handed on for it, then each change in order.
    if KIND == "venue":
        asyncio.ensure_future(plugin.receive(placement_updated=served.on_placement,
                                             trading_instruction_changed=served.on_instruction,
                                             seed=False))
    elif KIND == "desk":
        asyncio.ensure_future(plugin.receive(trading_instruction_changed=served.on_instruction,
                                             seed=False))
    log(registered=plugin.identity.instance_id, kind=KIND)
    threading.Thread(target=serve, args=(served,), daemon=True).start()
    while True:
        await asyncio.sleep(3600)


if __name__ == "__main__":
    asyncio.set_event_loop(LOOP)
    LOOP.run_until_complete(main())
