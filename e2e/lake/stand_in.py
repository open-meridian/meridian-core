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
  /quotes?subject=ID[&dataset=D][&quarantined=1]
                             ListQuotes, the latest in force, each with its
                             quality; with quarantined, include_quarantined
                             (contract v28), its refusal said;
  /quotes-wanted?subject=ID  ListQuotes of a subject nothing recorded.

From contract v27 (the rates slice) it declares `rates` -- rates, pulled
daily in New York -- and serves:

  /rates     resolves SOFR and Treasury's par curve, referential records
             the platform seeds (W1.1), through the SDK's rates helpers;
             records SOFR's fixing and the curve's points for one date, a
             tenor left unpublished making no row, then the fixing revised
             with a later published time (W10.4).

And from contract v20 (W10.6, W10.9, W3.1):

  /latest?subject=ID         ListTrades latest_recorded: its own last trade
                             per dataset and venue, for a restart's backfill;
  /licences                  ListDatasets: its own datasets' licences.

The wire is the SDK's own (SDK 0.24.0 carries v20's typed operations).

From contract v28 (quality states; W10.4, W10.6, W10.12; ruling 619):

  /quality   records four BTC quotes on `live` through the SDK's edge
             checks (meridian.roles.data.quality.EdgeChecks): one valid,
             the same again under a new key (a duplicate, suspicious), one
             after a sequence gap (suspicious) and, resynced, a crossed one
             (invalid), each recorded with its state.

From contract v28's filings slice (W10.4 to W10.6; core release C2) it
declares `filings` -- filings, reported facts, rating actions, holdings
reports and holdings, pushed -- and serves:

  /filings   resolves Apple, Berkshire Hathaway and Moody's by CIK
             (ResolveCompany, W3.32) and Apple's share by its FIGI, and
             records an 8-K with Item 2.02, a quarter's basic EPS and then
             a later filing restating it, a 13F report and its Apple
             holding, and Moody's affirmation of Apple's rating.

From contract v29 (track C; W10.4 to W10.7; rulings 680, 685, 900) it
declares `fixed_income` -- trades, quotes and factors, pushed -- and serves:

  /fixed-income
             resolves the harness platform's corporate bond by its FIGI and
             its agency pool by agency_pool, and records a TRACE-like print
             of the bond (5MM+, capped, at 98.375 per 100 of face, its yield
             to worst beside it, a dealer the contra and the reporting
             dealer principal), a when-issued print in yield alone, a firm
             quote with bucketed sizes, and two months of the pool's factors,
             then October's republished.

STAND_IN_LAKE=signal is a `signal` plugin serving:

  /assess?subject=ID&date=D&state=S[&version=N]
             reads the subject's close for the date from coin:daily and
             records its assessment (RecordQualityAssessments), its version
             the row's unless given; answering what was recorded or the
             refusal;
  /follow-filings?subject=ID  hears the filings recorded about it (W10.5);
  /filings-heard             each filing heard, its row key and items.

STAND_IN_LAKE=reader serves:

  /follow?subject=ID...                     hears what is recorded for them;
  /heard                                    what it heard, each row's dataset,
                                            row key and version;
  /prices?subject=ID&date=D[&kind=K][&dataset=D][&as_of=N][&latest=1]
         [&quarantined=1]
                                            one read, its rows (each with its
                                            quality) and what it left
                                            unanswered; with quarantined, the
                                            datasets named include_quarantined
                                            (contract v28), its refusal said;
  /datasets                                 the datasets it may read;
  /invalid-date?subject=ID                  a read naming 2026-02-30, sent past
                                            the SDK's own check, as the sidecar
                                            answers it;
  /rates?subject=ID&date=D[&tenor=T][&published_as_of=N]
                                            ListRates (contract v27): its rows and
                                            what it left unanswered;
  /factors?subject=ID[&from=D][&to=D][&after=N]
                                            contract v29: ListFactors by payment
                                            date, or after fixed_income's
                                            sequence N;
  /filed?subject=ID&role=R[&published_as_of=N]
                                            contract v28: ListFilings,
                                            ListReportedFacts, ListRatingActions
                                            and ListHoldings of the subject in
                                            its role, each row in brief.

