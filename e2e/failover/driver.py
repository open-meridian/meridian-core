"""The store-level write driver (step 3b part 5): a test-only plugin on the
Python SDK that writes through the stores' own write paths while
e2e/failover/run.py injects each failure class, so what a class lost is
read in the stores, not the database. TEST ONLY: e2e/failover/run.py runs it
from the chart's `sidecars[]` at install, in the SDK's interop image, with
this file passed as `MERIDIAN_DRIVER`; it is in no values file, no catalogue
and no image. Nothing here reaches a broker, a brokerage or a vendor.

DRIVER_ROLE says which of its instances it is:

- `data`: one dataset declared at registration (`<instance>:drive`), and a
  price recorded into it every half second (RecordPrices, data -> lake),
  each named by its row key; and, holding reporting too, its latest read
  back every second as a live page reads it (a default read of the latest,
  ListPrices), each read kept in /tmp/page.log as `PAGE <unix ms> ok
  <row key> <valid ms>`, `PAGE <unix ms> empty` or `PAGE <unix ms> failed
  <status>`: what the buy-side trust targets are measured from (contract
  v30, stage C; the resilience spec's section 15);
- `oms`: telephoned orders recorded in the accounts of DRIVER_ACCOUNTS
  (RecordOrder, oms -> book), each the next number in its account, and the
  accounts read back through the book's writer (ListOrders), by two
  workers at once;
- `desk`: the ems stand-in, holding oms and ems, the order-plugin class's
  subject (below): orders recorded in its own account and placed;
- `standing` (contract v30): holding reporting, the standing-coordinator
  class's subject: a standing job over its first account held as a view
  (`analytics.Jobs().standing`), each job ID it is held under kept in
  /tmp/standing.log as `HELD <job ID> <unix ms>`, so a job registered
  again with another coordinator shows;
- `flood`: holding data and reporting, the memory-pressure and noisy-plugin
  classes' subject (stage M, below): its own dataset recorded once, then
  read past the lake's budget, or past its own share at its sidecar, as
  /tmp/flood says. It declares a second dataset too (`<instance>:unkept`,
  asked of it, which the run licenses as served, not kept): each want of it
  served, and in the `unkept` mode read, so a lake reader that takes the read
  hands it to the writer (contract v30, stage B).

- `scale-data` and `scale-oms`: the write-scale step's load (step 3d,
  stage T, below), in the scaled run alone: nothing until /tmp/scale is
  there, then prices recorded into the datasets of DRIVER_SCALE_KEYS, or
  orders into DRIVER_ACCOUNTS, by DRIVER_SCALE_WORKERS workers at once, as
  fast as each is answered.

- `shard` and `shard-reader`: the cluster proof that each subject is served
  once (step 3d, checkpoint 5, item 33, below), in the scaled run alone:
  `shard` a data plugin on `meridian.run`, each pod of an instance at two
  shards with a standby, serving its share of the reader's want, and
  `shard-reader` the reader whose view of DRIVER_SUBJECTS raises it.

Each acknowledgement is printed and kept in /tmp/acked as `ACKED <kind>
<id> <unix ms>`. The `oms` instance also keeps a Jepsen-style history in
/tmp/history.edn (invoke, ok, fail, info), each order a list-append
transaction: an append of its number to the key `book/<account>`, and a
read of that key as the list of numbers the book holds there, in the order
the book's journal took them. Elle checks it (the ha failover CI job): an
order recorded to an account partition and acknowledged is an append that
every later read sees, in one order (spec/a-deployment-scales-out-and-
fails-over-by-configuration section 2, items 2 and 4; step 3a's one fenced
log per partition; ruling 1048).

`touch /tmp/stop` ends it: every call is bounded (CALL_SECONDS), so the stop
is heard within that, and `DONE` is printed and kept in /tmp/done. /tmp is
the pod's own scratch volume, kept across a restart of this container, so a
restart goes on numbering where it was and with processes of its own.
"""

import asyncio
import hashlib
import os
import random
import sys
import time
import uuid
from decimal import Decimal

import grpc
from meridian import DatasetDeclaration, Declaration
from meridian.client import connect
from meridian.plugin.v1 import operations_pb2 as ops
from meridian.roles import data, ems, oms, reporting
from meridian.v1 import sidecar_pb2

ROLE = os.environ.get("DRIVER_ROLE", "")
ACCOUNTS = [a for a in os.environ.get("DRIVER_ACCOUNTS", "").split(",") if a]
INSTRUMENT = os.environ.get("DRIVER_INSTRUMENT", "")
# US dollars' cash record (e2e/book/instruments.sql), named by its ID alone:
# a code is resolved through the platform's master for currencies (W3.3),
# and this run has no platform.
USD = os.environ.get("DRIVER_CASH", "")
DATASET_KEY = "drive"
# The flood instance's dataset asked of it, served and not kept (the run
# licenses it so): what a lake reader hands to its writer (contract v30).
UNKEPT_KEY = "unkept"
# Every call bounded: a stop is heard within this, whatever the stores do.
CALL_SECONDS = 10
# A key takes this many appends at most, so a read of it is one page: 400,
# or what DRIVER_PER_KEY says (the reconfigure run's, longer from stage T,
# takes twice as many a key, so its orders are appended to its end; a
# key's read is then two pages, which the read follows).
PER_KEY = int(os.environ.get("DRIVER_PER_KEY", "400"))
# Appends to a key before the next key is taken.
RUN_OF = 40
WORKERS = 2
STOP = "/tmp/stop"
# The person every order is recorded for (a recorded order is a user's act,
# W11.2): vouched for as the dashboard vouches, signed with a key made for
# the run (e2e/failover/run.py), whose public half the run puts beside the
# dashboard's in the keys every sidecar trusts. Its seed and id come from a
# Secret made for the run, never printed.
PERSON = "local|e2e-driver"
SIGNER_SEED = os.environ.get("DRIVER_SIGNER_SEED", "")
SIGNER_KEY_ID = os.environ.get("DRIVER_SIGNER_KEY_ID", "")


def say(line):
    print(line, flush=True)


def kept(path, line):
    with open(path, "a", encoding="utf-8") as held:
        held.write(line + "\n")


def generation():
    """This start's number: the restarts before it, kept in /tmp."""
    path = "/tmp/generation"
    try:
        with open(path, encoding="utf-8") as held:
            now = int(held.read().strip() or 0) + 1
    except (OSError, ValueError):
        now = 1
    with open(path, "w", encoding="utf-8") as held:
        held.write(str(now))
    return now


def acked(kind, ident):
    line = f"ACKED {kind} {ident} {int(time.time() * 1000)}"
    say(line)
    kept("/tmp/acked", line)


def took(kind, began):
    """A write's own time, from its call to its answer, kept in /tmp/took as
    `TOOK <kind> <unix ms answered> <ms taken>`: what the rebalance class
    reads a move's write pause from (step 3d; the resilience spec's section
    15)."""
    now = time.time()
    kept("/tmp/took", f"TOOK {kind} {int(now * 1000)} {int((now - began) * 1000)}")


def refused(kind, ident, failed):
    """A write refused, or answered by nothing, kept in /tmp/refused as
    `REFUSED <kind> <id> <unix ms> <its words>`: what the rebalance class
    holds to none inside a move's hold."""
    kept("/tmp/refused", f"REFUSED {kind} {ident} {int(time.time() * 1000)} {refusal_words(failed)}")


