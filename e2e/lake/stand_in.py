"""Core's stand-ins for `make e2e-lake` (contract v18, W10): a data plugin that
serves the lake from an exchange file, and a reading plugin, each on the
Python SDK, each answering its page as JSON so the harness's runner can drive
it (`page --level ... PATH`).

STAND_IN_LAKE=data declares a catalogue of three datasets -- `daily` (prices
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

From contract v19 (the lake's 1b) it declares `live` as well -- trades and
quotes, streamed -- and serves:

  /trades    records three trades and three quotes of BTC on the venue, two
             quotes in dollars (the second for the first) and one in USDC;
  /late      records a late print, its valid time a day before, and the
             first trade withdrawn, as a new version;
  /identity  resolves a symbol and a FIGI to two records, then both together
             (ambiguous: the conflict listed, nothing minted or escalated);
  /resolve-as-of?scheme=S&value=V&source=N&as_of=NS
             one resolve as of a date, answering the record.

On a standing want of `live` it streams: a trade on each subject the want
names every two seconds, until the want is withdrawn, one stream per want
however often the want is delivered. It holds nothing it cannot lose, so a
restart ends the stream until the lake delivers the want again.

STAND_IN_LAKE=desk is an `ems` plugin hearing trades and quotes (a raw
receive of TradesRecorded and QuotesRecorded) and serving:

  /follow?subject=ID...      hears trades and quotes about them;
  /heard                     each trade and quote heard, with its sequence;
  /catch-up?after=N          ListTrades after the watermark live:N;
  /quotes?subject=ID         ListQuotes, the latest in force;
  /quotes-wanted?subject=ID  ListQuotes of a subject nothing recorded.

And from contract v20 (W10.6, W10.9, W3.1):

  /latest?subject=ID         ListTrades latest_recorded: its own last trade
                             per dataset and venue, for a restart's backfill;
  /licences                  ListDatasets: its own datasets' licences.

The wire is the SDK's own (SDK 0.24.0 carries v20's typed operations).

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
import sys
import threading
import time
import urllib.parse
from datetime import date, datetime, timezone
from decimal import Decimal



import meridian  # noqa: E402
from meridian import (  # noqa: E402
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
from meridian.operations import as_decimal, as_money  # noqa: E402
from meridian.plugin.v1 import operations_pb2 as ops  # noqa: E402

KIND = os.environ.get("STAND_IN_LAKE", "data")
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


# ── The data plugin ─────────────────────────────────────────────────────────────


def _v19_types(declaration, data_types):
    """A catalogue entry naming the 1b's data types, which the sidecar checks
    at registration; the SDK's own check is v18's until SDK 0.23.0."""
    object.__setattr__(declaration, "data_types", tuple(data_types))
    return declaration


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
    # Contract v19: trades and quotes, streamed, kept two days. The SDK's
    # v18 dictionary knows no Trade or Quote yet (SDK 0.23.0, row 18), so the
    # entry is checked as prices and then names its own types.
    _v19_types(DatasetDeclaration(
        key="live",
        vendor="Coinbase",
        data_types=["meridian.v1.Price"],
        modes=["stream"],
        licence_default=DatasetLicence(kept=True, retention_days=2, display=True),
        day_time_zone="Etc/UTC",
        day_end_minute=0,
        venue_id=VENUE,
    ), ["meridian.v1.Trade", "meridian.v1.Quote"]),
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
        self.streams = {}
        self.streamed = 0
        self.started = time.time_ns()

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

    async def stream(self, want):
        """Keep a standing want current, as a vendor's stream would: a trade
        on each subject every two seconds."""
        n = 0
        while True:
            n += 1
            at = time.time_ns()
            trades = [ops.Trade(
                meta=ops.ObservationMeta(
                    row_key=f"STREAM-{self.started}-{subject.entity_id}-{n}",
                    subjects=[subject],
                    source=ops.Source(dataset=want.dataset, venue_id=VENUE),
                    valid_from_ns=at),
                price=ops.Money(amount=_decimal("1.0001"), currency_code="USD"),
                quantity=_decimal("10"),
                attributes=ops.TradeAttributes(),
            ) for subject in want.subjects]
            try:
                done = await self.plugin._operations().RecordTrades(
                    ops.RecordTradesParams(trades=trades, want_id=want.want_id))
                self.streamed += done.recorded
            except Exception as failed:  # said, and the stream goes on
                log(stream_failed=str(failed))
            await asyncio.sleep(2)

    async def on_want(self, heard):
        want = heard.message
        self.wants.append({"want_id": want.want_id, "dataset": want.dataset,
                           "business_date": want.business_date, "standing": want.standing,
                           "subjects": [s.entity_id for s in want.subjects]})
        log(want=self.wants[-1])
        # One stream per want, however often it is delivered.
        if want.standing and want.dataset.endswith(":live") and want.want_id not in self.streams:
            self.streams[want.want_id] = asyncio.ensure_future(self.stream(want))
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
        streaming = self.streams.pop(heard.message.want_id, None)
        if streaming:
            streaming.cancel()
        self.withdrawn.append({"want_id": heard.message.want_id, "dataset": heard.message.dataset})
        log(withdrawn=self.withdrawn[-1])

    # ── The lake's 1b (contract v19) ──

    def live_meta(self, row_key, at_ns):
        return ops.ObservationMeta(
            row_key=row_key,
            subjects=[ops.SubjectRef(entity_id=self.ids["btc"])],
            source=ops.Source(dataset=self.dataset("live"), venue_id=VENUE),
            valid_from_ns=at_ns,
        )

    def money(self, amount, code="USD", instrument=""):
        return ops.Money(amount=_decimal(amount), currency_code=code, instrument_id=instrument)

    def trade(self, row_key, at_ns, price, cancelled=False):
        return ops.Trade(
            meta=self.live_meta(row_key, at_ns),
            price=self.money(price),
            quantity=_decimal("0.25"),
            attributes=ops.TradeAttributes(),
            aggressor=ops.AGGRESSOR_BUY,
            source_sequence=int(row_key.split("-")[-1]) if row_key[-1].isdigit() else 0,
            cancelled=cancelled,
        )

    async def trades(self):
        await self.resolve()
        now = time.time_ns()
        self.at = now
        trades = [self.trade(f"BTC-USD-T-{n}", now + n * 1000, f"62000.{n}") for n in (1, 2, 3)]
        done = await self.plugin._operations().RecordTrades(ops.RecordTradesParams(trades=trades))
        quotes = [
            ops.Quote(meta=self.live_meta("BTC-USD-Q-1", now), bid=self.money("61999"),
                      ask=self.money("62001"), bid_quantity=_decimal("1"), ask_quantity=_decimal("2")),
            ops.Quote(meta=self.live_meta("BTC-USD-Q-2", now + 5000), bid=self.money("62000"),
                      ask=self.money("62002"), bid_quantity=_decimal("1"), ask_quantity=_decimal("2")),
            ops.Quote(meta=self.live_meta("BTC-USDC-Q-1", now + 3000),
                      bid=self.money("61998", "", self.ids["usdc"]),
                      ask=self.money("62003", "", self.ids["usdc"])),
        ]
        quoted = await self.plugin._operations().RecordQuotes(ops.RecordQuotesParams(quotes=quotes))
        said = {"trades": done.recorded, "quotes": quoted.recorded,
                "head": max(p.sequence for p in quoted.watermark.partitions)}
        log(trades=said)
        return said

    async def late(self):
        await self.resolve()
        rows = [self.trade("BTC-USD-T-9", self.at - DAY_NS, "61500.9"),
                self.trade("BTC-USD-T-1", self.at + 1000, "62000.1", cancelled=True)]
        done = await self.plugin._operations().RecordTrades(ops.RecordTradesParams(trades=rows))
        said = {"recorded": done.recorded, "restated": done.restated}
        log(late=said)
        return said

    async def resolve_as_of(self, query):
        one = lambda name: query.get(name, [""])[0]
        found = await self.plugin._operations().ResolveIdentifier(ops.ResolveIdentifierParams(
            identifiers=[ops.Identifier(scheme=one("scheme"), value=one("value"), source=one("source"))],
            as_of_ns=int(one("as_of") or 0)))
        return {"found": found.found, "instrument_id": found.instrument_id,
                "miss_reason": ops.MissReason.Name(found.miss_reason), "minted": found.minted}

    async def identity(self):
        symbol = ops.Identifier(scheme="symbol", value="ZZAMB", source="coinbase")
        figi = ops.Identifier(scheme="figi", value="BBG000ZZAMB1")
        first = await self.plugin._operations().ResolveIdentifier(
            ops.ResolveIdentifierParams(identifiers=[symbol]))
        second = await self.plugin._operations().ResolveIdentifier(
            ops.ResolveIdentifierParams(identifiers=[figi]))
        both = await self.plugin._operations().ResolveIdentifier(
            ops.ResolveIdentifierParams(identifiers=[symbol, figi]))
        said = {"symbol": first.instrument_id, "figi": second.instrument_id,
                "both": {"found": both.found, "miss_reason": ops.MissReason.Name(both.miss_reason),
                         "minted": both.minted}}
        log(identity=said)
        return said

    async def latest(self, query):
        """Contract v20 (W10.6): the last trade this instance recorded of
        each subject asked, for a restart's backfill."""
        refs = [ops.SubjectRef(entity_id=s) for s in query.get("subject", [])]
        answer = await self.plugin._operations().ListTrades(
            ops.ListTradesParams(subjects=refs, latest_recorded=True))
        return {"trades": [trade_json(t) for t in answer.trades],
                "watermark": [[p.partition, p.sequence] for p in answer.watermark.partitions]}

    async def licences(self):
        """Contract v20 (W10.9): its own datasets' licences."""
        answer = await self.plugin._operations().ListDatasets(ops.ListDatasetsParams())
        return {"datasets": [d.dataset for d in answer.datasets],
                "licences": [{"dataset": l.dataset, "kept": l.kept, "updated_by": l.updated_by}
                             for l in answer.licences]}

    async def answer(self, path, query):
        if path == "/latest":
            return await self.latest(query)
        if path == "/licences":
            return await self.licences()
        if path == "/trades":
            return await self.trades()
        if path == "/late":
            return await self.late()
        if path == "/identity":
            return await self.identity()
        if path == "/resolve-as-of":
            return await self.resolve_as_of(query)
        if path == "/resolve":
            return await self.resolve()
        if path == "/record":
            return await self.record()
        if path == "/restate":
            return await self.restate()
        if path == "/wants":
            return {"wants": self.wants, "withdrawn": self.withdrawn, "streaming": sorted(self.streams),
                    "streamed": self.streamed, "started": self.started}
        return {"page": "the lake's data plugin stand-in"}


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
        self.heard.append(price_json(heard.message.price))
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