The exchange file (STAND_IN_EXCHANGES, a directory holding exchange.json) is
shaped on a vendor's responses with invented values: candles as
[time, low, high, open, close, volume], text for every number, so nothing
passes through a float. `make e2e-lake` reads the synthetic one committed
beside this file; `make e2e-recorded` the directory RECORDED names, a
developer's recorded exchanges, never committed.
"""
import asyncio
import dataclasses
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
    Quote,
    SourceChoice,
    SourceTime,
)
from meridian.operations import as_decimal, as_money  # noqa: E402
from meridian.plugin.v1 import operations_pb2 as ops  # noqa: E402
from meridian.roles.data import rates as rate_helpers  # noqa: E402
from meridian.roles.data.quality import EdgeChecks, assessment  # noqa: E402

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
    # Contract v27, the rates slice: a reference rate and a curve.
    DatasetDeclaration(
        key="rates",
        vendor="Federal Reserve",
        data_types=["meridian.v1.Rate"],
        modes=["pull", "push"],
        cadence=86_400,
        licence_default=DatasetLicence(kept=True, derived_use=True, display=True),
        day_time_zone="America/New_York",
    ),
    # Contract v28, the filings slice: EDGAR-like rows, pushed.
    DatasetDeclaration(
        key="filings",
        vendor="SEC EDGAR",
        data_types=["meridian.v1.Filing", "meridian.v1.ReportedFact",
                    "meridian.v1.RatingAction", "meridian.v1.HoldingsReport",
                    "meridian.v1.Holding"],
        modes=["push"],
        licence_default=DatasetLicence(kept=True, derived_use=True, display=True),
        day_time_zone="America/New_York",
    ),
    # Contract v29, track C: a fixed-income source's prints, quotes and an
    # agency's factors, pushed; named after the SDK's check, as the 1b's are.
    _v19_types(DatasetDeclaration(
        key="fixed_income",
        vendor="FINRA",
        data_types=["meridian.v1.Price"],
        modes=["push"],
        licence_default=DatasetLicence(kept=True, derived_use=True, display=True),
        day_time_zone="America/New_York",
    ), ["meridian.v1.Trade", "meridian.v1.Quote", "meridian.v1.Factor"]),
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
        # Each with what the venue states of it: from contract v24 a source
        # stating less than an identifier and a description (a crypto asset
        # is its own unit, so no currency) is no record (W3.7).
        asked = {
            # Each by its CAIP-19 ID beside the venue's symbol, so the
            # platform's master answers it: a provisional local record is
            # not priced (W10.6).
            "btc": ([ops.Identifier(scheme="symbol", value="BTC", source="coinbase"),
                     ops.Identifier(scheme="caip19",
                                    value="bip122:000000000019d6689c085ae165831e93/slip44:0")],
                    "crypto_asset", "Bitcoin"),
            "usdc": ([ops.Identifier(scheme="symbol", value="USDC", source="coinbase"),
                      ops.Identifier(scheme="caip19",
                                     value="eip155:1/erc20:0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")],
                     "crypto_asset", "USD Coin"),
            "eur": ([ops.Identifier(scheme="iso4217", value="EUR")], None, ""),
        }
        for name, (identifiers, stated, described) in asked.items():
            if name in self.ids:
                continue
            found = await self.plugin.resolve_identifier(
                identifiers=identifiers, stated_asset_class=stated, stated_description=described)
            if not found.instrument_id:
                raise RuntimeError(f"{name} did not resolve: {found}")
            self.ids[name] = found.instrument_id
        return dict(self.ids)

    def meta(self, row_key, subject, dataset, seconds, venue=""):
        # A subject by its name here, or (contract v27) by the ID a want named.
        entity = self.ids.get(subject, subject)
        return ObservationMeta(
            row_key=row_key,
            # SDK 0.33.0 (contract v28): the SDK's own SubjectRef, its role
            # an enum it converts.
            subjects=[meridian.SubjectRef(entity_id=entity)],
            source=ops.Source(dataset=self.dataset(dataset), venue_id=venue),
            valid_from_ns=seconds * 10**9,
            valid_until_ns=seconds * 10**9 + DAY_NS,
            business_date=day_of(seconds),
            source_times=[SourceTime(kind="published", at_ns=seconds * 10**9 + DAY_NS)],
            raw=self.plugin.raw_record(f"candles/{row_key}"),
        )

    def close(self, candle, subject="btc"):
        seconds, _low, _high, _open, close, _volume = candle
        key = "BTC-USD" if subject == "btc" else subject
        return Price(
            meta=self.meta(f"{key}:1d:{seconds}", subject, "daily", seconds, VENUE),
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
        # Each subject the want names: BTC by its own name, and from contract
        # v27 a provisional (LCL-) record by the ID the want names, as a data
        # plugin answers by the identifiers the record carries (ruling 484).
        btc = self.ids["btc"]
        subjects = ["btc" if s.entity_id == btc else s.entity_id for s in want.subjects]
        for candle in exchange()["candles"]["BTC-USD"]:
            if day_of(candle[0]) == wanted:
                done = await self.plugin.record_prices(
                    prices=[self.close(candle, subject) for subject in subjects],
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

    async def quality(self):
        """Contract v28 (W10.4; ruling 424): BTC's quotes on `live` judged by
        the SDK's edge checks, as a data plugin's `Wants.record` judges them:
        valid; the same again under a new key, a duplicate; after a gap in
        the stream's sequence; and, resynced, crossed."""
        await self.resolve()
        edge = EdgeChecks()
        now = time.time_ns()

        def quote(key, at, bid, ask):
            return Quote(
                meta=ObservationMeta(
                    row_key=key,
                    subjects=[meridian.SubjectRef(entity_id=self.ids["btc"])],
                    source=ops.Source(dataset=self.dataset("live"), venue_id=VENUE),
                    valid_from_ns=at),
                bid=Money(Decimal(bid), "USD"), ask=Money(Decimal(ask), "USD"))

        live = "live"
        edge.sequenced(live, 1)
        rows = edge.judged(live, [quote("BTC-USD-Q-21", now + 1000, "62010", "62012")])
        rows += edge.judged(live, [quote("BTC-USD-Q-22", now + 2000, "62010", "62012")])
        edge.sequenced(live, 2)
        edge.sequenced(live, 5)
        rows += edge.judged(live, [quote("BTC-USD-Q-23", now + 3000, "62011", "62013")])
        edge.resynced(live)
        rows += edge.judged(live, [quote("BTC-USD-Q-24", now + 4000, "62020", "62015")])
        done = await self.plugin.record_quotes(quotes=rows)
        said = {"recorded": done.recorded,
                "states": {r.meta.row_key: [str(r.meta.quality or "valid"),
                                            list(r.meta.quality_reasons)] for r in rows}}
        log(quality=said)
        return said

    async def rates(self):
        """Contract v27 (W10.4): SOFR's fixing and Treasury's par curve for
        one date, through the SDK's rates helpers; then the fixing revised,
        published later, as the next version of its row key."""
        feed = rate_helpers.Feed(self.plugin, "lake", day_zone="America/New_York")
        # By the administrator's own code, the administrator its source
        # (ruling 545).
        sofr = await feed.subject(("symbol", "SOFR", "nyfed"),
                                  description="Secured Overnight Financing Rate",
                                  instrument_type="interest_rate")
        par = await feed.subject(("symbol", "Daily Treasury Par Yield Curve Rates", "ustreasury"),
                                 description="Daily Treasury par yield curve",
                                 instrument_type="rate_curve")
        if not sofr or not par:
            raise RuntimeError(f"a referential record did not resolve: {sofr!r} {par!r}")
        now = time.time_ns()
        first, revised = now - 3600 * 10**9, now - 60 * 10**9
        day = "2026-10-07"
        fixing = lambda value, published: feed.rate(  # noqa: E731
            sofr, "rates", "fixing", value, business_date=day, published_ns=published,
            convention=rate_helpers.MONEY_MARKET, raw_key=f"sofr/{day}")
        curve = feed.curve(par, "rates", "par_yield",
                           {"3M": "4.05", "2Y": "3.60", "10Y": "4.12", "20Y": None},
                           business_date=day, published_ns=first,
                           convention=rate_helpers.BOND_EQUIVALENT, raw_key=f"par/{day}")
        done = await self.plugin.record_rates(rates=[fixing("4.31", first)] + curve)
        again = await self.plugin.record_rates(rates=[fixing("4.32", revised)])
        said = {"sofr": sofr, "par": par, "recorded": done.recorded,
                "restated": again.restated, "first_published": first,
                "revised_published": revised}
        log(rates=said)
        return said

    async def filings(self):
        """Contract v28, the filings slice (W10.4): each company by its CIK,
        Apple's share by its FIGI; an 8-K dating an earnings report, a fact
        and its restatement by a later filing published after it, a 13F and
        its holding, and a rating action."""
        companies = {}
        for name, cik in (("apple", "0000320193"), ("berkshire", "0001067983"),
                          ("moodys", "0001059556")):
            found = await self.plugin.resolve_company(
                identifiers=[ops.Identifier(scheme="cik", value=cik)])
            companies[name] = found.company_id
        share = await self.plugin.resolve_identifier(
            identifiers=[ops.Identifier(scheme="figi", value="BBG000B9XRY4")],
            stated_asset_class="equity", stated_description="Apple Inc.", stated_currency="USD")
        if not share.instrument_id:
            raise RuntimeError(f"Apple's share did not resolve: {share}")
        apple, berkshire, moodys = companies["apple"], companies["berkshire"], companies["moodys"]
        now = time.time_ns()
        first, restated = now - 3600 * 10**9, now - 60 * 10**9

        def meta(row_key, subjects, valid, published):
            return ObservationMeta(
                row_key=row_key,
                subjects=[meridian.SubjectRef(entity_id=e, role=r) for e, r in subjects],
                source=ops.Source(dataset=self.dataset("filings")),
                valid_from_ns=valid,
                source_times=[SourceTime(kind="published", at_ns=published)],
                raw=self.plugin.raw_record(f"edgar/{row_key}"))

        eight_k = "0000320193-26-000105"
        filed = await self.plugin.record_filings(filings=[meridian.Filing(
            meta=meta(eight_k, [(apple, "filer")], first, first),
            filing=ops.FilingRef(accession_number=eight_k, form="8-K"),
            kind="current_report", items=["2.02", "9.01"], filed_date="2026-10-30")])
        eps_key = "320193:us-gaap:EarningsPerShareBasic:USD/shares:2026-06-28:2026-09-27"

        def eps(amount, accession, published):
            return meridian.ReportedFact(
                meta=meta(eps_key, [(apple, "filer")], published, published),
                concept="us-gaap:EarningsPerShareBasic", amount=Money(Decimal(amount), "USD"),
                period_start="2026-06-28", period_end="2026-09-27",
                filing=ops.FilingRef(accession_number=accession, form="10-K"))

        fact = await self.plugin.record_reported_facts(
            facts=[eps("1.85", "0000320193-26-000107", first)])
        fact_again = await self.plugin.record_reported_facts(
            facts=[eps("1.86", "0000320193-26-000121", restated)])
        q3 = "0001067983-26-000045"
        quarter_end = int(datetime(2026, 9, 30, tzinfo=timezone.utc).timestamp()) * 10**9
        reported = await self.plugin.record_holdings_reports(reports=[meridian.HoldingsReport(
            meta=meta(q3, [(berkshire, "holder")], quarter_end, first),
            kind="form_13f", report_date="2026-09-30", complete=True,
            filing=ops.FilingRef(accession_number=q3, form="13F-HR"))])
        held = await self.plugin.record_holdings(holdings=[meridian.Holding(
            meta=meta(f"{q3}:037833100", [(berkshire, "holder"), (share.instrument_id, "held")],
                      quarter_end, first),
            report_row_key=q3,
            quantity=meridian.UnitValue(value=Decimal("300000000"), unit="shares"),
            market_value=Money(Decimal("69000000000"), "USD"))])
        affirmed = int(datetime(2026, 5, 2, tzinfo=timezone.utc).timestamp()) * 10**9
        rated = await self.plugin.record_rating_actions(actions=[meridian.RatingAction(
            meta=meta(f"moodys:issuer:{apple}:2026-05-02", [(apple, "subject"), (moodys, "rater")],
                      affirmed, first),
            rating="Aaa", rating_class="long-term issuer", action="affirmed")])
        said = {"apple": apple, "berkshire": berkshire, "moodys": moodys,
                "share": share.instrument_id, "first_published": first,
                "restated_published": restated,
                "recorded": [filed.recorded, fact.recorded, reported.recorded, held.recorded,
                             rated.recorded],
                "restated": fact_again.restated}
        log(filings=said)
        return said

    async def fixed_income(self):
        """Contract v29, track C (W10.4; rulings 680, 685, 900): the
        harness platform's bond and pool, a capped print in percent of par
        with its yield, a when-issued print in yield, a firm quote with
        bucketed sizes, and the pool's factors, October's republished."""
        bond = await self.plugin.resolve_identifier(
            identifiers=[ops.Identifier(scheme="figi", value="BBG01BOND0001")],
            stated_asset_class="debt", stated_description="Harness Corp 4.875% 2036-02-05",
            stated_currency="USD")
        pool = await self.plugin.resolve_identifier(
            identifiers=[ops.Identifier(scheme="agency_pool", value="MA5123", source="FNMA")],
            stated_asset_class="debt", stated_description="FNMA CL MA5123 5.5",
            stated_currency="USD")
        if not bond.instrument_id or not pool.instrument_id:
            raise RuntimeError(f"the bond or the pool did not resolve: {bond} {pool}")
        now = time.time_ns()
        dataset = self.dataset("fixed_income")

        def meta(row_key, subject, at, venue="", business_date=""):
            return ops.ObservationMeta(row_key=row_key,
                                       subjects=[ops.SubjectRef(entity_id=subject)],
                                       source=ops.Source(dataset=dataset, venue_id=venue),
                                       valid_from_ns=at, business_date=business_date)

        def quoted(percent, kind, to_date=""):
            return ops.QuotedYield(
                value=ops.UnitValue(value=_decimal(percent), unit=ops.VALUE_UNIT_PERCENT),
                convention=ops.RateConvention(day_count=ops.DAY_COUNT_THIRTY_360_BOND_BASIS,
                                              compounding=ops.COMPOUNDING_COMPOUNDED,
                                              periods_per_year=2),
                kind=kind, to_date=to_date)

        prints = [
            ops.Trade(meta=meta("TRACE-HARN36-1", bond.instrument_id, now),
                      price=self.money("98.375"), quantity=_decimal("5000000"),
                      quantity_capped=True, basis=ops.PRICE_BASIS_PERCENT_OF_PAR,
                      attributes=ops.TradeAttributes(characteristics=[
                          ops.TRADE_CHARACTERISTIC_CONTRA_DEALER,
                          ops.TRADE_CHARACTERISTIC_PRINCIPAL_CAPACITY]),
                      **{"yield": quoted("5.093", ops.YIELD_KIND_TO_WORST, "2034-02-05")}),
            ops.Trade(meta=meta("TRACE-HARN36-WI", bond.instrument_id, now + 1000),
                      quantity=_decimal("250000"), basis=ops.PRICE_BASIS_YIELD,
                      attributes=ops.TradeAttributes(characteristics=[
                          ops.TRADE_CHARACTERISTIC_WHEN_ISSUED]),
                      **{"yield": quoted("4.875", ops.YIELD_KIND_TO_MATURITY)}),
        ]
        traded = await self.plugin._operations().RecordTrades(ops.RecordTradesParams(trades=prints))
        quote = ops.Quote(meta=meta("ATS-HARN36-Q-1", bond.instrument_id, now + 2000, VENUE),
                          bid=self.money("98.25"), ask=self.money("98.50"),
                          bid_quantity=_decimal("1000000"), ask_quantity=_decimal("1000000"),
                          bid_size_capped=True, ask_size_capped=True,
                          basis=ops.PRICE_BASIS_PERCENT_OF_PAR,
                          bid_yield=quoted("5.110", ops.YIELD_KIND_TO_WORST, "2034-02-05"),
                          ask_yield=quoted("5.076", ops.YIELD_KIND_TO_WORST, "2034-02-05"))
        quoted_done = await self.plugin._operations().RecordQuotes(ops.RecordQuotesParams(quotes=[quote]))

        def factor(month, value, record, payment, published):
            return ops.Factor(meta=meta(f"FNMA:MA5123:{month}", pool.instrument_id, now,
                                        business_date=published),
                              factor=_decimal(value), factor_month=month,
                              record_date=record, payment_date=payment)

        factors = await self.plugin._operations().RecordFactors(ops.RecordFactorsParams(factors=[
            factor("202609", "0.84017733", "2026-09-30", "2026-10-26", "2026-10-06"),
            factor("202610", "0.83102210", "2026-10-31", "2026-11-25", "2026-11-04")]))
        republished = await self.plugin._operations().RecordFactors(ops.RecordFactorsParams(
            factors=[factor("202610", "0.83102211", "2026-10-31", "2026-11-25", "2026-11-04")]))
        said = {"bond": bond.instrument_id, "pool": pool.instrument_id,
                "trades": traded.recorded, "quotes": quoted_done.recorded,
                "factors": factors.recorded, "republished": republished.restated,
                "head": max(p.sequence for p in republished.watermark.partitions)}
        log(fixed_income=said)
        return said

    async def resolve_as_of(self, query):
        one = lambda name: query.get(name, [""])[0]
        # A security with the name and currency the venue states (from
        # contract v24 a source stating less is no record, W3.7); a
        # currency's own code states both.
        stated = ({} if one("scheme") == "iso4217" else
                  {"stated_description": f"{one('value')}, as the venue names it",
                   "stated_currency": "USD"})
        found = await self.plugin._operations().ResolveIdentifier(ops.ResolveIdentifierParams(
            identifiers=[ops.Identifier(scheme=one("scheme"), value=one("value"), source=one("source"))],
            as_of_ns=int(one("as_of") or 0), **stated))
        return {"found": found.found, "instrument_id": found.instrument_id,
                "miss_reason": ops.MissReason.Name(found.miss_reason), "minted": found.minted}

    async def identity(self):
        symbol = ops.Identifier(scheme="symbol", value="ZZAMB", source="coinbase")
        figi = ops.Identifier(scheme="figi", value="BBG000ZZAMB1")
        stated = {"stated_description": "ZZAMB, as the venue names it", "stated_currency": "USD"}
        first = await self.plugin._operations().ResolveIdentifier(
            ops.ResolveIdentifierParams(identifiers=[symbol], **stated))
        second = await self.plugin._operations().ResolveIdentifier(
            ops.ResolveIdentifierParams(identifiers=[figi], **stated))
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
        if path == "/rates":
            return await self.rates()
        if path == "/quality":
            return await self.quality()
        if path == "/filings":
            return await self.filings()
        if path == "/fixed-income":
            return await self.fixed_income()
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
        # Contract v28: the row's effective state and its reasons.
        "quality": ops.QualityState.Name(meta.quality),
        "quality_reasons": [ops.QualityReason.Name(r) for r in meta.quality_reasons],
    }