def refusal_words(failed):
    """A refusal's whole words on one line, for the run's log and artifact
    (CI 43626539705: five refused prices whose cause no log named): its
    status, its detail in full, and each trailing metadata entry the
    sidecar sent with it (the refusal's reason among them)."""
    if not isinstance(failed, grpc.aio.AioRpcError):
        return f"{type(failed).__name__}: {failed}".replace("\n", " ")
    said = [f"status={failed.code().name}", f"detail={failed.details()}"]
    for key, value in failed.trailing_metadata() or ():
        if not key.endswith("-bin"):
            said.append(f"{key}={value}")
    return " ".join(said).replace("\r", " ").replace("\n", " ")


def decimal(text):
    exact = Decimal(str(text))
    scale = max(0, -exact.as_tuple().exponent)
    units = int(exact.scaleb(scale))
    return ops.Decimal(high=units >> 64, low=units & ((1 << 64) - 1), scale=scale)


def stopped():
    return os.path.exists(STOP)


async def bounded(call):
    return await asyncio.wait_for(call, CALL_SECONDS)


# ── A person, as the dashboard would vouch for them (W4.9) ─────────────────
#
# Ed25519 (RFC 8032) in the reference algorithm, as the SDK's book suite
# signs (meridian-python tests/test_book.py): pure Python, for one
# signature every half minute, so the image needs nothing more.

