"""Core's stand-ins for `make e2e-lake` (contract v18, W10): a `dgm` that
serves the lake from an exchange file, and a reading plugin, each on the
Python SDK, each answering its page as JSON so the harness's runner can drive
it (`page --level ... PATH`).

STAND_IN_LAKE=dgm declares a catalogue of three datasets -- `daily` (prices
and bars, pull and push, on a venue), `ticks` (prices, streamed, a 30-second
cadence) and `fx` (prices, pushed) -- and serves at its page:

  /resolve   resolves BTC, USDC and EUR's cash instrument (W3.1), answering
             their records' IDs;
  /record    records from the exchange file the closes of two days and a bar
             for the first, a BTC price in USDC on the token's own
             instrument, and an FX rate (W10.4);
  /restate   records the second day's close again, changed: its version 2;
  /wants     what the lake asked of it and withdrew (W10.7).

On a want naming a business date the exchange file holds, it records that
day's close against the want; on a standing want it records nothing, and
hears it withdrawn.

STAND_IN_LAKE=reader serves:

  /follow?subject=ID...                     hears what is recorded for them;
  /heard                                    what it heard, each row's dataset,
                                            row key and version;
  /prices?subject=ID&date=D[&kind=K][&dataset=D][&as_of=N][&latest=1]
                                            one read, its rows and what it left
                                            unanswered;
  /datasets                                 the datasets it may read;
  /invalid-date?subject=ID                  a read naming 2026-02-30, sent past
                                            the SDK's own check, as the sidecar
                                            answers it.

The exchange file (STAND_IN_EXCHANGES, a directory holding exchange.json) is
shaped on a vendor's responses with invented values: candles as
[time, low, high, open, close, volume], text for every number, so nothing
passes through a float. `make e2e-lake` reads the synthetic one committed
beside this file; `make e2e-recorded` the directory RECORDED names, a
developer's recorded exchanges, never committed.
"""
import asyncio
import http.server
import json
import os
import threading
import time
import urllib.parse
from datetime import date, datetime, timezone
from decimal import Decimal

import meridian
from meridian import (
    Bar,
    DatasetDeclaration,
    DatasetLicence,
    Declaration,
    Interface,
    Money,
    ObservationMeta,
    Page,
    Price,
    SourceChoice,
    SourceTime,
)
from meridian.operations import as_money
from meridian.plugin.v1 import operations_pb2 as ops

KIND = os.environ.get("STAND_IN_LAKE", "dgm")
PORT = 8088
DAY_NS = 86_400 * 10**9
VENUE = "VEN-01JA0000000000000CBEXC"
EXCHANGES = os.environ.get("STAND_IN_EXCHANGES", "/stand-in/exchanges")
LOOP = asyncio.new_event_loop()


def log(**said):
    print("LAKE " + json.dumps(said, sort_keys=True), flush=True)


def exchange():
    with open(os.path.join(EXCHANGES, "exchange.json"), encoding="utf-8") as held:
        return json.load(held)


def day_of(seconds):
    return datetime.fromtimestamp(seconds, tz=timezone.utc).date()


# ── The dgm ─────────────────────────────────────────────────────────────


CATALOGUE = [
    DatasetDeclaration(
        key="daily",
        vendor="Coinbase",
        data_types=["meridian.v1.Price", "meridian.v1.Bar"],
        modes=["pull", "push"],
        cadence=86_400,
        history=3650,
        licence_default=DatasetLicence(kept=True, derived_use=True, display=True),
        day_time_zone="Etc/UTC",
        day_end_minute=0,
        venue_id=VENUE,
    ),
    DatasetDeclaration(
        key="ticks",
        vendor="Coinbase",
        data_types=["meridian.v1.Price"],
        modes=["stream"],
        cadence=30,
        licence_default=DatasetLicence(kept=True, display=True),
        day_time_zone="Etc/UTC",
    ),
    DatasetDeclaration(
        key="fx",
        vendor="Federal Reserve",
        data_types=["meridian.v1.Price"],
        modes=["push"],
        cadence=604_800,
        licence_default=DatasetLicence(kept=True, derived_use=True, display=True),
        day_time_zone="America/New_York",
    ),
]


