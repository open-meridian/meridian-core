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

From contract v23 the desk is the ems's page for venue-originated fills
(W11.23, W11.24): /fills is the page a person opens, the unmatched fills in
its accounts on the kit, one line each on its pager, each with Accept (to an
order, or to a new order with its sleeve) and DK (a reason from the closed
list, text with "other"), and "the venue is not told" before a DK where the
fill's venue takes no acknowledgement; posted, each acts for the person.
Under parity its tools are the ems's, accept_execution and dk_execution,
and the page's read read_unmatched_fills, each at write on `ems`. For the
run: /unmatched, /accept?executions=&order=|&new=1[&sleeve=],
/dk?executions=&reason=[&text=].

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

  /resolve?symbol=S[&figi=F][&cusip=C][&description=D][&thin=1]
                                  resolves its symbol, with the FIGI and a
                                  CUSIP beside it where given (W3.1), stating
                                  a description and USD unless thin (from
                                  v24 the minimum a record needs), answering
                                  its ID and whether it is provisional;
  /settle?instrument=&trade=      asks the date read for the instrument's
                                  settlement date from a trade date (W3.20),
                                  answering the date, outcome and rule;
  /heard                          the placements heard, by ID, and what it did;
  /astray?code=                   an execution naming a placement nobody
                                  holds, for the book to keep unplaced;
  /originate?code=&instrument=&quantity=&side=&order=
                                  a fill on no order and no placement, as a
                                  broker reports one placed in its own app
                                  (contract v23, W11.11), answering the
                                  execution's ID and state;
  /answers                        each execution it reported, as it heard
                                  it answered: matched or DK (ruling 15);
  /spot?pair=..,..&trade=         a pair's spot date from a trade date
                                  (W3.20), by the platform's INS- IDs or the
                                  deployment's own LCL- ones, answering the
                                  date and rule;
  /resolve?iso4217=               a currency's record by its ISO 4217 code.
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

import html

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


def split(text):
    return [part for part in text.replace(" ", "").split(",") if part]


def fill_json(execution):
    return {"execution_id": execution.execution_id, "account_id": execution.account_id,
            "instrument_id": execution.instrument_id, "side": ops.Side.Name(execution.side),
            "quantity": number(execution.quantity) if execution.HasField("quantity") else "",
            "state": ops.ExecutionState.Name(execution.state),
            "venue_order_id": execution.venue_order_id, "order_id": execution.order_id,
            "placement_id": execution.placement_id, "trade_date": execution.trade_date,
            "dk_reason": ops.DkReason.Name(execution.dk_reason)}


# ── Contract v23: the ems's page for venue-originated fills ────────────────

KIT = "/.meridian/ui/0.9.0/"
CODE_MAPPED = "SIM-ACCT-1"
DK_REASONS = [
    ("unknown_instrument", "Unknown instrument"), ("wrong_side", "Wrong side"),
    ("quantity_exceeds_order", "Quantity exceeds order"),
    ("no_matching_order", "No matching order"), ("price_exceeds_limit", "Price exceeds limit"),
    ("calculation_difference", "Calculation difference"), ("other", "Other"),
]


# Each column sized so a row's actions keep both buttons on one line; the
# kit's `wide-only` columns step aside at a phone's width.
FILLS_CSS = (
    "table.fills th.side{width:3.75rem}table.fills th.num{width:5rem}"
    "table.fills th.actions{width:9rem}table.fills td.actions{text-align:right;overflow:visible}"
)