def refusal_of(failed):
    """A refusal as gRPC carries it, in words."""
    details = getattr(failed, "details", None)
    return details() if callable(details) else str(failed)


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
        subjects = [meridian.SubjectRef(entity_id=s) for s in query.get("subject", [])]
        named = query.get("dataset", [])
        quarantined = one("quarantined") == "1"
        sources = (ops.SourceChoice(named=named, include_quarantined=quarantined)
                   if named or quarantined else None)
        asked = {"subjects": subjects, "sources": sources}
        if one("kind"):
            asked["kinds"] = [one("kind")]
        if one("date"):
            asked["business_date"] = one("date")
        if one("as_of"):
            asked["as_of_ns"] = int(one("as_of"))
        try:
            answer = await self.plugin.list_prices(**asked)
        except Exception as failed:  # the lake's refusal, said back
            return {"refused": refusal_of(failed), "prices": [], "unanswered": []}
        return {
            "prices": [price_json(price) for price in answer.prices],
            "unanswered": [{"subject": u.subject.entity_id, "dataset": u.dataset,
                            "reason": ops.UnansweredReason.Name(u.reason)}
                           for u in answer.unanswered],
        }

    async def rates(self, query):
        """ListRates (contract v27, W10.6)."""
        one = lambda name, default="": query.get(name, [default])[0]  # noqa: E731
        answer = await self.plugin.list_rates(
            subjects=[meridian.SubjectRef(entity_id=s) for s in query.get("subject", [])],
            business_date=one("date"),
            tenors=[rate_helpers.tenor(t) for t in query.get("tenor", [])],
            published_as_of_ns=int(one("published_as_of", "0")),
        )
        return {
            "rates": [{"row_key": r.meta.row_key, "version": r.meta.version,
                       "dataset": r.meta.source.dataset, "kind": ops.RateKind.Name(r.kind),
                       "value": _amount(r.value.value),
                       "tenor": rate_helpers.tenor_label(r.tenor) if r.HasField("tenor") else ""}
                      for r in answer.rates],
            "unanswered": [{"subject": u.subject.entity_id, "dataset": u.dataset,
                            "reason": ops.UnansweredReason.Name(u.reason)}
                           for u in answer.unanswered],
        }

    async def filed(self, query):
        """Contract v28 (W10.6): the filings slice's four reads of one
        subject in its role, each row in brief."""
        one = lambda name, default="": query.get(name, [default])[0]  # noqa: E731
        subjects = [meridian.SubjectRef(entity_id=one("subject"), role=one("role") or None)]
        published = int(one("published_as_of", "0"))
        brief = lambda meta: {"row_key": meta.row_key, "version": meta.version,  # noqa: E731
                              "dataset": meta.source.dataset}
        filings = await self.plugin.list_filings(subjects=subjects, published_as_of_ns=published)
        facts = await self.plugin.list_reported_facts(subjects=subjects,
                                                      published_as_of_ns=published)
        ratings = await self.plugin.list_rating_actions(subjects=subjects,
                                                        published_as_of_ns=published)
        holdings = await self.plugin.list_holdings(subjects=subjects, published_as_of_ns=published,
                                                   kinds=["form_13f"], from_date="2026-09-30")
        return {
            "filings": [dict(brief(f.meta), items=list(f.items), filed_date=f.filed_date,
                             kind=ops.FilingKind.Name(f.kind)) for f in filings.filings],
            "facts": [dict(brief(f.meta), amount=_amount(f.amount.amount),
                           filing=f.filing.accession_number) for f in facts.facts],
            "ratings": [dict(brief(r.meta), rating=r.rating,
                             action=ops.RatingActionKind.Name(r.action)) for r in ratings.actions],
            "holdings": [dict(brief(h.meta), report=h.report_row_key,
                              quantity=_amount(h.quantity.value)) for h in holdings.holdings],
            "reports": [dict(brief(r.meta), report_date=r.report_date, complete=r.complete)
                        for r in holdings.reports],
        }

    async def factors(self, query):
        """ListFactors (contract v29, W10.6): by payment date, or caught up
        after fixed_income's sequence `after`."""
        one = lambda name, default="": query.get(name, [default])[0]  # noqa: E731
        after = one("after")
        watermark = (ops.Watermark(partitions=[ops.PartitionSequence(
            partition="coin:fixed_income", sequence=int(after))]) if after else None)
        answer = await self.plugin.list_factors(
            subjects=[meridian.SubjectRef(entity_id=s) for s in query.get("subject", [])],
            from_date=one("from"), to_date=one("to"), after_watermark=watermark)
        return {
            "factors": [{"month": f.factor_month, "factor": _amount(f.factor),
                         "payment_date": f.payment_date, "version": f.meta.version,
                         "dataset": f.meta.source.dataset} for f in answer.factors],
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
        if path == "/rates":
            return await self.rates(query)
        if path == "/filed":
            return await self.filed(query)
        if path == "/factors":
            return await self.factors(query)
        if path == "/curve":
            return await self.curve(query)
        return {"page": "the lake's reading stand-in"}

    async def curve(self, query):
        """Contract v29 (W12.1, W12.3; ruling 688): the Treasury curve at a
        business date, which the analytics component fits monotone convex
        over the par yields the lake holds, read on this plugin's behalf:
        each figure by its curve and tenor, and each line not evaluated."""
        one = lambda name, default="": query.get(name, [default])[0]  # noqa: E731
        ran = await self.plugin._operations().RunJob(ops.RunJobParams(job=ops.Job(
            business_date=one("date"),
            measures=[ops.MeasureRef(definition="treasury_curve", version=1)])))
        answer = ran.answer
        return {
            "figures": [{"curve": {b.breakdown: b.value for b in f.breakdown}.get("curve", ""),
                         "tenor": {b.breakdown: b.value for b in f.breakdown}.get("tenor", ""),
                         "value": _amount(f.value)} for f in answer.figures],
            "not_evaluated": [{"reason": ops.NotEvaluatedReason.Name(u.reason), "detail": u.detail}
                              for u in answer.not_evaluated],
            "datasets": list(answer.provenance.datasets),
            "models": sorted(m.model_id for m in answer.provenance.models),
        }


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
            "valid_from_ns": meta.valid_from_ns,
            "amount": _amount(trade.price.amount) if trade.HasField("price") else "",
            "instrument_id": trade.price.instrument_id, "cancelled": trade.cancelled,
            # Contract v29 (ruling 685): the basis, the yield beside it, a
            # capped size and the characteristics.
            "basis": ops.PriceBasis.Name(trade.basis),
            "yield": (_amount(getattr(trade, "yield").value.value)
                      if trade.HasField("yield") else ""),
            "quantity_capped": trade.quantity_capped,
            "characteristics": [ops.TradeCharacteristic.Name(c)
                                for c in trade.attributes.characteristics]}