class Dgm:
    def __init__(self, plugin):
        self.plugin = plugin
        self.ids = {}
        self.wants = []
        self.withdrawn = []

    def dataset(self, key):
        return f"{self.plugin.identity.instance_id}:{key}"

    async def resolve(self):
        asked = {
            "btc": ([ops.Identifier(scheme="symbol", value="BTC", source="coinbase")], "crypto_asset"),
            "usdc": ([ops.Identifier(scheme="symbol", value="USDC", source="coinbase")], "crypto_asset"),
            "eur": ([ops.Identifier(scheme="iso4217", value="EUR")], None),
        }
        for name, (identifiers, stated) in asked.items():
            if name in self.ids:
                continue
            found = await self.plugin.resolve_identifier(
                identifiers=identifiers, stated_asset_class=stated)
            if not found.instrument_id:
                raise RuntimeError(f"{name} did not resolve: {found}")
            self.ids[name] = found.instrument_id
        return dict(self.ids)

    def meta(self, row_key, subject, dataset, seconds, venue=""):
        return ObservationMeta(
            row_key=row_key,
            subjects=[ops.SubjectRef(entity_id=self.ids[subject])],
            source=ops.Source(dataset=self.dataset(dataset), venue_id=venue),
            valid_from_ns=seconds * 10**9,
            valid_until_ns=seconds * 10**9 + DAY_NS,
            business_date=day_of(seconds),
            source_times=[SourceTime(kind="published", at_ns=seconds * 10**9 + DAY_NS)],
            raw=self.plugin.raw_record(f"candles/{row_key}"),
        )

    def close(self, candle):
        seconds, _low, _high, _open, close, _volume = candle
        return Price(
            meta=self.meta(f"BTC-USD:1d:{seconds}", "btc", "daily", seconds, VENUE),
            kind="close",
            price=Money(Decimal(close), "USD"),
            basis="per_unit",
        )

    def bar(self, candle):
        seconds, low, high, open_, close, volume = candle
        return Bar(
            meta=self.meta(f"BTC-USD:1d:{seconds}", "btc", "daily", seconds, VENUE),
            open=Money(Decimal(open_), "USD"),
            high=Money(Decimal(high), "USD"),
            low=Money(Decimal(low), "USD"),
            close=Money(Decimal(close), "USD"),
            volume=Decimal(volume),
        )

    async def record(self):
        await self.resolve()
        held = exchange()
        candles = held["candles"]["BTC-USD"]
        first, second = candles[1], candles[2]
        prices = [self.close(first), self.close(second)]
        seconds, last = held["tickers"]["BTC-USDC"][0]
        # A stablecoin's quote, on the token's own cash instrument.
        prices.append(Price(
            meta=self.meta(f"BTC-USDC:last:{seconds}", "btc", "daily", seconds, VENUE),
            kind="last",
            price=Money(Decimal(last), instrument_id=self.ids["usdc"]),
            basis="per_unit",
        ))
        recorded = await self.plugin.record_prices(prices=prices)
        bars = await self.plugin.record_bars(bars=[self.bar(first)])
        # An FX rate: the price of EUR's cash instrument, in dollars.
        day, rate = held["rates"]["EUR"][0]
        at = int(datetime.fromisoformat(day).replace(tzinfo=timezone.utc).timestamp())
        fx = await self.plugin.record_prices(prices=[Price(
            meta=self.meta(f"EUR-USD:{day}", "eur", "fx", at),
            kind="close",
            price=Money(Decimal(rate), "USD"),
            basis="per_unit",
        )])
        said = {"prices": recorded.recorded, "bars": bars.recorded, "fx": fx.recorded,
                "restated": recorded.restated, "unchanged": recorded.unchanged}
        log(recorded=said)
        return said

    async def restate(self):
        await self.resolve()
        candle = exchange()["restated"]["BTC-USD"][0]
        done = await self.plugin.record_prices(prices=[self.close(candle)])
        said = {"recorded": done.recorded, "restated": done.restated, "unchanged": done.unchanged}
        log(restated=said)
        return said

    async def on_want(self, heard):
        want = heard.message
        self.wants.append({"want_id": want.want_id, "dataset": want.dataset,
                           "business_date": want.business_date, "standing": want.standing,
                           "subjects": [s.entity_id for s in want.subjects]})
        log(want=self.wants[-1])
        if want.standing or not want.business_date or not want.dataset.endswith(":daily"):
            return
        await self.resolve()
        wanted = date.fromisoformat(want.business_date)
        for candle in exchange()["candles"]["BTC-USD"]:
            if day_of(candle[0]) == wanted:
                done = await self.plugin.record_prices(prices=[self.close(candle)],
                                                       want_id=want.want_id)
                log(answered={"want_id": want.want_id, "recorded": done.recorded})
                return

    async def on_withdrawn(self, heard):
        self.withdrawn.append({"want_id": heard.message.want_id, "dataset": heard.message.dataset})
        log(withdrawn=self.withdrawn[-1])

    async def answer(self, path, query):
        if path == "/resolve":
            return await self.resolve()
        if path == "/record":
            return await self.record()
        if path == "/restate":
            return await self.restate()
        if path == "/wants":
            return {"wants": self.wants, "withdrawn": self.withdrawn}
        return {"page": "the lake's dgm stand-in"}