# The dialogs and the fields shown on a selection, the page's own: the kit
# draws the pager and the status, and opens no dialog of a plugin's.
FILLS_JS = """
document.addEventListener("click", function (event) {
  var opener = event.target.closest("[data-dialog-open]");
  if (opener) {
    var dialog = document.getElementById(opener.dataset.dialogOpen);
    var fill = JSON.parse(opener.dataset.fill || "{}");
    Object.keys(fill).forEach(function (name) {
      var field = dialog.querySelector('[name="' + name + '"]');
      if (field) { field.value = fill[name]; }
    });
    var title = dialog.querySelector("h2");
    if (title && opener.dataset.title) { title.textContent = opener.dataset.title; }
    shown(dialog);
    dialog.showModal();
  }
  var closer = event.target.closest("[data-dialog-close]");
  if (closer) { closer.closest("dialog").close(); }
});
function shown(root) {
  root.querySelectorAll("[data-om-applies-when]").forEach(function (label) {
    var form = label.closest("form");
    var name = label.dataset.omAppliesWhen;
    var chosen = form.querySelector('[name="' + name + '"]:checked') ||
      form.querySelector('select[name="' + name + '"]');
    var value = chosen ? chosen.value : "";
    label.hidden = label.dataset.omOneOf.split(",").indexOf(value) < 0;
  });
}
document.addEventListener("change", function () { shown(document); });
shown(document);
"""