def quote_json(quote):
    meta = quote.meta
    side = quote.bid if quote.HasField("bid") else quote.ask
    return {"row_key": meta.row_key, "version": meta.version, "sequence": meta.sequence,
            "bid": _amount(quote.bid.amount) if quote.HasField("bid") else "",
            "instrument_id": side.instrument_id,
            # Contract v28: the row's state and its reasons.
            "quality": ops.QualityState.Name(meta.quality),
            "quality_reasons": [ops.QualityReason.Name(r) for r in meta.quality_reasons],
            # Contract v29 (rulings 685 and 900): its venue, bucketed sizes
            # and the bid's yield.
            "venue_id": meta.source.venue_id,
            "bid_size_capped": quote.bid_size_capped, "ask_size_capped": quote.ask_size_capped,
            "basis": ops.PriceBasis.Name(quote.basis),
            "bid_yield": _amount(quote.bid_yield.value.value) if quote.HasField("bid_yield") else ""}


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
            named = query.get("dataset", [])
            quarantined = query.get("quarantined", [""])[0] == "1"
            params = ops.ListQuotesParams(subjects=refs)
            if named or quarantined:
                params.sources.CopyFrom(ops.SourceChoice(named=named,
                                                         include_quarantined=quarantined))
            try:
                answer = await self.plugin._operations().ListQuotes(params)
            except Exception as failed:  # the lake's refusal, said back
                return {"refused": refusal_of(failed), "quotes": [], "unanswered": []}
            return {"quotes": [quote_json(q) for q in answer.quotes],
                    "unanswered": [ops.UnansweredReason.Name(u.reason) for u in answer.unanswered]}
        return {"page": "the lake's desk stand-in"}