_P = 2**255 - 19
_L = 2**252 + 27742317777372353535851937790883648493
_D = -121665 * pow(121666, _P - 2, _P) % _P
_I = pow(2, (_P - 1) // 4, _P)


def _inverse(x):
    return pow(x, _P - 2, _P)


def _x_of(y, sign):
    x2 = (y * y - 1) * _inverse(_D * y * y + 1) % _P
    x = pow(x2, (_P + 3) // 8, _P)
    if (x * x - x2) % _P:
        x = x * _I % _P
    if x & 1 != sign:
        x = _P - x
    return x


_GY = 4 * _inverse(5) % _P
_G = (_x_of(_GY, 0), _GY, 1, _x_of(_GY, 0) * _GY % _P)


def _add(a, b):
    e1 = (a[1] - a[0]) * (b[1] - b[0]) % _P
    e2 = (a[1] + a[0]) * (b[1] + b[0]) % _P
    c = 2 * a[3] * b[3] * _D % _P
    d = 2 * a[2] * b[2] % _P
    e, f, g, h = e2 - e1, d - c, d + c, e2 + e1
    return (e * f % _P, g * h % _P, f * g % _P, e * h % _P)


def _times(scalar, point):
    held = (0, 1, 1, 0)
    while scalar:
        if scalar & 1:
            held = _add(held, point)
        point = _add(point, point)
        scalar >>= 1
    return held


def _compressed(point):
    z = _inverse(point[2])
    x, y = point[0] * z % _P, point[1] * z % _P
    return int.to_bytes(y | ((x & 1) << 255), 32, "little")


def _sign(seed, message):
    digest = hashlib.sha512(seed).digest()
    a = int.from_bytes(digest[:32], "little")
    a = (a & ((1 << 254) - 8)) | (1 << 254)
    public = _compressed(_times(a, _G))
    r = int.from_bytes(hashlib.sha512(digest[32:] + message).digest(), "little") % _L
    big_r = _compressed(_times(r, _G))
    h = int.from_bytes(hashlib.sha512(big_r + public + message).digest(), "little") % _L
    return big_r + int.to_bytes((r + h * a) % _L, 32, "little")


class Person:
    """The run's person at write on the driver's accounts, as an assertion
    for this instance: made again before it is 30 seconds old (it lives
    50), so every command carries one in date."""

    def __init__(self, instance, roles=()):
        self.instance = instance
        # On a plugin holding several roles, the claims say what the person
        # holds on each (contract v15): write on every account, on each.
        everything = list(range(len(ACCOUNTS)))
        self.roles = [sidecar_pb2.RoleAccess(role=role, level=sidecar_pb2.ACCESS_LEVEL_WRITE,
                                             read_positions=everything, write_positions=everything)
                      for role in roles]
        self.made = 0
        self.held = None

    def assertion(self):
        now = time.time_ns()
        if self.held is None or now - self.made > 30_000_000_000:
            claims = sidecar_pb2.CallerClaims(
                subject=PERSON, display_name="E2E driver", audience_instance_id=self.instance,
                level=sidecar_pb2.ACCESS_LEVEL_WRITE, read_account_ids=ACCOUNTS,
                write_account_ids=ACCOUNTS, issued_at_ns=now - 1_000_000_000,
                expires_at_ns=now + 50_000_000_000, assertion_id=str(uuid.uuid4()), roles=self.roles,
            ).SerializeToString()
            self.held = sidecar_pb2.CallerAssertion(
                claims=claims, signature=_sign(bytes.fromhex(SIGNER_SEED), claims), key_id=SIGNER_KEY_ID)
            self.made = now
        return self.held


# ── The history ─────────────────────────────────────────────────────────────


class History:
    """Jepsen's history, as EDN, one operation a line in the order they
    happened here: `:index` in that order, `:time` nanoseconds on this
    process's monotonic clock."""

    def __init__(self, path):
        self.path = path
        self.index = 0
        self.began = time.monotonic_ns()
        # Indexes go on from a previous start's.
        try:
            with open(path, encoding="utf-8") as held:
                self.index = sum(1 for _ in held)
        except OSError:
            pass

    def write(self, kind, process, value):
        line = (f"{{:type :{kind}, :f :txn, :value {value}, :process {process}, "
                f":time {time.monotonic_ns() - self.began}, :index {self.index}}}")
        self.index += 1
        kept(self.path, line)


def append_edn(key, number):
    return f'[[:append "{key}" {number}]]'


def read_edn(key, numbers):
    held = "nil" if numbers is None else "[" + " ".join(str(n) for n in numbers) + "]"
    return f'[[:r "{key}" {held}]]'


# ── data -> lake ────────────────────────────────────────────────────────────


class Recording(data.Role):
    seed = False

    async def on_observations_wanted(self, heard):
        # Nothing is asked of it: it records unasked. (`connect` starts no
        # stream; the flood instance hears its wants itself, below.)
        pass


async def serve_want(plugin, heard):
    """The flood instance's dataset served, not kept, is asked of it: each
    subject's price served for the want, which the lake hands to the read
    waiting on it. Each want heard, and whether it was served, kept in
    /tmp/unkept-wants.log."""
    want = heard.message
    kept("/tmp/unkept-wants.log", f"WANT {want.want_id} {want.dataset} {len(want.subjects)}")
    if want.dataset != f"{plugin.identity.instance_id}:{UNKEPT_KEY}":
        return
    now = time.time_ns()
    prices = [ops.Price(
        meta=ops.ObservationMeta(
            row_key=f"unkept-{subject.entity_id}-{now}",
            subjects=[ops.SubjectRef(entity_id=subject.entity_id)],
            source=ops.Source(dataset=want.dataset), valid_from_ns=now,
            source_times=[ops.SourceTime(kind=ops.SourceTimeKind.Value("SOURCE_TIME_KIND_PUBLISHED"),
                                         at_ns=now)],
            raw=plugin.raw_record(f"unkept/{now}")),
        kind=ops.PriceKind.PRICE_KIND_LAST,
        price=ops.Money(amount=decimal("101.25"), instrument_id=USD),
        basis=ops.PriceBasis.PRICE_BASIS_PER_UNIT) for subject in want.subjects]
    try:
        await bounded(plugin._operations().RecordPrices(
            ops.RecordPricesParams(prices=prices, want_id=want.want_id)))
        kept("/tmp/unkept-wants.log", f"SERVED {want.want_id}")
    except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
        kept("/tmp/unkept-wants.log", f"REFUSED {want.want_id} {words(failed)[:200]}")
        say(f"driver: flood: want {want.want_id} not served ({words(failed)})")


async def drive_data(plugin, gen):
    stub = plugin._operations()
    dataset = f"{plugin.identity.instance_id}:{DATASET_KEY}"
    # A row key fixes its valid time (the lake's rule 5, enforced from step
    # 3d): a pod rescheduled starts its generations again from 1 with an
    # empty /tmp, so each start's keys carry when it started, never another
    # start's key at a later time.
    started = time.time_ns() // 1_000_000
    n = 0
    while not stopped():
        n += 1
        key = f"drive-{gen}-{started}-{n}"
        now = time.time_ns()
        price = ops.Price(
            meta=ops.ObservationMeta(
                row_key=key,
                subjects=[ops.SubjectRef(entity_id=INSTRUMENT)],
                source=ops.Source(dataset=dataset),
                valid_from_ns=now,
                source_times=[ops.SourceTime(kind=ops.SourceTimeKind.Value("SOURCE_TIME_KIND_PUBLISHED"),
                                             at_ns=now)],
                raw=plugin.raw_record(f"drive/{key}"),
            ),
            kind=ops.PriceKind.PRICE_KIND_LAST,
            price=ops.Money(amount=decimal(f"{100 + n % 50}.25"), instrument_id=USD),
            basis=ops.PriceBasis.PRICE_BASIS_PER_UNIT,
        )
        began = time.time()
        try:
            done = await bounded(stub.RecordPrices(ops.RecordPricesParams(prices=[price])))
            took("price", began)
            if done.recorded + done.restated + done.unchanged == 1:
                acked("price", key)
            else:
                # Answered, and its one row not counted (CI 43656183991, ha:
                # every price after its range moved to another writer group
                # was answered 0, 0, 0): never acknowledged, and kept as a
                # refusal is, so a move's window holds it to none.
                # Its own name: `words` is the module's function, which the
                # except below calls (CI 43804841979: a local of that name
                # made every refused price an UnboundLocalError, which ended
                # the driver's loop and hid what the lake said).
                uncounted = (f"UNCOUNTED: answered recorded {done.recorded}, restated {done.restated}, "
                             f"unchanged {done.unchanged} for one row")
                say(f"REFUSED price {key}: {uncounted}")
                kept("/tmp/refused", f"REFUSED price {key} {int(time.time() * 1000)} {uncounted}")
        except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
            say(f"REFUSED price {key}: {words(failed)}")
            refused("price", key, failed)
            await asyncio.sleep(0.5)
        await asyncio.sleep(0.5)


async def drive_page(plugin):
    """A live page's reads of the driver's own price (contract v30, stage
    C): the latest, by the deployment's default sources, every second, as
    the kit's live cells are drawn; each answer's row and its valid time
    kept, or that it was empty or failed. The page ages each value itself."""
    stub = plugin._operations()
    while not stopped():
        began = int(time.time() * 1000)
        # Each read kept at the moment its answer came: what the page holds
        # from then (a slow read answers with a row recorded while it waited).
        try:
            listed = await bounded(stub.ListPrices(ops.ListPricesParams(
                subjects=[ops.SubjectRef(entity_id=INSTRUMENT)], page_size=1)))
            answered = int(time.time() * 1000)
            if listed.prices:
                meta = listed.prices[0].meta
                kept("/tmp/page.log", f"PAGE {answered} ok {meta.row_key} {meta.valid_from_ns // 1_000_000}")
            else:
                kept("/tmp/page.log", f"PAGE {answered} empty")
        except asyncio.TimeoutError:
            kept("/tmp/page.log", f"PAGE {int(time.time() * 1000)} failed TIMEOUT")
        except grpc.aio.AioRpcError as failed:
            kept("/tmp/page.log", f"PAGE {int(time.time() * 1000)} failed {failed.code().name}")
        except Exception as failed:  # noqa: BLE001 - a page's read never takes the writes down
            kept("/tmp/page.log", f"PAGE {int(time.time() * 1000)} failed {type(failed).__name__}")
        await asyncio.sleep(max(0.0, 1.0 - (time.time() * 1000 - began) / 1000))


# ── oms -> book ─────────────────────────────────────────────────────────────


class Managing(oms.Role):
    seed = False

    async def on_order_updated(self, heard):
        pass

    async def on_booking_updated(self, heard):
        pass

    async def on_alert_changed(self, heard):
        pass


def words(failed):
    if isinstance(failed, grpc.aio.AioRpcError):
        return f"{failed.code().name}: {failed.details()}"[:300]
    return type(failed).__name__


def definitely_not(failed):
    """Whether a failed call is known not to have taken effect: refused by
    the sidecar before it left, or answered by the book with a refusal
    (`aborted`: a fenced writer's commit refused among them, which never
    commits). A timeout, or the bus or a store not answering, may have
    committed: indeterminate."""
    if not isinstance(failed, grpc.aio.AioRpcError):
        return False
    return failed.code() in (grpc.StatusCode.INVALID_ARGUMENT, grpc.StatusCode.PERMISSION_DENIED,
                             grpc.StatusCode.FAILED_PRECONDITION, grpc.StatusCode.NOT_FOUND,
                             grpc.StatusCode.ALREADY_EXISTS, grpc.StatusCode.UNAUTHENTICATED,
                             grpc.StatusCode.ABORTED)


class Book:
    def __init__(self, plugin, gen, history):
        self.stub = plugin._operations()
        self.person = Person(plugin.identity.instance_id)
        self.gen = gen
        self.history = history
        self.counts = {a: 0 for a in ACCOUNTS}
        self.next = 0
        self.full = False
        self.lock = asyncio.Lock()

    def key_now(self):
        """The account taking appends: the next in turn after each run of
        RUN_OF, passing over any that holds PER_KEY."""
        turn = (self.next // RUN_OF) % len(ACCOUNTS)
        for step in range(len(ACCOUNTS)):
            account = ACCOUNTS[(turn + step) % len(ACCOUNTS)]
            if self.counts[account] < PER_KEY:
                return account
        return None

    async def append(self, process):
        async with self.lock:
            account = self.key_now()
            if account is None:
                # Said once and kept: the run holds that it never happens
                # (e2e/failover/run.py, driver_history), since from here no
                # order is appended and nothing after is under its load.
                if not self.full:
                    self.full = True
                    say(f"FULL: every key holds {PER_KEY} appends; no order is appended from here")
                    kept("/tmp/full", f"FULL {int(time.time() * 1000)} after {self.next} appends")
                return "full"
            self.next += 1
            self.counts[account] += 1
            number = self.gen * 1_000_000 + self.next
        key = f"book/{account}"
        value = append_edn(key, number)
        self.history.write("invoke", process, value)
        began = time.time()
        try:
            done = await bounded(self.stub.RecordOrder(ops.RecordOrderParams(
                account_id=account, instrument_id=INSTRUMENT, side=ops.Side.SIDE_BUY,
                quantity=decimal("1"), order_type=ops.OrderType.ORDER_TYPE_MARKET,
                # Good till cancelled, never a day order: an unreleased day
                # order expires at the deployment's day end, a change of its
                # own, and the reads below order the list by each order's
                # last change -- a run crossing midnight read its lists
                # reordered (CI run 42536769361). Nothing else changes these.
                time_in_force=ops.TimeInForce.TIME_IN_FORCE_GOOD_TILL_CANCELLED,
                venue_order_id=str(number), origin=ops.OrderOrigin.ORDER_ORIGIN_TELEPHONED,
                idempotency_key=f"drive-{number}", acting_for=self.person.assertion())))
        except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
            refused("order", number, failed)
            if definitely_not(failed):
                self.history.write("fail", process, value)
                say(f"REFUSED order {number}: {words(failed)}")
                return "fail"
            self.history.write("info", process, value)
            say(f"UNKNOWN order {number}: {words(failed)}")
            return "info"
        took("order", began)
        self.history.write("ok", process, value)
        acked("order", done.order.order_id)
        return "ok"

    async def read(self, process):
        account = self.key_now() or ACCOUNTS[0]
        key = f"book/{account}"
        self.history.write("invoke", process, read_edn(key, None))
        try:
            numbered = []
            cursor = ""
            while True:
                listed = await bounded(self.stub.ListOrders(ops.ListOrdersParams(
                    account_id=account, page_size=500, cursor=cursor)))
                for order in listed.orders:
                    if order.venue_order_id.isdigit() and order.origin == ops.OrderOrigin.ORDER_ORIGIN_TELEPHONED:
                        at = order.last_change
                        numbered.append((at.partition, at.sequence, at.previous_sequence,
                                         int(order.venue_order_id)))
                cursor = listed.next_cursor
                if not cursor:
                    break
        except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
            # A read changes nothing: failed, whatever the reason.
            self.history.write("fail", process, read_edn(key, None))
            say(f"UNREAD {key}: {words(failed)}")
            return "fail"
        self.history.write("ok", process, read_edn(key, as_recorded(numbered)))
        return "ok"


def as_recorded(numbered):
    """An account's orders in the order the book recorded them, across the
    account's moves: `numbered` holds each order's last change -- its
    partition, its sequence there, the sequence of the account's order
    change before it -- and its number.

    A partition numbers its own sequences, so after a move an order may
    carry a lower one than an order recorded before it: sorted by sequence
    alone, a read after ACC-DRIVE-00's move to P1 put its new orders first,
    and Elle read eight incompatible orders (PR CI 43662032539, ha). The
    book says the order itself: within a partition its sequence does, and
    each change names the one before it, in whichever partition that was
    (crates/bor/tests/postgres.rs,
    m11_an_accounts_orders_chain_in_order_across_its_move). So each stay in
    a partition is in sequence order, and a stay follows the one whose last
    order its first names. These orders change once, when recorded, so the
    chain is theirs alone."""
    stays = {}
    for partition, sequence, previous, number in sorted(numbered):
        stays.setdefault(partition, []).append((sequence, previous, number))
    # Each stay by the order its first names, where that is in another
    # partition; the first stay of all names none (0), or one not listed.
    last = {rows[-1][0]: partition for partition, rows in stays.items()}
    follows = {}
    first = []
    for partition, rows in stays.items():
        before = last.get(rows[0][1])
        if before is None or before == partition or before in follows:
            first.append(partition)
        else:
            follows[before] = partition
    ordered = []
    for partition in sorted(first):
        while partition is not None and stays.get(partition) is not None:
            ordered.extend(number for _, _, number in stays.pop(partition))
            partition = follows.get(partition)
    return ordered


async def worker(book, slot, gen):
    # A process whose operation's outcome is unknown is never used again
    # (Jepsen's rule): the slot takes a new one.
    process = (gen * 100 + slot) * 1000
    while not stopped():
        said = await (book.append(process) if random.random() < 0.75 else book.read(process))
        if said == "info":
            process += 1
        await asyncio.sleep(0.25 if said == "ok" else 1.0)


async def drive_oms(plugin, gen):
    book = Book(plugin, gen, History("/tmp/history.edn"))
    await asyncio.gather(*(worker(book, slot, gen) for slot in range(WORKERS)))


# ── The ems stand-in: the order-plugin class's subject ─────────────────────
#
# DRIVER_ROLE=desk holds `oms` and `ems`, as e2e/orders/stand_in.py's desk
# does: it records an order in its account (DRIVER_ACCOUNTS, one) and places
# it manually, agreed by phone (no connectivity plugin to reach), each act
# acknowledged and kept as `ACKED order <id>` and `ACKED placement <id>`,
# and each placement said as `PLACED <placement> <order>`. Nothing of its
# work is kept but in the book (stateless compute, owned storage): started
# again it reads its account back, places any order of its own the book
# holds unplaced, and goes on numbering after the highest. Each act carries
# an idempotency key made from its order's number, so one sent twice -- the
# second time after a restart -- is one in the book.


class Desking(ems.Role):
    seed = False

    async def on_order_updated(self, heard):
        pass

    async def on_execution_recorded(self, heard):
        pass

    async def on_placement_updated(self, heard):
        pass

    async def on_trading_instruction_changed(self, heard):
        pass


VENUE = "VEN-01JE2EDR1VEDESK000000000"


class Desk:
    def __init__(self, plugin):
        self.stub = plugin._operations()
        self.person = Person(plugin.identity.instance_id, roles=("oms", "ems"))
        self.account = ACCOUNTS[0]

    async def held(self):
        """The book's own orders of this desk, by number, and whether each
        is placed."""
        numbered, cursor = {}, ""
        while True:
            listed = await bounded(self.stub.ListOrders(ops.ListOrdersParams(
                account_id=self.account, page_size=500, cursor=cursor)))
            placed = {p.order_id for p in listed.placements}
            for order in listed.orders:
                if order.venue_order_id.isdigit():
                    numbered[int(order.venue_order_id)] = (order.order_id,
                                                           order.order_id in placed or bool(order.placements))
            cursor = listed.next_cursor
            if not cursor:
                return numbered

    async def place(self, number, order_id):
        done = await bounded(self.stub.CreatePlacement(ops.CreatePlacementParams(
            order_id=order_id, quantity=decimal("1"),
            time_in_force=ops.TimeInForce.TIME_IN_FORCE_GOOD_TILL_CANCELLED,
            placement_type=ops.PlacementType.PLACEMENT_TYPE_MANUAL, destination=VENUE,
            idempotency_key=f"desk-place-{number}", acting_for=self.person.assertion())))
        say(f"PLACED {done.placement.placement_id} {order_id}")
        acked("placement", done.placement.placement_id)

    async def run(self):
        held = await self.held()
        for number, (order_id, placed) in sorted(held.items()):
            if not placed:
                say(f"driver: desk: order {order_id} ({number}) unplaced on start; placing it")
                await self.place(number, order_id)
        number = max(held, default=0)
        while not stopped():
            number += 1
            try:
                done = await bounded(self.stub.RecordOrder(ops.RecordOrderParams(
                    account_id=self.account, instrument_id=INSTRUMENT, side=ops.Side.SIDE_BUY,
                    quantity=decimal("1"), order_type=ops.OrderType.ORDER_TYPE_MARKET,
                    # Good till cancelled, as the book's appends: a day
                    # order and its placement end at the day's end.
                    time_in_force=ops.TimeInForce.TIME_IN_FORCE_GOOD_TILL_CANCELLED,
                    venue_order_id=str(number),
                    origin=ops.OrderOrigin.ORDER_ORIGIN_TELEPHONED, idempotency_key=f"desk-{number}",
                    acting_for=self.person.assertion())))
                acked("order", done.order.order_id)
                await self.place(number, done.order.order_id)
            except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
                say(f"UNPLACED {number}: {words(failed)}")
                # Read back before going on: what the book holds decides.
                await asyncio.sleep(1.0)
                return
            await asyncio.sleep(0.5)


async def drive_desk(plugin):
    # Each pass reads the book back first, so a call whose outcome was not
    # heard is settled by what the book holds before the next.
    while not stopped():
        try:
            await Desk(plugin).run()
        except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
            say(f"driver: desk: the book could not be read back ({words(failed)})")
            await asyncio.sleep(1.0)


# ── flood -> lake: stage M's memory-pressure and noisy-plugin subject ───────
#
# DRIVER_ROLE=flood holds `data` and `reporting`: it records FLOOD_ROWS
# prices into its own dataset once (`<instance>:drive`, kept across a
# restart by /tmp/seeded), then reads them as /tmp/flood says, until it says
# nothing:
#
# - `pressure`: FLOOD_PRESSURE readers at once, each reading the whole of its
#   dataset by valid time, page after page -- reads past the lake's budget
#   (the resilience spec's section 10a), refused REFUSAL_REASON_OVERLOADED.
#   Each reader is impatient, as a client in a retry storm is: a read not
#   answered within FLOOD_PATIENCE is given up and asked again at once. What
#   the lake already took keeps its turn there, so the flood's demand on
#   each member answering it is no longer capped by the flooder's own
#   requests in flight at its sidecar (`sidecar.limits.inFlight`, 32 then,
#   16 since 2026-11-11), which, split over the lake's writer and its reader
#   in ha, matched their 16 turns each (MERIDIAN_HANDLERS_AT_ONCE) and was
#   refused only as the split ran uneven (8, 23 and 0 times in part 3's
#   three runs); no bound is changed;
# - `noisy`: FLOOD_NOISY readers at once, each asking for one page as fast
#   as it is answered -- past this plugin's share at its sidecar, refused
#   REFUSAL_REASON_RATE_LIMITED.
#
# Every five seconds it says what it heard, as `FLOOD <mode> answered=N
# overloaded=N rate_limited=N other=N retry_after_min=S other_<why>=N...`,
# kept in /tmp/flood.log: answered reads, each refusal for load by its
# reason, the least retry-after a refusal carried, and what each `other` was
# -- `other_timeout` a read given up, `other_<CODE>` a gRPC status (with
# `_<reason>` where it named a refusal reason that is not for load).

FLOOD_ROWS = 20000
FLOOD_PRESSURE = 32
# How long a pressure reader waits for a read before it gives it up and asks
# again, in seconds: past a page's answer at the lake's guarded read rate
# unloaded, well short of what sixteen waiting for two answers built at once
# take.
FLOOD_PATIENCE = 2
FLOOD_NOISY = 160
FLOOD_MODE = "/tmp/flood"
REFUSAL_METADATA = "meridian-refusal-bin"
# RefusalReason REFUSAL_REASON_OVERLOADED and REFUSAL_REASON_RATE_LIMITED
# (contract v30), by number: read whatever the image's generated code holds.
OVERLOADED = 32
RATE_LIMITED = 33


class Reporting(reporting.Role):
    seed = False


def flood_mode():
    try:
        with open(FLOOD_MODE, encoding="utf-8") as held:
            return held.read().strip()
    except OSError:
        return ""


def refusal_of(failed):
    """The reason a refusal names, and its retry-after, from its trailing
    metadata (W4.4): none where it names none."""
    if not isinstance(failed, grpc.aio.AioRpcError):
        return None, 0
    for key, value in failed.trailing_metadata() or ():
        if key == REFUSAL_METADATA:
            said = sidecar_pb2.Refusal.FromString(value)
            return said.reason, said.retry_after_seconds
    return None, 0


async def flood_seed(plugin, dataset):
    if os.path.exists("/tmp/seeded"):
        return
    stub = plugin._operations()
    # Each row's valid time a constant of its key (the lake's rule 5): a pod
    # rescheduled seeds again, its /tmp gone, and restates nothing.
    began = 1_793_491_200_000_000_000  # 2026-11-01T00:00:00Z
    for batch in range(FLOOD_ROWS // 500):
        prices = []
        for n in range(500):
            at = began + (batch * 500 + n) * 1_000_000
            key = f"flood-{batch}-{n}"
            prices.append(ops.Price(
                meta=ops.ObservationMeta(
                    row_key=key, subjects=[ops.SubjectRef(entity_id=INSTRUMENT)],
                    source=ops.Source(dataset=dataset), valid_from_ns=at,
                    source_times=[ops.SourceTime(kind=ops.SourceTimeKind.Value("SOURCE_TIME_KIND_PUBLISHED"),
                                                 at_ns=at)],
                    raw=plugin.raw_record(f"flood/{key}")),
                kind=ops.PriceKind.PRICE_KIND_LAST,
                price=ops.Money(amount=decimal(f"{100 + n % 50}.25"), instrument_id=USD),
                basis=ops.PriceBasis.PRICE_BASIS_PER_UNIT))
        while True:
            try:
                await bounded(stub.RecordPrices(ops.RecordPricesParams(prices=prices)))
                break
            except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
                say(f"driver: flood: seeding batch {batch} again ({words(failed)})")
                # Told to stop: stops, seeded or not.
                if stopped():
                    return
                await asyncio.sleep(1.0)
    kept("/tmp/seeded", "SEEDED")
    say(f"driver: flood: {FLOOD_ROWS} rows recorded in {dataset}")


async def drive_flood(plugin):
    dataset = f"{plugin.identity.instance_id}:{DATASET_KEY}"
    # Its wants heard on the one stream, as `meridian.run` hears a role's
    # events (`connect` starts none): each served (contract v30, stage B).
    from meridian.receive import follow
    hearing = asyncio.create_task(
        follow(plugin, {"ObservationsWanted": lambda heard: serve_want(plugin, heard)}, seed=False))
    hearing.add_done_callback(
        lambda done: done.cancelled() or not done.exception()
        or say(f"driver: flood: its wants no longer heard ({done.exception()})"))
    await flood_seed(plugin, dataset)
    stub = plugin._operations()
    heard = {"answered": 0, "overloaded": 0, "rate_limited": 0, "other": 0}
    # What each `other` was: a read given up, or a status not for load.
    others = {}
    least = [0]

    def other(why):
        heard["other"] += 1
        others[why] = others.get(why, 0) + 1

    def refused(failed):
        reason, retry = refusal_of(failed)
        if failed.code() == grpc.StatusCode.RESOURCE_EXHAUSTED and reason == OVERLOADED:
            heard["overloaded"] += 1
        elif failed.code() == grpc.StatusCode.RESOURCE_EXHAUSTED and reason == RATE_LIMITED:
            heard["rate_limited"] += 1
        else:
            other(failed.code().name + (f"_{reason}" if reason else ""))
            return
        least[0] = retry if not least[0] else min(least[0], retry)

    def what_heard(mode):
        said = (f"FLOOD {mode} answered={heard['answered']} overloaded={heard['overloaded']} "
                f"rate_limited={heard['rate_limited']} other={heard['other']} retry_after_min={least[0]}")
        return said + "".join(f" other_{why}={n}" for why, n in sorted(others.items()))

    def asked(cursor, page):
        return ops.ListPricesParams(
            subjects=[ops.SubjectRef(entity_id=INSTRUMENT)],
            sources=ops.SourceChoice(named=[dataset]),
            valid_from_ns=1, valid_until_ns=time.time_ns(), page_size=page, cursor=cursor)

    async def whole(mode):
        while flood_mode() == mode:
            cursor = ""
            try:
                while flood_mode() == mode:
                    listed = await asyncio.wait_for(
                        stub.ListPrices(asked(cursor, 500)), FLOOD_PATIENCE)
                    cursor = listed.next_cursor
                    if not cursor:
                        break
                heard["answered"] += 1
            except asyncio.TimeoutError:
                other("timeout")
            except grpc.aio.AioRpcError as failed:
                refused(failed)
                await asyncio.sleep(0.05)

    async def one_page(mode):
        while flood_mode() == mode:
            try:
                await bounded(stub.ListPrices(asked("", 1)))
                heard["answered"] += 1
            except asyncio.TimeoutError:
                other("timeout")
            except grpc.aio.AioRpcError as failed:
                refused(failed)

    async def said(mode):
        while flood_mode() == mode:
            await asyncio.sleep(5)
            line = what_heard(mode)
            say(line)
            kept("/tmp/flood.log", line)

    async def calm():
        """After a flood, one reader asking one page a second, as a plugin
        does, waiting out each refusal's retry-after: each read says what
        it was -- `answered` the reads answered since the last refusal, and
        the last refusal's reason 1 where the last read was refused -- so
        the runner sees the flooder answered again (the plan's Track 7)."""
        while flood_mode() == "calm" and not stopped():
            last = {"answered": 0, "overloaded": 0, "rate_limited": 0, "other": 0}
            wait = 1.0
            try:
                await bounded(stub.ListPrices(asked("", 1)))
                streak[0] += 1
                last["answered"] = streak[0]
            except asyncio.TimeoutError:
                streak[0] = 0
                last["other"] = 1
            except grpc.aio.AioRpcError as failed:
                streak[0] = 0
                reason, retry = refusal_of(failed)
                if failed.code() == grpc.StatusCode.RESOURCE_EXHAUSTED and reason == OVERLOADED:
                    last["overloaded"] = 1
                elif failed.code() == grpc.StatusCode.RESOURCE_EXHAUSTED and reason == RATE_LIMITED:
                    last["rate_limited"] = 1
                else:
                    last["other"] = 1
                wait = max(wait, float(retry))
            line = (f"FLOOD calm answered={last['answered']} overloaded={last['overloaded']} "
                    f"rate_limited={last['rate_limited']} other={last['other']} retry_after_min=0")
            kept("/tmp/flood.log", line)
            await asyncio.sleep(wait)

    async def unkept():
        """Its dataset served, not kept, read a price at a time: each read
        raises a want, which this instance serves, and is answered the price
        served -- whichever of the lake's members took it, a reader handing
        it to the writer (contract v30, stage B). Each read says what it was,
        as `UNKEPT answered=N served=N other=N ms=N why=<dataset>/<reason>,...`
        in /tmp/unkept.log: `served` the reads answered with the price served
        for them; the last read's time and what it said it could not answer."""
        named = f"{plugin.identity.instance_id}:{UNKEPT_KEY}"
        said = {"answered": 0, "served": 0, "other": 0}
        while flood_mode() == "unkept" and not stopped():
            began = time.monotonic()
            why = ""
            try:
                listed = await bounded(stub.ListPrices(ops.ListPricesParams(
                    subjects=[ops.SubjectRef(entity_id=INSTRUMENT)],
                    sources=ops.SourceChoice(named=[named]))))
                said["answered"] += 1
                if any(p.meta.row_key.startswith("unkept-") for p in listed.prices):
                    said["served"] += 1
                why = ",".join(f"{u.dataset}/{u.reason}" for u in listed.unanswered)
            except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
                said["other"] += 1
                why = words(failed)[:120].replace(" ", "_")
                say(f"driver: flood: a read of {named} failed ({words(failed)})")
            ms = int((time.monotonic() - began) * 1000)
            kept("/tmp/unkept.log",
                 f"UNKEPT answered={said['answered']} served={said['served']} other={said['other']} "
                 f"ms={ms} why={why or '-'}")
            await asyncio.sleep(0.5)

    streak = [0]
    while not stopped():
        mode = flood_mode()
        if mode == "unkept":
            await unkept()
        elif mode == "calm":
            streak[0] = 0
            await calm()
        elif mode in ("pressure", "noisy"):
            for key in heard:
                heard[key] = 0
            others.clear()
            least[0] = 0
            readers = [whole(mode) for _ in range(FLOOD_PRESSURE)] if mode == "pressure" else \
                [one_page(mode) for _ in range(FLOOD_NOISY)]
            await asyncio.gather(said(mode), *readers)
            line = what_heard(mode) + " END"
            say(line)
            kept("/tmp/flood.log", line)
        await asyncio.sleep(1.0)


# ── standing -> analytics: the standing-coordinator class's subject ────────
#
# Contract v30 (W12.4; spec/one-data-access-model section 4, question 13):
# a standing job over the first account, held by the SDK's view. A view
# whose job the analytics component no longer holds -- its coordinator
# killed -- hears a loss at its sidecar's next renewal, reads the job again,
# is answered NOT_FOUND and registers it again, under another job ID, with
# a live coordinator. Each ID it is held under is kept.

async def drive_standing(role):
    from meridian import analytics

    jobs = analytics.Jobs()
    # The Treasury curve, a job over no account (the component answers one
    # asking only it; W12.1): this run's accounts hold no position, so a
    # job over one has no base currency the stores answer and is refused,
    # and this run holds no curve, so its figure is not evaluated -- the
    # class is about where the job is held, not its figures. The SDK's
    # builder asks for an account, so the job is built and then cleared of it.
    job = analytics.job(analytics.measure("treasury_curve", refresh="per_change"),
                        accounts=ACCOUNTS[:1], standing=True)
    del job.account_ids[:]
    view = None
    while not stopped() and view is None:
        try:
            view = await asyncio.wait_for(jobs.standing(role, job), 60)
        except Exception as failed:  # noqa: BLE001 - registered again until it holds
            # What the call said, and the status under it: a CallFailed
            # names only its type in words().
            cause = failed.__cause__ or failed
            said = words(cause) if isinstance(cause, grpc.aio.AioRpcError) else str(failed)
            say(f"driver: standing: not held yet ({type(failed).__name__}: {said[:400]})")
            await asyncio.sleep(2)
    last = ""
    while not stopped():
        held = tuple(getattr(view._shared.selector, "job_ids", ()))
        now = held[0] if held else ""
        if now and now != last:
            line = f"HELD {now} {int(time.time() * 1000)}"
            kept("/tmp/standing.log", line)
            say(f"driver: standing: {line}")
            last = now
        await asyncio.sleep(0.25)


# ── Start ───────────────────────────────────────────────────────────────────


# ── The write-scale step's load (step 3d, stage T; section 15) ─────────────
#
# DRIVER_ROLE=scale-data and scale-oms, in e2e-cluster-scaled alone: each
# store's writes at saturation, so the run can read what one writer group
# on one database commits a second against two groups on two. Nothing is
# written until the run makes /tmp/scale, and nothing once it removes it.
# While it is there SCALE_WORKERS workers each write as fast as they are
# answered: scale-data a batch of SCALE_BATCH prices into one of the
# datasets of DRIVER_SCALE_KEYS, taken in turn (several, so they fall to
# both writer groups); scale-oms a telephoned order into one of
# DRIVER_ACCOUNTS, taken in turn -- or of those /tmp/scale names, by commas,
# where it names any (the run names as many of each writer group's). What
# was committed is read by the run in
# the stores' own rows, never here; this keeps a count a second in
# /tmp/scale.log (`SCALE <unix ms> <answered> <refused>`) and the last
# refusal's words in /tmp/scale-refused, for reading a run that wrote
# nothing. No figure is printed.

SCALE = "/tmp/scale"
SCALE_KEYS = [k for k in os.environ.get("DRIVER_SCALE_KEYS", "").split(",") if k]
SCALE_WORKERS = int(os.environ.get("DRIVER_SCALE_WORKERS", "48"))
SCALE_BATCH = 20


class Scale:
    def __init__(self):
        self.answered = 0
        self.refused = 0
        self.turn = 0
        self.named = []
        self.read_at = None

    def told(self):
        """Whether to write now, and what /tmp/scale names, read again
        when the file changes."""
        try:
            at = os.stat(SCALE).st_mtime_ns
        except OSError:
            self.read_at = None
            return False
        if at != self.read_at:
            with open(SCALE, encoding="utf-8") as held:
                self.named = [a for a in held.read().strip().split(",") if a]
            self.read_at = at
        return True

    def next(self, among):
        self.turn += 1
        return among[self.turn % len(among)]

    async def count_forever(self):
        while not stopped():
            await asyncio.sleep(1)
            if self.answered or self.refused:
                kept("/tmp/scale.log", f"SCALE {int(time.time() * 1000)} {self.answered} {self.refused}")
                self.answered = self.refused = 0

    async def work(self, write):
        while not stopped():
            if not self.told():
                await asyncio.sleep(0.2)
                continue
            try:
                await bounded(write())
                self.answered += 1
            except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
                self.refused += 1
                with open("/tmp/scale-refused", "w", encoding="utf-8") as held:
                    held.write(f"{int(time.time() * 1000)} {refusal_words(failed)}\n")
                await asyncio.sleep(0.05)


async def drive_scale_data(plugin, gen):
    stub = plugin._operations()
    scale = Scale()
    started = time.time_ns() // 1_000_000
    numbers = iter(range(1, 1 << 62))

    def write():
        dataset = f"{plugin.identity.instance_id}:{scale.next(SCALE_KEYS)}"
        now = time.time_ns()
        prices = []
        for at in range(SCALE_BATCH):
            # A row key of its own, with one valid time (the lake's rule 5).
            key = f"scale-{gen}-{started}-{next(numbers)}"
            prices.append(ops.Price(
                meta=ops.ObservationMeta(
                    row_key=key, subjects=[ops.SubjectRef(entity_id=INSTRUMENT)],
                    source=ops.Source(dataset=dataset), valid_from_ns=now + at,
                    source_times=[ops.SourceTime(kind=ops.SourceTimeKind.Value("SOURCE_TIME_KIND_PUBLISHED"),
                                                 at_ns=now + at)],
                    raw=plugin.raw_record(f"scale/{key}")),
                kind=ops.PriceKind.PRICE_KIND_LAST,
                price=ops.Money(amount=decimal("101.25"), instrument_id=USD),
                basis=ops.PriceBasis.PRICE_BASIS_PER_UNIT))
        return stub.RecordPrices(ops.RecordPricesParams(prices=prices))

    await asyncio.gather(scale.count_forever(), *(scale.work(write) for _ in range(SCALE_WORKERS)))


async def drive_scale_oms(plugin, gen):
    stub = plugin._operations()
    scale = Scale()
    person = Person(plugin.identity.instance_id)
    started = time.time_ns() // 1_000_000
    numbers = iter(range(1, 1 << 62))

    def write():
        number = next(numbers)
        return stub.RecordOrder(ops.RecordOrderParams(
            account_id=scale.next(scale.named or ACCOUNTS), instrument_id=INSTRUMENT, side=ops.Side.SIDE_BUY,
            quantity=decimal("1"), order_type=ops.OrderType.ORDER_TYPE_MARKET,
            time_in_force=ops.TimeInForce.TIME_IN_FORCE_GOOD_TILL_CANCELLED,
            venue_order_id=str(number), origin=ops.OrderOrigin.ORDER_ORIGIN_TELEPHONED,
            idempotency_key=f"scale-{gen}-{started}-{number}", acting_for=person.assertion()))

    await asyncio.gather(scale.count_forever(), *(scale.work(write) for _ in range(SCALE_WORKERS)))


# DRIVER_ROLE=shard and shard-reader, in e2e-cluster-scaled alone (step 3d,
# checkpoint 5, item 33; the resilience spec's section 7): the cluster proof
# that each subject is served once.
#
# `shard` runs in each pod of a plugin instance launched at two shards with
# a standby pod (run.py's SHARDED), as a data plugin runs: on meridian.run,
# nothing of it started until its sidecar admits it, its classes stopped
# when its sidecar is lost, and started again as its pod takes a shard. It
# declares one streamed dataset (`<instance>:served`) and serves its share
# of each want of it: the subjects its sidecar hands it, by rendezvous
# hashing over the live shards. Once a second it records a price of each
# subject it serves, under a row key fixed by the subject and the second
# (`shard-<subject>-<unix s>`, valid at that second, its price a function of
# both): two shards serving one subject in one second record the same row.
# Each subject of a batch the lake acknowledged whole is kept in
# /tmp/served.log as `SERVED <pod> <shard> <subject> <unix s> <unix ms>`:
# the second its row is of, and when it was acknowledged. Each batch
# refused is kept in /tmp/shard-refused.log as `REFUSED <unix ms> <words>`,
# and each want heard and withdrawn in /tmp/shard-wants.log.
#
# Beside it, as the stand-in it replaces did, it says whether the members'
# token is in its container (SHARD_NO_TOKEN; item 31) and, each time it
# changes, whether its sidecar answers on loopback (SHARD_ANSWERED,
# SHARD_GONE): where a plugin's Register is answered, so whether its run()
# may start. run.py reads the same words.
#
# `shard-reader` holds reporting: one view (`plugin.subscribe`) of the
# prices of DRIVER_SUBJECTS, open for the run, which its sidecar keeps as a
# standing want at the lake (W10.7; contract v28, amended 2026-10-30) of the
# dataset the lake chooses for it: the shard instance's, the one it is
# entitled to. Every ten seconds it says how many rows its view holds, in
# /tmp/reader.log as `READ <unix ms> <rows>`.

SHARD_KEY = "served"
SUBJECTS = [s for s in os.environ.get("DRIVER_SUBJECTS", "").split(",") if s]
TOKEN = "/var/run/secrets/kubernetes.io/serviceaccount/token"
SHARD_NO_TOKEN = "shard: no lease token in the plugin container"
SHARD_ANSWERED = "shard: the sidecar answers"
SHARD_GONE = "shard: the sidecar no longer answers"
# Where a plugin's Register is answered: its sidecar, on loopback.
SIDECAR_PORT = 9191


def shard_number():
    """The shard this pod serves, as its sidecar's Register answered it
    (item 30); `?` before it has."""
    from meridian import edge

    serving = getattr(edge, "_serving", None)
    return str(serving[0]) if serving else "?"


def shard_price(subject, second):
    """A subject's price at a second: the same in every pod."""
    cents = int.from_bytes(hashlib.sha256(f"{subject}/{second}".encode()).digest()[:4], "big") % 10000
    return f"{100 + cents // 100}.{cents % 100:02d}"


async def sidecar_answers():
    """Said each time it changes: whether the sidecar answers on loopback."""
    was = False
    while True:
        try:
            _, writer = await asyncio.wait_for(asyncio.open_connection("127.0.0.1", SIDECAR_PORT), 1)
            writer.close()
            now = True
        except (OSError, asyncio.TimeoutError):
            now = False
        if now != was:
            say(SHARD_ANSWERED if now else SHARD_GONE)
            was = now
        await asyncio.sleep(0.5)


class Sharding(data.Role):
    seed = False

    async def on_start(self):
        # Afresh for each serving: what an earlier one held is not this one's.
        self.serving = {}
        say(f"driver: shard: serving as shard {shard_number()}")

    async def on_observations_wanted(self, heard):
        want = heard.message
        if want.dataset != f"{self.plugin.identity.instance_id}:{SHARD_KEY}":
            return
        subjects = sorted({subject.entity_id for subject in want.subjects})
        self.serving[want.want_id] = subjects
        kept("/tmp/shard-wants.log", f"WANT {int(time.time() * 1000)} {want.want_id} "
                                     f"standing={want.standing} {','.join(subjects)}")

    async def on_want_withdrawn(self, heard):
        self.serving.pop(heard.message.want_id, None)
        kept("/tmp/shard-wants.log", f"WITHDRAWN {int(time.time() * 1000)} {heard.message.want_id}")

    async def run(self):
        stub = self.plugin._operations()
        dataset = f"{self.plugin.identity.instance_id}:{SHARD_KEY}"
        pod = os.environ.get("HOSTNAME", "?")
        while not stopped():
            # Just past the next second, so each batch names the second it
            # is recorded in.
            await asyncio.sleep(1.05 - time.time() % 1)
            subjects = sorted(set().union(*self.serving.values())) if self.serving else []
            if not subjects:
                continue
            second = int(time.time())
            prices = [ops.Price(
                meta=ops.ObservationMeta(
                    row_key=f"shard-{subject}-{second}",
                    subjects=[ops.SubjectRef(entity_id=subject)],
                    source=ops.Source(dataset=dataset), valid_from_ns=second * 1_000_000_000,
                    source_times=[ops.SourceTime(kind=ops.SourceTimeKind.Value("SOURCE_TIME_KIND_PUBLISHED"),
                                                 at_ns=second * 1_000_000_000)],
                    raw=self.plugin.raw_record(f"served/{subject}/{second}")),
                kind=ops.PriceKind.PRICE_KIND_LAST,
                price=ops.Money(amount=decimal(shard_price(subject, second)), instrument_id=USD),
                basis=ops.PriceBasis.PRICE_BASIS_PER_UNIT) for subject in subjects]
            try:
                done = await bounded(stub.RecordPrices(ops.RecordPricesParams(prices=prices)))
            except (asyncio.TimeoutError, grpc.aio.AioRpcError) as failed:
                kept("/tmp/shard-refused.log", f"REFUSED {int(time.time() * 1000)} {refusal_words(failed)}")
                say(f"driver: shard: a batch of {len(prices)} refused ({refusal_words(failed)[:300]})")
                continue
            at = int(time.time() * 1000)
            if done.recorded + done.restated + done.unchanged != len(prices):
                kept("/tmp/shard-refused.log",
                     f"REFUSED {at} UNCOUNTED: answered recorded {done.recorded}, restated {done.restated}, "
                     f"unchanged {done.unchanged} for {len(prices)} rows")
                continue
            shard = shard_number()
            with open("/tmp/served.log", "a", encoding="utf-8") as held:
                held.write("".join(f"SERVED {pod} {shard} {subject} {second} {at}\n" for subject in subjects))


async def drive_shard():
    if not os.path.exists(TOKEN):
        say(SHARD_NO_TOKEN)
    answers = asyncio.create_task(sidecar_answers())
    from meridian import run_async

    try:
        await run_async(declaration(), [Sharding()])
    finally:
        answers.cancel()


async def drive_shard_reader(plugin):
    view = await plugin.subscribe("PricesRecorded", subjects=SUBJECTS)
    say(f"driver: shard-reader: a view of {len(SUBJECTS)} subjects' prices open")
    while not stopped():
        await asyncio.sleep(10)
        kept("/tmp/reader.log", f"READ {int(time.time() * 1000)} {len(view.rows())}")


def declaration():
    if ROLE == "shard":
        return Declaration(catalogue=[DatasetDeclaration(
            key=SHARD_KEY, vendor="e2e failover driver", data_types=["meridian.v1.Price"], modes=["stream"],
            terms_and_pricing="A test-only driver's prices, served to the reader that wants them and kept.",
            day_time_zone="Etc/UTC")])
    if ROLE == "scale-data":
        return Declaration(catalogue=[DatasetDeclaration(
            key=key, vendor="e2e failover driver", data_types=["meridian.v1.Price"], modes=["push"],
            terms_and_pricing="A test-only driver's own prices, written for the run and kept by it.",
            day_time_zone="Etc/UTC") for key in SCALE_KEYS])
    if ROLE in ("data", "flood"):
        catalogue = [DatasetDeclaration(
            key=DATASET_KEY, vendor="e2e failover driver", data_types=["meridian.v1.Price"],
            modes=["push"],
            # Contract v30 (ruling 1032): the vendor's terms in words; the
            # licence is the deployment admin's.
            terms_and_pricing="A test-only driver's own prices, written for the run and kept by it.",
            day_time_zone="Etc/UTC")]
        if ROLE == "flood":
            catalogue.append(DatasetDeclaration(
                key=UNKEPT_KEY, vendor="e2e failover driver", data_types=["meridian.v1.Price"],
                modes=["pull"],
                terms_and_pricing="A test-only driver's prices, served when asked and not kept.",
                day_time_zone="Etc/UTC"))
        return Declaration(catalogue=catalogue)
    return Declaration()


async def main():
    if ROLE not in ("data", "oms", "desk", "flood", "standing", "scale-data", "scale-oms", "shard",
                    "shard-reader") or not INSTRUMENT or (
            ROLE in ("oms", "desk", "scale-oms") and not (ACCOUNTS and SIGNER_SEED and SIGNER_KEY_ID)) or (
            ROLE == "scale-data" and not SCALE_KEYS) or (ROLE == "shard" and not USD) or (
            ROLE == "shard-reader" and not SUBJECTS):
        raise SystemExit("driver: DRIVER_ROLE is data, oms or desk, with DRIVER_INSTRUMENT (and, for oms and "
                         "desk, DRIVER_ACCOUNTS and the run's signer, DRIVER_SIGNER_SEED and DRIVER_SIGNER_KEY_ID)")
    gen = generation()
    say(f"driver: {ROLE}, start {gen}")
    if ROLE == "shard":
        # Its process's life on meridian.run, as a data plugin's: it waits
        # for its sidecar without limit, and leaves only when stopped.
        await drive_shard()
        while True:
            await asyncio.sleep(3600)
    roles = {"data": [Recording(), Reporting()], "oms": [Managing()], "desk": [Desking(), Managing()],
             "flood": [Recording(), Reporting()], "standing": [Reporting()],
             "scale-data": [Recording()], "scale-oms": [Managing()], "shard-reader": [Reporting()]}[ROLE]
    # Registered once the sidecar and the stores behind it answer: refused
    # while the deployment is not yet configured, asked again.
    while not stopped():
        try:
            async with await connect(declaration=declaration(), roles=roles) as plugin:
                say(f"driver: registered as {plugin.identity.instance_id}")
                kept("/tmp/registered", f"REGISTERED {int(time.time() * 1000)}")
                await {"data": lambda: asyncio.gather(drive_data(plugin, gen), drive_page(plugin)),
                       "oms": lambda: drive_oms(plugin, gen),
                       "desk": lambda: drive_desk(plugin), "flood": lambda: drive_flood(plugin),
                       "standing": lambda: drive_standing(roles[0]),
                       "scale-data": lambda: drive_scale_data(plugin, gen),
                       "scale-oms": lambda: drive_scale_oms(plugin, gen),
                       "shard-reader": lambda: drive_shard_reader(plugin)}[ROLE]()
            break
        except Exception as failed:  # noqa: BLE001 - every start failure is waited out
            say(f"driver: not registered yet ({type(failed).__name__}: {str(failed)[:200]})")
            await asyncio.sleep(3)
    kept("/tmp/done", "DONE")
    say("DONE")
    # Kept running, writing nothing, so the run reads its files from a
    # container that is there (an exit would be restarted, and the reads
    # would meet one starting).
    while True:
        await asyncio.sleep(3600)


if __name__ == "__main__":
    random.seed()
    asyncio.run(main())
    sys.exit(0)