def fills_html(fills, told, said, code):
    """The unmatched fills on the kit: one line each on its pager, Accept and
    DK in dialogs, the page fitting one screen at a desk and on a phone."""
    e = html.escape
    rows = "".join(
        f"<tr data-id=\"{e(f.execution_id)}\"><td title=\"{e(f.execution_id)}\">{e(f.execution_id)}</td>"
        f"<td class=\"wide-only\">{e(f.account_id)}</td>"
        f"<td class=\"wide-only\" title=\"{e(f.instrument_id)}\">{e(f.instrument_id)}</td>"
        f"<td>{e(ops.Side.Name(f.side).removeprefix('SIDE_').replace('_', ' ').lower())}</td>"
        f"<td class=\"num\">{e(number(f.quantity))}</td>"
        f"<td class=\"wide-only\">{e(f.venue_order_id)}</td><td class=\"wide-only\">{e(f.trade_date)}</td>"
        f"<td class=\"actions\"><button type=\"button\" data-dialog-open=\"accept\" "
        f"data-title=\"Accept {e(f.execution_id)}\" data-fill=\"{e(json.dumps({'execution_ids': f.execution_id}))}\">Accept</button> "
        f"<button type=\"button\" data-dialog-open=\"dk\" data-title=\"DK {e(f.execution_id)}\" "
        f"data-fill=\"{e(json.dumps({'execution_ids': f.execution_id}))}\">DK</button></td></tr>"
        for f in fills)
    table = (
        "<p class=\"empty\" data-fills=\"0\">No unmatched fill: a fill your venues report on no "
        "order and no placement waits here for you to accept it or answer it DK.</p>"
        if not fills else
        f"<om-pager search=\"Search fills\"><table class=\"list one-line fills\" data-fills=\"{len(fills)}\">"
        "<thead><tr><th class=\"id\">Execution</th><th class=\"wide-only\">Account</th><th class=\"wide-only\">Instrument</th><th class=\"side\">Side</th>"
        "<th class=\"num\">Quantity</th><th class=\"wide-only\">Venue order</th><th class=\"wide-only\">Traded</th>"
        f"<th class=\"actions\"></th></tr></thead><tbody>{rows}</tbody></table></om-pager>")
    reasons = "".join(f"<option value=\"{v}\">{e(label)}</option>" for v, label in DK_REASONS)
    not_told = "" if told else (
        "<p class=\"hint\" data-not-told>The venue is not told: it takes no acknowledgement of "
        "a fill it originated.</p>")
    notice = f"<p class=\"notice\" role=\"status\">{e(said)}</p>" if said else ""
    state = "warn" if fills else "ok"
    return (
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">"
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">"
        f"<title>Unmatched fills</title><link rel=\"stylesheet\" href=\"{KIT}meridian.css\">"
        f"<script src=\"{KIT}meridian.js\"></script><style>{FILLS_CSS}</style></head><body><main class=\"page\">"
        "<header class=\"page-head\"><div><h1>Unmatched fills</h1></div>"
        f"<om-status data-om-header state=\"{state}\" label=\"{len(fills)} unmatched\"></om-status></header>"
        f"{notice}{table}"
        "<dialog id=\"accept\" aria-labelledby=\"accept-title\"><form method=\"post\" action=\"/fills/accept\">"
        "<div class=\"dialog-head\"><h2 id=\"accept-title\">Accept</h2></div><div class=\"dialog-body\">"
        "<input type=\"hidden\" name=\"execution_ids\">"
        "<label class=\"check\"><input type=\"radio\" name=\"to\" value=\"new\" checked> To a new order</label>"
        "<label class=\"check\"><input type=\"radio\" name=\"to\" value=\"order\"> To an order of yours</label>"
        "<label data-om-applies-when=\"to\" data-om-one-of=\"order\">Order <input name=\"order_id\" placeholder=\"ORD-...\"></label>"
        "<label data-om-applies-when=\"to\" data-om-one-of=\"new\">Sleeve <input name=\"sleeve\" placeholder=\"The account's unsleeved part\"></label>"
        "<label>Note <input name=\"note\" maxlength=\"2000\" placeholder=\"Why\"></label></div>"
        "<div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>"
        "<button type=\"submit\" class=\"primary\">Accept</button></div></form></dialog>"
        "<dialog id=\"dk\" aria-labelledby=\"dk-title\"><form method=\"post\" action=\"/fills/dk\">"
        "<div class=\"dialog-head\"><h2 id=\"dk-title\">DK</h2></div><div class=\"dialog-body\">"
        "<input type=\"hidden\" name=\"execution_ids\">"
        f"<label>Reason <select name=\"reason\" required>{reasons}</select></label>"
        "<label data-om-applies-when=\"reason\" data-om-one-of=\"other\">Text <input name=\"text\" maxlength=\"2000\" placeholder=\"Why, as the venue reads it\"></label>"
        f"{not_told}</div><div class=\"dialog-foot\"><button type=\"button\" data-dialog-close>Cancel</button>"
        "<button type=\"submit\" class=\"primary\">DK</button></div></form></dialog>"
        f"<script>{FILLS_JS}</script></main></body></html>")


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
        if path == "/tca":
            return await self.tca(query)
        return {"page": "the portfolio stand-in"}

    async def tca(self, query):
        """Contract v27 (W10.11, W12.1; ruling 510): an execution_cost job on
        an account, which the analytics component answers from the book's
        orders, placements, executions and the orders' history and records
        in the lake; then the account's rows, as ListExecutionCosts reads
        them."""
        account = one(query, "account")
        ran = await self.call("RunJob", ops.RunJobParams(job=ops.Job(
            account_ids=[account],
            measures=[ops.MeasureRef(definition="execution_cost", version=1)])))
        listed = await self.call("ListExecutionCosts", ops.ListExecutionCostsParams(
            account_id=account, page_size=500))
        return {
            "answered_inline": ran.answered_inline,
            "rows": [{"row_key": c.meta.row_key, "execution_id": c.execution_id,
                      "order_id": c.order_id, "placement_id": c.placement_id,
                      "benchmark": ops.TcaBenchmark.Name(c.benchmark),
                      "benchmark_at_ns": c.benchmark_at_ns,
                      "unanswered": ops.TcaUnanswered.Name(c.unanswered),
                      "version": c.meta.version}
                     for c in listed.costs],
        }


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
        if path == "/block":
            # Contract v24 (W11.1): a block order in the allocation account,
            # its members the funds' released orders, released on creation.
            done = await self.call("ProposeOrder", ops.ProposeOrderParams(
                account_id=one(query, "account"), instrument_id=one(query, "instrument"),
                side=ops.Side.SIDE_BUY, order_type=ops.OrderType.ORDER_TYPE_LIMIT,
                limit_price=usd("230.00"), time_in_force=ops.TimeInForce.TIME_IN_FORCE_DAY,
                block_member_order_ids=split(one(query, "members")), acting_for=caller))
            return order_json(done.order)
        if path == "/book":
            # Contract v24 (W11.27): an order's executions booked to their
            # accounts, each allocation ACCOUNT:QUANTITY:GROSS[:error], and
            # handed off where asked.
            allocations = []
            for said in split(one(query, "alloc")):
                account, quantity, gross, *error = said.split(":")
                allocation = ops.Allocation(account_id=account, quantity=decimal(quantity),
                                            average_price=usd(one(query, "price", PRICE)),
                                            gross_amount=usd(gross))
                if error:
                    allocation.error_cause = ops.ErrorCause.ERROR_CAUSE_OVERFILL
                    allocation.reason = "the venue filled more than ordered"
                allocations.append(allocation)
            done = await self.call("BookOrder", ops.BookOrderParams(
                order_id=order, execution_ids=split(one(query, "executions")),
                allocations=allocations, hand_off=bool(one(query, "handoff")), acting_for=caller))
            return booking_json(done.booking)
        if path == "/handoff":
            done = await self.call("HandOffBooking", ops.HandOffBookingParams(
                booking_id=one(query, "booking"), acting_for=caller))
            return booking_json(done.booking)
        if path == "/executions":
            listed = await self.call("ListOrders", ops.ListOrdersParams(
                account_id=one(query, "account"), page_size=500))
            return {"executions": [fill_json(e) for e in listed.executions
                                   if not order or e.order_id == order]}
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
        # Contract v23 (W11.23, W11.24): the unmatched fills, answered.
        if path == "/unmatched":
            return {"fills": [fill_json(e) for e in await self.unmatched(one(query, "account"))]}
        if path == "/accept":
            return await self.accept(split(one(query, "executions")), one(query, "order"),
                                     bool(one(query, "new")), one(query, "sleeve"),
                                     one(query, "note"), caller)
        if path == "/dk":
            return await self.dont_know(split(one(query, "executions")), one(query, "reason"),
                                        one(query, "text"), caller)
        return {"page": "the oms and ems stand-in"}

    async def unmatched(self, account=""):
        listed = await self.call("ListOrders", ops.ListOrdersParams(
            account_id=account, page_size=500,
            execution_state=ops.ExecutionState.EXECUTION_STATE_UNMATCHED))
        return list(listed.executions)

    async def accept(self, ids, order, new, sleeve, note, caller):
        params = ops.AcceptExecutionParams(execution_ids=ids, note=note, acting_for=caller)
        if new or not order:
            params.new_order.CopyFrom(ops.NewOrderFromExecutions(sleeve=sleeve))
        else:
            params.order_id = order
        done = await self.call("AcceptExecution", params)
        return {"order": order_json(done.order),
                "placement": {"placement_id": done.placement.placement_id,
                              "state": ops.PlacementState.Name(done.placement.status.state),
                              "routed_at_ns": done.placement.routed_at_ns,
                              "connectivity": done.placement.connectivity_instance_id},
                "executions": [fill_json(e) for e in done.executions]}

    async def dont_know(self, ids, reason, text, caller):
        done = await self.call("DontKnowExecution", ops.DontKnowExecutionParams(
            execution_ids=ids, reason=ops.DkReason.Value("DK_REASON_" + (reason or "other").upper()),
            text=text, acting_for=caller))
        return {"executions": [fill_json(e) for e in done.executions]}

    async def venue_told(self, connectivity, code):
        """Whether the fill's venue takes an acknowledgement, by its catalogue
        (W11.20): unspecified, or none published, reads as none."""
        try:
            read = await self.call("ReadVenueCatalogue", ops.ReadVenueCatalogueParams(
                connectivity_instance_id=connectivity, venue_account_code=code))
        except Refusal:
            return False
        said = read.catalogue.execution_acknowledgement
        return said in (ops.ExecutionAcknowledgement.EXECUTION_ACKNOWLEDGEMENT_BN,
                        ops.ExecutionAcknowledgement.EXECUTION_ACKNOWLEDGEMENT_DONT_KNOW_TRADE)

    async def fills_page(self, caller, said=""):
        fills = await self.unmatched()
        told = await self.venue_told("venue", CODE_MAPPED) if fills else False
        return fills_html(fills, told, said, CODE_MAPPED)

    async def tool(self, path, asked, caller):
        """A tool's call (W6.20, contract v12): its arguments as JSON, its
        answer typed, outcome made, unchanged or refused."""
        try:
            if path == "/fills/read":
                return {"outcome": "unchanged", "data": {
                    "fills": [fill_json(e) for e in await self.unmatched(asked.get("account_id", ""))]}}
            ids = asked.get("execution_ids") or []
            if path == "/fills/accept":
                new = asked.get("new_order")
                done = await self.accept(ids, asked.get("order_id", ""), new is not None,
                                         (new or {}).get("sleeve", ""), asked.get("note", ""), caller)
            else:
                done = await self.dont_know(ids, asked.get("reason", ""), asked.get("text", ""), caller)
            return {"outcome": "made", "data": done}
        except Refusal as failed:
            said = failed.said["refused"]
            return {"outcome": "refused", "reason": said.get("reason", "refused").lower(),
                    "fields": [{"path": field} for field in said.get("fields", [])],
                    "detail": said.get("words", "")}

    async def form(self, path, fields, caller):
        """The page's forms, posted by a person."""
        ids = split(fields.get("execution_ids", ""))
        try:
            if path == "/fills/accept":
                await self.accept(ids, fields.get("order_id", ""), fields.get("to") == "new",
                                  fields.get("sleeve", ""), fields.get("note", ""), caller)
                said = f"Accepted {', '.join(ids)}."
            else:
                await self.dont_know(ids, fields.get("reason", ""), fields.get("text", ""), caller)
                said = f"Answered {', '.join(ids)} DK."
        except Refusal as failed:
            said = "Refused: " + failed.said["refused"].get("words", "")
        return await self.fills_page(caller, said)


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
        self.answers = []

    async def on_execution(self, heard):
        """Each execution it reported, as the book answered it (ruling 15):
        delivered at this instance by its raw record alone."""
        execution = heard.message.execution
        if execution.raw_record.instance_id != self.me:
            self.answers.append({"execution_id": execution.execution_id, "not_ours": True})
            return
        self.answers.append(fill_json(execution))

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
                if placement.venue_account_code.endswith("-OVER"):
                    # Contract v24's run: a venue that fills two more than
                    # placed, the excess for the error account (W11.27).
                    await self.fill(placement, Decimal(2), quantity + 2)
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
            identifiers = [ops.Identifier(scheme="symbol", value=one(query, "symbol"), source="sim")]
            if one(query, "iso4217"):
                # A currency by its ISO 4217 code: the deployment's own record,
                # the platform's ID beside it (contract v20, W3.1).
                identifiers = [ops.Identifier(scheme="iso4217", value=one(query, "iso4217"))]
            if one(query, "figi"):
                identifiers.append(ops.Identifier(scheme="figi", value=one(query, "figi")))
            if one(query, "cusip"):
                # A licensed scheme (contract v24, W3.25): kept in the
                # deployment, never sent to the platform.
                identifiers.append(ops.Identifier(scheme="cusip", value=one(query, "cusip")))
            # What the venue states of it: from contract v24 a record needs an
            # identifier, a description and a currency (W3.7), and a source
            # thinner than that is not found, nothing minted.
            thin = bool(one(query, "thin"))
            found = await self.call("ResolveIdentifier", ops.ResolveIdentifierParams(
                identifiers=identifiers,
                stated_description="" if thin else one(
                    query, "description", f"{one(query, 'symbol')} on the simulated venue"),
                stated_currency="" if thin or one(query, "iso4217") else one(query, "currency", "USD")))
            return {"instrument_id": found.instrument_id, "found": found.found,
                    "minted": found.minted, "provisional": getattr(found, "provisional", False)}
        if path == "/settle":
            answered = await self.call("ComputeDate", ops.ComputeDateParams(
                settlement=ops.SettlementDateInputs(instrument_id=one(query, "instrument"),
                                                    trade_date=one(query, "trade"))))
            rule = answered.basis.rule
            return {"date": answered.date, "outcome": ops.DateOutcome.Name(answered.outcome),
                    "rule": f"{rule.subject} v{rule.version}" if rule.subject else ""}
        if path == "/heard":
            return {"heard": self.heard, "halted": self.halted, "working": sorted(self.worked)}
        if path == "/answers":
            return {"answers": self.answers}
        if path == "/spot":
            # Contract v23: a pair's spot date (W3.20), by the two cash
            # instruments' IDs: the platform's, or the deployment's own. Until
            # the one-ID revision a deployment keeps a currency under a local
            # ID with the platform's beside it (the 2026-10-11 ruling), a rule
            # pulled for a pair names the platform's, and the read translates
            # the deployment's (approved 2026-10-13).
            pair = split(one(query, "pair"))
            answered = await self.call("ComputeDate", ops.ComputeDateParams(
                spot=ops.SpotDateInputs(pair=pair, trade_date=one(query, "trade"))))
            rule = answered.basis.rule
            return {"date": answered.date, "outcome": ops.DateOutcome.Name(answered.outcome),
                    "rule": f"{rule.subject} v{rule.version}" if rule.subject else ""}
        if path == "/originate":
            # Contract v23 (W11.11): a fill of an order placed in the broker's
            # own app, on no order and no placement, its account stamped from
            # the ems's map.
            today = datetime.now(timezone.utc).date()
            venue_order = one(query, "order")
            self.sent += 1
            done = await self.call("ReportExecution", ops.ReportExecutionParams(
                venue_account_code=one(query, "code"),
                venue_execution_id=f"SIM-OWN-{venue_order}:{self.sent}",
                kind=ops.ExecutionKind.EXECUTION_KIND_VENUE_REPORTED,
                source=ops.ExecutionSource.EXECUTION_SOURCE_PRIMARY,
                quantity=decimal(one(query, "quantity")), price=usd(PRICE),
                trade_time_ns=time.time_ns(), trade_date=today.isoformat(),
                finality=ops.Finality(level=ops.FinalityLevel.FINALITY_LEVEL_FINAL, at_ns=time.time_ns()),
                settlement_date=settles(today).isoformat(),
                instrument_id=one(query, "instrument"),
                side=ops.Side.Value("SIDE_" + one(query, "side", "buy").upper()),
                venue_order_id=venue_order, venue_client_order_id=f"CL-{venue_order}",
                raw_record=self.raw(f"fills/own/{venue_order}/{self.sent}")))
            return fill_json(done.execution)
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
        def answer_html(self, page):
            data = page.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_POST(self):
            # The desk's page (contract v23): a person's form, or a tool's call.
            parsed = urllib.parse.urlparse(self.path)
            caller = assertion(self.headers.get("Meridian-Caller"))
            sent = self.rfile.read(int(self.headers.get("Content-Length") or 0))
            if not isinstance(served, Desk) or parsed.path not in ("/fills/accept", "/fills/dk", "/fills/read"):
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if self.headers.get("Content-Type", "").startswith("application/json"):
                asked = json.loads(sent or b"{}")
                future = asyncio.run_coroutine_threadsafe(served.tool(parsed.path, asked, caller), LOOP)
                data = json.dumps(future.result(timeout=60)).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            fields = {name: values[0] for name, values in urllib.parse.parse_qs(sent.decode()).items()}
            future = asyncio.run_coroutine_threadsafe(served.form(parsed.path, fields, caller), LOOP)
            self.answer_html(future.result(timeout=60))

        def do_GET(self):
            parsed = urllib.parse.urlparse(self.path)
            caller = assertion(self.headers.get("Meridian-Caller"))
            if isinstance(served, Desk) and parsed.path == "/fills":
                future = asyncio.run_coroutine_threadsafe(served.fills_page(caller), LOOP)
                self.answer_html(future.result(timeout=60))
                return
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