# ── The lens: a signal plugin escalating a close's quality (contract v28) ──


class Lens:
    def __init__(self, plugin):
        self.plugin = plugin
        self.filings = []
        self.hearing = None

    async def on_filing(self, heard):
        filing = heard.message.filing
        self.filings.append({"row_key": filing.meta.row_key, "version": filing.meta.version,
                             "items": list(filing.items)})
        log(heard_filing=self.filings[-1])

    async def assess(self, query):
        """W10.12: read the close, then record its assessment, as a signal
        plugin does once its baseline finds it an outlier."""
        one = lambda name, default="": query.get(name, [default])[0]  # noqa: E731
        read = await self.plugin.list_prices(
            subjects=[meridian.SubjectRef(entity_id=one("subject"))], kinds=["close"],
            business_date=one("date"),
            sources=ops.SourceChoice(named=["coin:daily"], include_quarantined=True))
        if not read.prices:
            return {"refused": f"no close read: {read.unanswered}"}
        found = assessment(read.prices[0], one("state"), ("price_outlier",))
        if one("version"):
            found = dataclasses.replace(found, version=int(one("version")))
        try:
            done = await self.plugin.record_quality_assessments(assessments=[found])
        except Exception as failed:  # the lake's refusal, said back
            return {"refused": refusal_of(failed)}
        said = {"recorded": done.recorded, "row_key": found.row_key, "version": found.version}
        log(assessed=said)
        return said

    async def answer(self, path, query):
        if path == "/assess":
            return await self.assess(query)
        if path == "/follow-filings":
            # Contract v28 (W10.5): every filing, never conflated.
            if self.hearing is None:
                self.hearing = asyncio.ensure_future(self.plugin.receive(
                    filings_recorded=self.on_filing, subjects=query.get("subject", [])))
            return {"following": query.get("subject", [])}
        if path == "/filings-heard":
            return {"filings": self.filings}
        return {"page": "the lake's signal stand-in"}


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
    elif KIND == "signal":
        plugin = await meridian.connect(
            interface=Interface(PORT, "Signal stand-in", pages=[Page("/", "Quality", levels=["read"])]),
        )
        served = Lens(plugin)
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