# ── The desk: an ems plugin hearing trades and quotes (contract v19) ────


def _decimal(text):
    """A decimal as the wire carries it: a 128-bit integer, its upper half
    signed, and its scale."""
    exact = Decimal(text)
    scale = max(0, -exact.as_tuple().exponent)
    units = int(exact.scaleb(scale))
    return ops.Decimal(high=units >> 64, low=units & ((1 << 64) - 1), scale=scale)


def _amount(decimal):
    return str(as_decimal(decimal))


def trade_json(trade):
    meta = trade.meta
    return {"row_key": meta.row_key, "version": meta.version, "sequence": meta.sequence,
            "dataset": meta.source.dataset, "venue_id": meta.source.venue_id,
            "valid_from_ns": meta.valid_from_ns, "amount": _amount(trade.price.amount),
            "instrument_id": trade.price.instrument_id, "cancelled": trade.cancelled}


def quote_json(quote):
    meta = quote.meta
    side = quote.bid if quote.HasField("bid") else quote.ask
    return {"row_key": meta.row_key, "version": meta.version, "sequence": meta.sequence,
            "bid": _amount(quote.bid.amount) if quote.HasField("bid") else "",
            "instrument_id": side.instrument_id}


class Desk:
    def __init__(self, plugin):
        self.plugin = plugin
        self.trades = []
        self.quotes = []
        self.lost = 0
        self.following = None

    async def hear(self, subjects):
        call = self.plugin._operations().Receive(
            ops.ReceiveRequest(rows=["TradesRecorded", "QuotesRecorded"], subjects=subjects))
        async for delivery in call:
            which = delivery.WhichOneof("item")
            if which == "trades_recorded":
                self.trades.append(trade_json(delivery.trades_recorded.trade))
                log(heard_trade=self.trades[-1])
            elif which == "quotes_recorded":
                self.quotes.append(quote_json(delivery.quotes_recorded.quote))
                log(heard_quote=self.quotes[-1])
            elif which == "lost":
                self.lost += 1

    async def answer(self, path, query):
        subjects = query.get("subject", [])
        refs = [ops.SubjectRef(entity_id=s) for s in subjects]
        if path == "/follow":
            if self.following is None:
                self.following = asyncio.ensure_future(self.hear(subjects))
            return {"following": subjects}
        if path == "/heard":
            return {"trades": self.trades, "quotes": self.quotes, "lost": self.lost}
        if path == "/catch-up":
            after = int(query.get("after", ["0"])[0])
            dataset = query.get("dataset", ["coin:live"])[0]
            answer = await self.plugin._operations().ListTrades(ops.ListTradesParams(
                subjects=refs, after_watermark=ops.Watermark(
                    partitions=[ops.PartitionSequence(partition=dataset, sequence=after)])))
            return {"trades": [trade_json(t) for t in answer.trades],
                    "unanswered": [ops.UnansweredReason.Name(u.reason) for u in answer.unanswered]}
        if path == "/latest":
            # Not a data plugin's: refused (contract v20, W10.6).
            try:
                await self.plugin._operations().ListTrades(
                    ops.ListTradesParams(subjects=refs, latest_recorded=True))
                return {"refused": ""}
            except Exception as refused:  # the sidecar's answer, said back
                return {"refused": str(refused)[:300]}
        if path in ("/quotes", "/quotes-wanted"):
            answer = await self.plugin._operations().ListQuotes(ops.ListQuotesParams(subjects=refs))
            return {"quotes": [quote_json(q) for q in answer.quotes],
                    "unanswered": [ops.UnansweredReason.Name(u.reason) for u in answer.unanswered]}
        return {"page": "the lake's desk stand-in"}


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
    if KIND == "data":
        plugin = await meridian.connect(
            interface=Interface(PORT, "Lake stand-in", pages=[Page("/", "Datasets", levels=["admin"])]),
            declaration=Declaration(catalogue=CATALOGUE),
        )
        served = Dgm(plugin)
        asyncio.ensure_future(plugin.receive(observations_wanted=served.on_want,
                                             want_withdrawn=served.on_withdrawn))
    elif KIND == "desk":
        plugin = await meridian.connect(
            interface=Interface(PORT, "Desk stand-in", pages=[Page("/", "Trades", levels=["read"])]),
        )
        served = Desk(plugin)
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