def booking_json(booking):
    return {"booking_id": booking.booking_id, "order_id": booking.order_id,
            "state": ops.BookingState.Name(booking.state),
            "allocations": [{"account_id": a.account_id, "quantity": number(a.quantity),
                             "error_cause": ops.ErrorCause.Name(a.error_cause)}
                            for a in booking.allocations]}


class Ops(Plugin):
    """Contract v24: `operations`' correction by replace after the hand-off
    (W11.29), each allocation ACCOUNT:QUANTITY:GROSS; and the bookings read
    (W11.31)."""

    async def answer(self, path, query, caller):
        if path == "/correct":
            allocations = [ops.Allocation(account_id=a, quantity=decimal(q),
                                          average_price=usd(one(query, "price", PRICE)),
                                          gross_amount=usd(g))
                           for a, q, g in (said.split(":") for said in split(one(query, "alloc")))]
            done = await self.call("CorrectBooking", ops.CorrectBookingParams(
                booking_id=one(query, "booking"),
                kind=ops.BookingCorrectionKind.BOOKING_CORRECTION_KIND_REPLACE,
                allocations=allocations, effective_date=one(query, "effective"),
                reason=one(query, "reason", "the allocation was corrected"), acting_for=caller))
            return booking_json(done.booking)
        if path == "/bookings":
            listed = await self.call("ListBookings", ops.ListBookingsParams(
                account_id=one(query, "account"), page_size=500))
            return {"bookings": [booking_json(b) for b in listed.bookings]}
        raise Refusal({"words": f"no page {path}"})