# ── The reader ──────────────────────────────────────────────────────────


def price_json(price):
    meta = price.meta
    money = as_money(price.price)
    return {
        "dataset": meta.source.dataset,
        "venue_id": meta.source.venue_id,
        "row_key": meta.row_key,
        "version": meta.version,
        "sequence": meta.sequence,
        "recorded_at_ns": meta.recorded_at_ns,
        "business_date": meta.business_date,
        "kind": ops.PriceKind.Name(price.kind),
        "amount": str(money.amount),
        "instrument_id": money.instrument_id,
        "currency_code": money.currency_code,
    }


class Reader:
    def __init__(self, plugin):
        self.plugin = plugin
        self.heard = []
        self.following = None

    async def on_price(self, heard):
        for price in heard.message.prices:
            self.heard.append(price_json(price))
            log(heard=self.heard[-1])

    async def follow(self, subjects):
        if self.following is None:
            self.following = asyncio.ensure_future(
                self.plugin.receive(prices_recorded=self.on_price, subjects=subjects))
        return {"following": subjects}

    async def prices(self, query):
        one = lambda name, default="": query.get(name, [default])[0]
        subjects = [ops.SubjectRef(entity_id=s) for s in query.get("subject", [])]
        named = query.get("dataset", [])
        sources = SourceChoice(named=named) if named else None
        asked = {"subjects": subjects, "sources": sources}
        if one("kind"):
            asked["kinds"] = [one("kind")]
        if one("date"):
            asked["business_date"] = one("date")
        if one("as_of"):
            asked["as_of_ns"] = int(one("as_of"))
        answer = await self.plugin.list_prices(**asked)
        return {
            "prices": [price_json(price) for price in answer.prices],
            "unanswered": [{"subject": u.subject.entity_id, "dataset": u.dataset,
                            "reason": ops.UnansweredReason.Name(u.reason)}
                           for u in answer.unanswered],
        }

    async def invalid_date(self, query):
        params = ops.ListPricesParams(
            subjects=[ops.SubjectRef(entity_id=s) for s in query.get("subject", [])],
            business_date="2026-02-30")
        try:
            await self.plugin._operations().ListPrices(params)
        except Exception as refused:  # the sidecar's refusal, as gRPC carries it
            details = getattr(refused, "details", None)
            return {"refused": details() if callable(details) else str(refused)}
        return {"refused": None}

    async def answer(self, path, query):
        if path == "/follow":
            return await self.follow(query.get("subject", []))
        if path == "/heard":
            return {"heard": self.heard}
        if path == "/prices":
            return await self.prices(query)
        if path == "/datasets":
            listed = await self.plugin.list_datasets()
            return {"datasets": [d.dataset for d in listed.datasets]}
        if path == "/invalid-date":
            return await self.invalid_date(query)
        return {"page": "the lake's reading stand-in"}


# ── Serving the page ────────────────────────────────────────────────────


def serve(served):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            parsed = urllib.parse.urlparse(self.path)
            future = asyncio.run_coroutine_threadsafe(
                served.answer(parsed.path, urllib.parse.parse_qs(parsed.query)), LOOP)
            try:
                body, status = json.dumps(future.result(timeout=60), sort_keys=True), 200
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
    if KIND == "dgm":
        plugin = await meridian.connect(
            interface=Interface(PORT, "Lake stand-in", pages=[Page("/", "Datasets", levels=["admin"])]),
            declaration=Declaration(catalogue=CATALOGUE),
        )
        served = Dgm(plugin)
        asyncio.ensure_future(plugin.receive(observations_wanted=served.on_want,
                                             want_withdrawn=served.on_withdrawn))
    else:
        plugin = await meridian.connect(
            interface=Interface(PORT, "Reader stand-in", pages=[Page("/", "Prices", levels=["read"])]),
        )
        served = Reader(plugin)
    log(registered=plugin.identity.instance_id, kind=KIND)
    threading.Thread(target=serve, args=(served,), daemon=True).start()
    while True:
        await asyncio.sleep(3600)


if __name__ == "__main__":
    asyncio.set_event_loop(LOOP)
    LOOP.run_until_complete(main())