class DeskInterface(Interface):
    """The desk's interface, with the ems's tools beside its pages (W6.20,
    contract v12): under parity, each act of the Unmatched fills page and its
    read, at write on `ems`."""

    def _tools(self):
        write = sidecar_pb2.ACCESS_LEVEL_WRITE
        ids = {"type": "array", "minItems": 1, "maxItems": 500, "items": {"type": "string"},
               "description": "the unmatched executions' EXE- IDs, of one account, instrument and side"}
        accept = {"type": "object", "properties": {
            "execution_ids": ids,
            "order_id": {"type": "string", "description": "an order the ems may place against; or new_order"},
            "new_order": {"type": "object", "properties": {"sleeve": {"type": "string"}, "note": {"type": "string"}},
                          "additionalProperties": False},
            "note": {"type": "string", "maxLength": 2000}},
            "required": ["execution_ids"], "additionalProperties": False}
        dk = {"type": "object", "properties": {
            "execution_ids": ids,
            "reason": {"type": "string", "enum": [v for v, _ in DK_REASONS]},
            "text": {"type": "string", "maxLength": 2000, "description": "required with other"}},
            "required": ["execution_ids", "reason"], "additionalProperties": False}
        read = {"type": "object", "properties": {"account_id": {"type": "string"}},
                "additionalProperties": False}

        def tool(name, title, description, path, schema, reads=False):
            return sidecar_pb2.ToolDeclaration(
                name=name, title=title, description=description, method="POST", path=path,
                levels=[write], reads=reads, input_schema=json.dumps(schema),
                output_schema=json.dumps({"type": "object"}) if reads else "", roles=["ems"])

        return [
            tool("read_unmatched_fills", "Read unmatched fills",
                 "the fills your venues reported on no order and no placement, waiting for an answer",
                 "/fills/read", read, reads=True),
            tool("accept_execution", "Accept unmatched fills",
                 "accept unmatched fills of one account, instrument and side to an order, or to a new "
                 "order made from them, for the person; the book creates the placement at filled",
                 "/fills/accept", accept),
            tool("dk_execution", "Answer unmatched fills DK",
                 "answer unmatched fills don't know, with a reason, and text with other, for the person",
                 "/fills/dk", dk),
        ]


async def main():
    title, kind, levels = {
        "pm": ("Portfolio stand-in", Pm, ["write", "admin"]),
        "desk": ("Desk stand-in", Desk, ["write", "admin"]),
        "comp": ("Compliance stand-in", Comp, ["write", "admin"]),
        "venue": ("Simulated venue", Venue, ["write", "admin"]),
        "ops": ("Operations stand-in", Ops, ["write", "admin"]),
    }[KIND]
    roles = {"desk": ["oms", "ems"]}.get(KIND, [])
    pages = [Page("/", title, levels=levels, roles=roles)]
    interface = Interface(PORT, title, pages=pages)
    if KIND == "desk":
        # Contract v23: the ems's page for unmatched fills, and its tools.
        pages.append(Page("/fills", "Unmatched fills", levels=["write"], roles=["ems"]))
        interface = DeskInterface(PORT, title, pages=pages)
    plugin = await meridian.connect(interface=interface)
    served = kind(plugin)
    # Heard through the SDK's receive (W4.3): seeded by one read at start,
    # nothing handed on for it, then each change in order.
    if KIND == "venue":
        asyncio.ensure_future(plugin.receive(placement_updated=served.on_placement,
                                             trading_instruction_changed=served.on_instruction,
                                             execution_recorded=served.on_execution,
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
