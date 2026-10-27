"""A stand-in for the platform's master, for the harness (contract v20, W3.1;
the 2026-10-11 ruling: a currency the master holds is pulled, never minted).

It holds a handful of currencies, each by its ISO 4217 code, and answers a
resolve by that code with its record; anything else, that it holds nothing.
`HARNESS_PLATFORM_SLOW` names codes it answers late, `JPY=4` four seconds
after it is asked, so a test can show a slow master mints nothing.
Standard library only; it reads no assertion, as the harness's deployment is
registered nowhere.

From contract v22 (W3.16; spec/settlement-calendars-are-core-reference-data,
Q15) it serves seed settlement calendars and a rule on
GET /api/v1/reference/calendars, by the one subject a pull names -- a
calendar's ID, a currency's cash instrument, a venue, or a market with its
rule -- and on a refresh only versions after `after_version`: the US dollar's
(the Federal Reserve's holidays), the US equity venues' trading calendar
(NYSE's), the US securities settlement calendar, and the US equities and
funds settlement-date rule, T+2 from 5 September 2017 and T+1 from 28 May
2024. Seed data for tests, covering 2026 and 2027, each day citing its
public primary source; nothing here is a deployment's claim about any
market.

From contract v22 it also serves the two US equity venues its trading
calendar names, XNYS and XNAS, each in the shape of the platform's venue
master record (W1.16, W3.14: its MIC under `iso10383` from its creation
date, the vendor's code beside it, country US, New York's zone), pulled by
its MIC (W3.15); and a test equity, answered by its FIGI with its listing
venue, XNAS (Apple Inc. lists on Nasdaq), and that venue (W3.3), so a
deployment's record minted for it and asked of the platform names a listing
venue in the US, and a settlement date worked out for it is T+1 on the US
securities settlement calendar.

From contract v23 it also serves the euro's TARGET calendar (the ECB's
TARGET closing days) and a EUR/USD spot-date rule, as the platform keeps
them: lag 2, counted on TARGET alone, as the market counts a USD pair --
the US dollar's holidays do not count against the lag -- the spot date good
on TARGET and the US dollar's calendar, cut 17:00 New York, modified
following with end of month. A pull names the pair by its two cash
instruments, either order. So EUR/USD traded Friday 9 October 2026 spots on
Tuesday 13 October, Columbus Day on the Monday counted on EUR alone.

From contract v24 it serves the one-ID routes (W3.4, W3.24, W3.25, W3.27,
W3.28): the matching and minting request, answered by what it holds -- the
test equity and the currencies a match, a FIGI it can confirm a mint, and
anything else its staff's queue; the template catalogue; the licence list;
the replacement feed; and the usage statistics. See V24 below.

From contract v25, where `HARNESS_REGISTRY` names a directory (e2e-plugin-
area's, e2e/plugin-area/registry, which its make.py wrote and signed), it
serves the registries' rows (W8.10, W5.19) from it: the catalogue, each
version's resolution, and the plugin usage reports, heard; and, over TLS on
port 8443 with the certificate in `HARNESS_REGISTRY_TLS`, the OCI registry
the resolutions name, read-only, by digest, with the referrers API and,
from v26, the referrers tag schema, the API answering 404 for an image
whose attestations are Sigstore bundles, as ghcr.io does. A version it
lists with an altered layer is served altered, as a tampered registry
would.

From contract v26 (W1.30, W3.29; the v26 plan's item 7, rulings 199 and
204) it mints binary event contracts by venue symbol: a matching request for
an event contract naming the venue's symbol as `ticker`, its `source` the
venue's MIC (Kalshi's KLSH) or, for a venue with none, its `VEN-` ID
(Polymarket's), is answered with a record minted for it, as the binary event
contract template matches (W3.4), paying one unit of the currency the
request offers. It holds the two prediction venues in its venue master, and
answers POST /api/v1/reference/escalate-event-group (W1.30): a venue's
grouping of contracts it minted, matched by the venue and its event's
reference, minted `EVG-` on first asking and a new version where the flags
or members changed; a member it does not hold queued for its staff. And the
template catalogue carries v26's versions with their calculation packages
(W1.24): cash, equity, and the binary event contract with its bounds.
See V26 below.

From contract v28 (W1.32; ruling 612) it answers POST
/api/v1/reference/match-company: a company its source confirms by CIK (or
the LEI it holds for one), minted `ENT-` on first asking and matched after,
any other queued for its staff; its licence list carries `cik`.

From contract v28 (W3.33; rulings 757, 775 and 778) it answers POST
/api/v1/reference/instrument-candidates, the instrument picker's search: the
test equity it holds (Apple Inc., by its `INS-` ID) for a query naming it,
then two funds it does not hold -- SPDR S&P 500 ETF Trust (SPY) on its US
composite with no listing venue (778), its ticker on the venue "US
composite", one with no MIC (779, 782), and Invesco QQQ (QQQ) with its Nasdaq
listing, XNAS (775) -- each with the identifiers an escalation sends; a
query it cannot place answers nothing, and an ISIN or a CUSIP is kept
nowhere. A matching request naming either fund's FIGIs is answered with a
record minted for it once, matched after. See CANDIDATES below."""
import datetime
import http.server
import json
import os
import ssl
import threading
import time
import urllib.parse

NAMES = {
    "USD": "US dollar", "EUR": "Euro", "GBP": "Pound sterling", "CHF": "Swiss franc",
    "JPY": "Japanese yen", "CAD": "Canadian dollar", "SEK": "Swedish krona",
}
SLOW = {code.strip().upper(): float(seconds)
        for code, _, seconds in (one.partition("=") for one in
                                 os.environ.get("HARNESS_PLATFORM_SLOW", "").split(",") if one)}


def record(code):
    return {
        "instrument_id": f"INS-01J{code}{'0' * 19}1",
        "identifiers": [{"scheme": "iso4217", "value": code, "source": ""}],
        "asset_class": "ASSET_CLASS_CASH",
        "currency": code,
        "description": NAMES[code],
        "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
        "version": 1,
    }


# Contract v22: the seed calendars and rule (see the module's note).
READ_AT = 1760140800000000000
FED = {"source_id": "fed-holidays", "name": "Federal Reserve holidays",
       "address": "https://www.federalreserve.gov/aboutthefed/k8.htm", "read_at_ns": READ_AT}
NYSE = {"source_id": "nyse-holidays", "name": "NYSE holidays and trading hours",
        "address": "https://www.nyse.com/markets/hours-calendars", "read_at_ns": READ_AT}
FED_CLOSED = [
    ("2026-01-01", "New Year's Day"), ("2026-01-19", "Martin Luther King Jr. Day"),
    ("2026-02-16", "Washington's Birthday"), ("2026-05-25", "Memorial Day"),
    ("2026-06-19", "Juneteenth"), ("2026-09-07", "Labor Day"),
    ("2026-10-12", "Columbus Day"), ("2026-11-11", "Veterans Day"),
    ("2026-11-26", "Thanksgiving Day"), ("2026-12-25", "Christmas Day"),
    ("2027-01-01", "New Year's Day"), ("2027-01-18", "Martin Luther King Jr. Day"),
    ("2027-02-15", "Washington's Birthday"), ("2027-05-31", "Memorial Day"),
    ("2027-07-05", "Independence Day"), ("2027-09-06", "Labor Day"),
    ("2027-10-11", "Columbus Day"), ("2027-11-11", "Veterans Day"),
    ("2027-11-25", "Thanksgiving Day"),
]
NYSE_CLOSED = [
    ("2026-01-01", "New Year's Day"), ("2026-01-19", "Martin Luther King Jr. Day"),
    ("2026-02-16", "Washington's Birthday"), ("2026-04-03", "Good Friday"),
    ("2026-05-25", "Memorial Day"), ("2026-06-19", "Juneteenth"),
    ("2026-07-03", "Independence Day"), ("2026-09-07", "Labor Day"),
    ("2026-11-26", "Thanksgiving Day"), ("2026-12-25", "Christmas Day"),
    ("2027-01-01", "New Year's Day"), ("2027-01-18", "Martin Luther King Jr. Day"),
    ("2027-02-15", "Washington's Birthday"), ("2027-03-26", "Good Friday"),
    ("2027-05-31", "Memorial Day"), ("2027-06-18", "Juneteenth"),
    ("2027-07-05", "Independence Day"), ("2027-09-06", "Labor Day"),
    ("2027-11-25", "Thanksgiving Day"), ("2027-12-24", "Christmas Day"),
]
ECB = {"source_id": "ecb-target", "name": "ECB: TARGET closing days",
       "address": "https://www.ecb.europa.eu/paym/target/target-professional-use-documents-links/target_closing_days/html/index.en.html",
       "read_at_ns": READ_AT}
# The weekdays TARGET is closed (1 January, Good Friday, Easter Monday, 1 May,
# 25 and 26 December); those on a weekend need no line.
TARGET_CLOSED = [
    ("2026-01-01", "New Year's Day"), ("2026-04-03", "Good Friday"),
    ("2026-04-06", "Easter Monday"), ("2026-05-01", "Labour Day"),
    ("2026-12-25", "Christmas Day"), ("2027-01-01", "New Year's Day"),
    ("2027-03-26", "Good Friday"), ("2027-03-29", "Easter Monday"),
]
USD_CALENDAR = "CAL-01JAE000000000000000USD1"
XNYS_CALENDAR = "CAL-01JAE000000000000000XNYS"
USSS_CALENDAR = "CAL-01JAE0000000000000000USSS"
EUR_CALENDAR = "CAL-01JAE000000000000000EUR1"


def calendar(calendar_id, name, kind, closed, source, **subject):
    return dict({
        "calendar_id": calendar_id, "name": name, "kind": kind,
        "weekends": [{"weekdays": ["WEEKDAY_SATURDAY", "WEEKDAY_SUNDAY"],
                      "valid_from_date": "2016-01-01"}],
        "days": [{"date": day, "kind": "CALENDAR_DAY_KIND_CLOSED", "name": holiday,
                  "source": source["source_id"]} for day, holiday in closed],
        "coverage": {"from_date": "2026-01-01", "to_date": "2027-12-31"},
        "sources": [source], "version": 1, "valid_time_ns": READ_AT,
        "record_time_ns": READ_AT + 100_000_000_000,
        "note": "seed data for the harness, from the public primary source",
        "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
    }, **subject)


CALENDARS = [
    calendar(USD_CALENDAR, "US dollar", "CALENDAR_KIND_CURRENCY", FED_CLOSED, FED,
             instrument_id=record("USD")["instrument_id"]),
    calendar(XNYS_CALENDAR, "US equity venues", "CALENDAR_KIND_TRADING", NYSE_CLOSED, NYSE,
             venue_ids=["VEN-01JAE0000000000000000XNYS", "VEN-01JAE0000000000000000XNAS"]),
    calendar(USSS_CALENDAR, "US securities settlement", "CALENDAR_KIND_SECURITIES_SETTLEMENT",
             FED_CLOSED, FED, settlement_system={"name": "DTC", "country_code": "US"}),
    calendar(EUR_CALENDAR, "Euro (TARGET)", "CALENDAR_KIND_CURRENCY", TARGET_CLOSED, ECB,
             instrument_id=record("EUR")["instrument_id"]),
]
SEC = {"source_id": "sec-t1", "name": "SEC: the T+1 settlement cycle",
       "address": "https://www.sec.gov/newsroom/press-releases/2023-29", "read_at_ns": READ_AT}


def us_equity(version, lag, effective):
    return {
        "kind": "DATE_RULE_KIND_SETTLEMENT", "subject": "US/equity", "version": version,
        "settlement": {"country_code": "US", "asset_class": "ASSET_CLASS_EQUITY",
                       "lag_days": lag, "counting_calendars": [USSS_CALENDAR],
                       "effective_from_date": effective},
        "sources": [SEC], "record_time_ns": READ_AT + 100_000_000_000,
    }


RULES = [us_equity(1, 2, "2017-09-05"), us_equity(2, 1, "2024-05-28")]

# Contract v23: EUR/USD's spot-date rule (see the module's note).
SPOT_SOURCE = {"source_id": "fx-spot", "name": "Foreign exchange date conventions",
               "address": "https://en.wikipedia.org/wiki/Foreign_exchange_date_conventions",
               "read_at_ns": READ_AT}
SPOT_RULES = [{
    "kind": "DATE_RULE_KIND_SPOT", "subject": "EUR/USD", "version": 1,
    "spot": {"pair": [record("EUR")["instrument_id"], record("USD")["instrument_id"]],
             "lag_days": 2, "counting_calendars": [EUR_CALENDAR],
             "spot_calendars": [EUR_CALENDAR, USD_CALENDAR],
             "cut": {"local_time": "17:00", "time_zone": "America/New_York"},
             "tenor_rule": {"convention": "BUSINESS_DAY_CONVENTION_MODIFIED_FOLLOWING",
                            "end_of_month": True},
             "effective_from_date": "2016-01-01"},
    "sources": [SPOT_SOURCE], "record_time_ns": READ_AT + 100_000_000_000,
}]

# Contract v22: the US equity venues, each as the platform's venue master
# keeps it (meridian-platform's venue_records: the ISO 10383 row's name and
# country, the curated kind and zone, the MIC from its creation date, and the
# vendor's code its venue-codes list holds).
def created(date):
    """Midnight UTC of an ISO date, in nanoseconds."""
    day = datetime.datetime.strptime(date, "%Y-%m-%d").replace(tzinfo=datetime.timezone.utc)
    return int(day.timestamp()) * 1_000_000_000


def venue(venue_id, mic, name, since, alpaca):
    return {
        "venue_id": venue_id, "name": name, "country_code": "US", "kind": "VENUE_KIND_EXCHANGE",
        "identifiers": [
            {"scheme": "iso10383", "value": mic, "source": "", "valid_from_ns": created(since),
             "valid_until_ns": 0},
            {"scheme": "symbol", "value": alpaca, "source": "alpaca", "valid_from_ns": created(since),
             "valid_until_ns": 0},
        ],
        "operating_venue_id": "", "time_zone": "America/New_York", "version": 1,
        "valid_from_ns": created(since), "record_time_ns": READ_AT,
        "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
    }


XNYS = venue("VEN-01JAE0000000000000000XNYS", "XNYS", "NEW YORK STOCK EXCHANGE, INC.", "2005-05-23", "N")
XNAS = venue("VEN-01JAE0000000000000000XNAS", "XNAS", "NASDAQ - ALL MARKETS", "2005-06-27", "Q")
VENUES = {"XNYS": XNYS, "XNAS": XNAS}

# Contract v26: the two prediction venues, each as the venue master keeps
# it -- Kalshi by its MIC, KLSH; Polymarket, which has none, by its own
# symbol, the vendor's code its venue-codes list holds.
KLSH = dict(venue("VEN-01JAE0000000000000000KLSH", "KLSH", "KALSHI EXCHANGE", "2021-07-01", "kalshi"),
            kind="VENUE_KIND_EXCHANGE")
KLSH["identifiers"][1]["source"] = "kalshi"
POLYMARKET = {
    "venue_id": "VEN-01JAE000000000000000POLYM", "name": "Polymarket", "country_code": "",
    "kind": "VENUE_KIND_EXCHANGE",
    "identifiers": [{"scheme": "symbol", "value": "polymarket", "source": "polymarket",
                     "valid_from_ns": created("2020-06-01"), "valid_until_ns": 0}],
    "operating_venue_id": "", "time_zone": "UTC", "version": 1,
    "valid_from_ns": created("2020-06-01"), "record_time_ns": READ_AT,
    "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
}
VENUES["KLSH"] = KLSH
# v27 stage E: Polymarket US, by its MIC, QCEX, as the venue master holds it.
QCEX = dict(venue("VEN-01JAE0000000000000000QCEX", "QCEX", "POLYMARKET US", "2025-07-01",
                  "polymarket-us"), kind="VENUE_KIND_EXCHANGE")
QCEX["identifiers"][1]["source"] = "polymarket-us"
VENUES["QCEX"] = QCEX
EVENT_VENUES = {KLSH["venue_id"]: KLSH, POLYMARKET["venue_id"]: POLYMARKET,
                QCEX["venue_id"]: QCEX}

# A test equity, by its FIGI: Apple Inc., listed on Nasdaq (XNAS).
EQUITIES = {
    "BBG000B9XRY4": {
        "instrument_id": "INS-01JAAPL0000000000000000001",
        "identifiers": [{"scheme": "figi", "value": "BBG000B9XRY4", "source": ""}],
        "asset_class": "ASSET_CLASS_EQUITY",
        "currency": "USD",
        "description": "Apple Inc.",
        "listing_venue_id": XNAS["venue_id"],
        "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
        "version": 1,
    },
}


# Crypto assets the platform's master holds, by their CAIP-19 IDs (contract
# v24): what e2e-lake prices, a record of the platform's rather than a
# provisional local one, which is not priced (W10.6).
BTC_CAIP19 = "bip122:000000000019d6689c085ae165831e93/slip44:0"
USDC_CAIP19 = "eip155:1/erc20:0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
CRYPTO = {
    caip19: {
        "instrument_id": instrument_id,
        "identifiers": [{"scheme": "caip19", "value": caip19, "source": ""}],
        "asset_class": "ASSET_CLASS_CRYPTO_ASSET",
        "description": description,
        "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
        "version": 1,
    }
    for caip19, instrument_id, description in (
        (BTC_CAIP19, f"INS-01JBTC{'0' * 19}1", "Bitcoin"),
        (USDC_CAIP19, f"INS-01JUSDC{'0' * 18}1", "USD Coin"),
    )
}

# Polymarket's collateral, which its contracts pay out in (ruling 550): pUSD,
# a cash record of the platform's, by its token on Polygon and by
# Polymarket's own symbol for it, as the platform's record carries them.
PUSD_CAIP19 = "eip155:137/erc20:0xc011a7e12a19f7b1f670d46f03b03f3342e82dfb"
PUSD = {
    "instrument_id": f"INS-01JPUSD{'0' * 18}1",
    "identifiers": [{"scheme": "caip19", "value": PUSD_CAIP19, "source": ""},
                    {"scheme": "symbol", "value": "PUSD", "source": "polymarket"}],
    "asset_class": "ASSET_CLASS_CASH",
    "currency": "PUSD",
    "description": "pUSD, Polymarket's collateral",
    "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
    "version": 1,
}

# What each event venue's contracts pay out when the source offers no
# currency (v27 stage E): the venue's own payout instrument -- Polymarket's
# 1 pUSD (ruling 550) -- never USD by default; Kalshi and Polymarket US pay
# dollars.
VENUE_PAYOUTS = {
    POLYMARKET["venue_id"]: {"amount": "1", "currency_code": "",
                             "instrument_id": PUSD["instrument_id"]},
    KLSH["venue_id"]: {"amount": "1", "currency_code": "USD", "instrument_id": ""},
    QCEX["venue_id"]: {"amount": "1", "currency_code": "USD", "instrument_id": ""},
}


# The platform's referential records (contract v27, W1.1): the 28 its
# `seed_referential` seeds (meridian-platform 932da80,
# platform_site/domain/referential_records.py, SEEDS), each named by its
# administrator's code as a `symbol` scoped to its source, and by an ISIN
# where one exists (€STR's), each under its type's template. Matched by
# symbol and source, or by ISIN; never minted for a deployment's ask.
REFERENTIAL_SEEDS = (
    ("nyfed", "SOFR", "interest_rate", "USD", "Secured Overnight Financing Rate (SOFR)", ""),
    ("nyfed", "EFFR", "interest_rate", "USD", "Effective Federal Funds Rate (EFFR)", ""),
    ("nyfed", "OBFR", "interest_rate", "USD", "Overnight Bank Funding Rate (OBFR)", ""),
    ("nyfed", "TGCR", "interest_rate", "USD", "Tri-Party General Collateral Rate (TGCR)", ""),
    ("nyfed", "BGCR", "interest_rate", "USD", "Broad General Collateral Rate (BGCR)", ""),
    ("nyfed", "SOFR 30-day average", "interest_rate", "USD", "30-day Average SOFR", ""),
    ("nyfed", "SOFR 90-day average", "interest_rate", "USD", "90-day Average SOFR", ""),
    ("nyfed", "SOFR 180-day average", "interest_rate", "USD", "180-day Average SOFR", ""),
    ("nyfed", "SOFR Index", "index", "USD", "SOFR Index", ""),
    ("frb", "FOMC target range lower bound", "interest_rate", "USD",
     "Federal funds target range, lower bound (FOMC)", ""),
    ("frb", "FOMC target range upper bound", "interest_rate", "USD",
     "Federal funds target range, upper bound (FOMC)", ""),
    ("ecb", "ESTR", "interest_rate", "EUR", "Euro short-term rate (€STR)", "EU000A2X2A25"),
    ("ecb", "ESTR 1-week compounded average", "interest_rate", "EUR",
     "€STR compounded average rate, 1 week", ""),
    ("ecb", "ESTR 1-month compounded average", "interest_rate", "EUR",
     "€STR compounded average rate, 1 month", ""),
    ("ecb", "ESTR 3-month compounded average", "interest_rate", "EUR",
     "€STR compounded average rate, 3 months", ""),
    ("ecb", "ESTR 6-month compounded average", "interest_rate", "EUR",
     "€STR compounded average rate, 6 months", ""),
    ("ecb", "ESTR 12-month compounded average", "interest_rate", "EUR",
     "€STR compounded average rate, 12 months", ""),
    ("ecb", "ESTR compounded index", "index", "EUR", "€STR compounded index", ""),
    ("ustreasury", "Daily Treasury Par Yield Curve Rates", "rate_curve", "USD",
     "Daily Treasury par yield curve", ""),
    ("ustreasury", "Daily Treasury Par Real Yield Curve Rates", "rate_curve", "USD",
     "Daily Treasury par real yield curve", ""),
    ("ustreasury", "Daily Treasury Bill Rates", "rate_curve", "USD", "Daily Treasury bill rates", ""),
    ("frb", "feds200628", "rate_curve", "USD",
     "Treasury zero-coupon yield curve (Federal Reserve Board)", ""),
    # H.15's other series (ruling 541; meridian-platform 932da80).
    ("frb", "Treasury bills secondary market", "rate_curve", "USD",
     "Treasury bill secondary market rates, 4-week to 1-year (H.15)", ""),
    ("frb", "AA nonfinancial commercial paper", "rate_curve", "USD",
     "AA nonfinancial commercial paper rates, 30- to 90-day (H.15)", ""),
    ("frb", "AA financial commercial paper", "rate_curve", "USD",
     "AA financial commercial paper rates, 30- to 90-day (H.15)", ""),
    ("frb", "Bank prime loan rate", "interest_rate", "USD", "Bank prime loan rate (H.15)", ""),
    ("frb", "Discount window primary credit rate", "interest_rate", "USD",
     "Discount window primary credit rate (H.15)", ""),
    ("frb", "Treasury inflation-indexed long-term average", "interest_rate", "USD",
     "Treasury inflation-indexed long-term average yield, over 10 years (H.15)", ""),
)


def referential_record(n, source, code, kind, currency, description, isin):
    identifiers = [{"scheme": "symbol", "value": code, "source": source}]
    if isin:
        identifiers.append({"scheme": "isin", "value": isin, "source": ""})
    return {
        "instrument_id": f"INS-01JREF{n:020d}",
        "identifiers": identifiers,
        "asset_class": "ASSET_CLASS_REFERENTIAL",
        "instrument_type": kind,
        "currency": currency,
        "description": description,
        "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
        "version": 1,
        "template_id": f"TPL-{kind.replace('_', '-')}",
        "template_version": "1",
    }


REFERENTIAL_RECORDS = [referential_record(n, *seed) for n, seed in enumerate(REFERENTIAL_SEEDS, 1)]
# By symbol and its source, and by ISIN.
REFERENTIAL_BY_SYMBOL = {(i["value"], i["source"]): r for r in REFERENTIAL_RECORDS
                         for i in r["identifiers"] if i["scheme"] == "symbol"}
REFERENTIAL_BY_ISIN = {i["value"]: r for r in REFERENTIAL_RECORDS
                       for i in r["identifiers"] if i["scheme"] == "isin"}

def calendars_for(one, many):
    """The seed answer to a pull by its one subject; versions after
    `after_version` only, on a refresh."""
    after = int(one("after_version") or 0)
    calendars, rules = [], []
    if one("calendar_id"):
        calendars = [c for c in CALENDARS if c["calendar_id"] == one("calendar_id")]
    elif one("instrument_id"):
        calendars = [c for c in CALENDARS if c.get("instrument_id") == one("instrument_id")]
    elif one("venue_id"):
        calendars = [c for c in CALENDARS if one("venue_id") in c.get("venue_ids", [])]
    elif one("kind") == "DATE_RULE_KIND_SPOT":
        pair = sorted(many("pair"))
        rules = [r for r in SPOT_RULES if sorted(r["spot"]["pair"]) == pair]
        named = {c for r in rules
                 for c in r["spot"]["counting_calendars"] + r["spot"]["spot_calendars"]}
        rules = [r for r in rules if r["version"] > after]
        calendars = [c for c in CALENDARS if c["calendar_id"] in named]
    elif one("kind") == "DATE_RULE_KIND_SETTLEMENT":
        rules = [r for r in RULES
                 if r["settlement"]["country_code"] == one("country_code")
                 and r["settlement"]["asset_class"] == one("asset_class")]
        named = {c for r in rules for c in r["settlement"]["counting_calendars"]}
        rules = [r for r in rules if r["version"] > after]
        calendars = [c for c in CALENDARS if c["calendar_id"] in named]
    calendars = [c for c in calendars if c["version"] > after or rules]
    return {"calendars": calendars, "rules": rules}


# ── Contract v24 (W3.4, W3.24, W3.25, W3.27, W3.28) ─────────────────────────
#
# The one-ID routes, as the platform builds them: the matching and minting
# request, the template catalogue, the licence list, the replacement feed
# and the usage statistics. `HARNESS_PLATFORM_V24=0` serves none of them, as
# a platform before v24 does (404), so a deployment shows it treats that as
# unreachable. What each request carried is kept for a run to read on
# GET /e2e/heard -- the identifiers sent, never anything else of the
# deployment -- and a run changes the licence list and the feed by
# POST /e2e/licence and POST /e2e/replacements, as the platform's staff would.

V24 = os.environ.get("HARNESS_PLATFORM_V24", "1") != "0"
# The administrator_symbol family and the administrators it admits, as the
# platform lists them (ruling 545): a symbol whose source is one of these.
LICENCE = {
    "version": 1,
    "schemes": [s for s in (os.environ.get("HARNESS_LICENCE")
                            or "figi,lei,ticker_and_venue,caip19,iso4217,sec_series,sec_class,"
                               "administrator_symbol,cik").split(",") if s],
    "pace": {"batch_size": 100, "batches_per_minute": 6},
    "administrators": ["nyfed", "frb", "ecb", "ustreasury"],
}
TEMPLATES = [
    {"template_id": "TPL-equity", "template_version": "1", "name": "Equity", "asset_class": "equity",
     "instrument_type": "", "base": False, "calculation": "",
     "fields": [{"path": "identifiers", "type": "identifier", "required": True},
                {"path": "description", "type": "text", "required": True},
                {"path": "currency", "type": "currency", "required": True},
                {"path": "listing_venue_id", "type": "venue", "required": False}],
     "match_rule": ["share_class_figi", "figi", "composite_figi", "ticker"]},
    {"template_id": "TPL-cash", "template_version": "1", "name": "Cash and currency",
     "asset_class": "cash", "instrument_type": "", "base": False, "calculation": "",
     "fields": [{"path": "identifiers", "type": "identifier", "required": True}],
     "match_rule": ["iso4217"]},
]


# Contract v26 (W1.24): each template's next version with its calculation
# package, declarative data the analytics engine's built-in model and every
# pricing plugin routed for it must equal exactly; and the binary event
# contract's, whose fields add a range market's bounds (A4), none required.
def packaged(template, version, package, extra_fields=()):
    return {**template, "template_version": str(version),
            "fields": template["fields"] + list(extra_fields), "calculation_package": package}


EVENT_CONTRACT_V1 = {
    "template_id": "TPL-binary-event-contract", "template_version": "1",
    "name": "Binary event contract", "asset_class": "event_contract",
    "instrument_type": "binary_event_contract", "base": False, "calculation": "",
    "fields": [{"path": "identifiers", "type": "identifier", "required": True},
               {"path": "description", "type": "text", "required": True},
               {"path": "currency", "type": "text", "required": True},
               {"path": "listing_venue_id", "type": "identifier", "required": True},
               {"path": "instrument_type", "type": "enum", "required": True},
               {"path": "binary_event_contract.payout", "type": "Money", "required": True},
               {"path": "binary_event_contract.closes_at_ns", "type": "moment", "required": False}],
    "match_rule": ["ticker"],
}
TEMPLATES += [
    packaged(TEMPLATES[1], 2, {
        "model_id": "cash", "model_version": 1, "pricing_inputs": ["close"],
        "corporate_action_factors": "", "calculation_version": "1",
        "test_cases": [{"name": "1,000 EUR at an EURUSD close of 1.0850, base USD",
                        "inputs": json.dumps({"fx": "1.0850", "quantity": "1000"}, separators=(",", ":")),
                        "expected": json.dumps({"market_value": "1085.00"}, separators=(",", ":"))}]}),
    packaged(TEMPLATES[0], 2, {
        "model_id": "quoted_price", "model_version": 1, "pricing_inputs": ["close"],
        "corporate_action_factors": "the template's split and distribution factors, CRSP's A(t) = P(t)/C(t)",
        "calculation_version": "1",
        "test_cases": [{"name": "10 shares at a close of 227.50 USD, base USD",
                        "inputs": json.dumps({"close": "227.50", "fx": "1", "quantity": "10"}, separators=(",", ":")),
                        "expected": json.dumps({"market_value": "2275.00"}, separators=(",", ":"))}]}),
    EVENT_CONTRACT_V1,
    packaged(EVENT_CONTRACT_V1, 2, {
        "model_id": "binary_event", "model_version": 1, "pricing_inputs": ["mid", "last", "outcome"],
        "corporate_action_factors": "", "calculation_version": "1",
        "test_cases": [{"name": "100 contracts paying 1 USD at a mid of 0.62",
                        "inputs": json.dumps({"mid": "0.62", "payout": "1", "quantity": "100"}, separators=(",", ":")),
                        "expected": json.dumps({"market_value": "62.00"}, separators=(",", ":"))}]},
        [{"path": f"binary_event_contract.{part}", "type": kind, "required": False}
         for part, kind in (("floor.value", "decimal"), ("floor.inclusive", "boolean"),
                            ("cap.value", "decimal"), ("cap.inclusive", "boolean"),
                            ("bound_unit", "text"))]),
]
# Contract v28 (W3.33): the funds the picker's search brings in are minted
# under the fund template, its fields the equity's.
TEMPLATES.append({**TEMPLATES[0], "template_id": "TPL-fund", "name": "ETF and mutual fund",
                  "asset_class": "fund",
                  "match_rule": ["share_class_figi", "composite_figi", "figi", "ticker",
                                 "sec_series", "sec_class"]})
# FIGIs the platform's source confirms and mints from, and those it cannot:
# a near match, minted and flagged; anything else, its staff's queue.
MINTS = {f for f in (os.environ.get("HARNESS_PLATFORM_MINTS") or "BBG00NEWMINT1,BBG00NEWMINT2").split(",") if f}
REPLACEMENTS = []
ANSWERS = {}
HEARD = []
MINTED = {}


def minted_record(figi):
    """A record the platform mints from its own source for `figi`."""
    if figi not in MINTED:
        n = len(MINTED) + 1
        MINTED[figi] = {
            "instrument_id": f"INS-01JMINT{n:019d}",
            "identifiers": [{"scheme": "share_class_figi", "value": figi, "source": ""}],
            "asset_class": "ASSET_CLASS_EQUITY",
            "currency": "USD",
            "description": f"Minted from the platform's source for {figi}",
            "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_DEFINE",
            "version": 1,
            "template_id": "TPL-equity",
            "template_version": "1",
        }
    return MINTED[figi]


# ── Contract v26 (W1.30, W3.29): event contracts and their groups ───────────

EVENT_CONTRACTS = {}
EVENT_GROUPS = {}


def event_venue(source):
    """The venue a symbol sent as `ticker` names: by its MIC, or by its
    `VEN-` ID where it has none (W3.4's spelling, ruled 2026-10-14)."""
    if source in EVENT_VENUES:
        return EVENT_VENUES[source]
    return next((v for v in EVENT_VENUES.values()
                 if any(i["scheme"] == "iso10383" and i["value"] == source for i in v["identifiers"])),
                None)


def event_contract(value, venue_record, asked):
    """A binary event contract minted by its venue symbol, once: the
    template's version 2, paying one unit of the currency offered."""
    key = (value, venue_record["venue_id"])
    if key not in EVENT_CONTRACTS:
        offered = {f.get("path"): f.get("value", "") for f in asked.get("fields", [])}
        # A currency the source offers is paid in; else the venue's own
        # payout instrument (v27 stage E).
        payout = ({"amount": "1", "currency_code": offered["currency"], "instrument_id": ""}
                  if offered.get("currency") else
                  VENUE_PAYOUTS.get(venue_record["venue_id"],
                                    {"amount": "1", "currency_code": "USD", "instrument_id": ""}))
        currency = offered.get("currency") or "USD"
        n = len(EVENT_CONTRACTS) + 1
        mic = next((i["value"] for i in venue_record["identifiers"] if i["scheme"] == "iso10383"), "")
        EVENT_CONTRACTS[key] = {
            "instrument_id": f"INS-01JEVT{n:020d}",
            "identifiers": [{"scheme": "ticker", "value": value, "source": mic or venue_record["venue_id"]}],
            "asset_class": "ASSET_CLASS_EVENT_CONTRACT",
            "instrument_type": "binary_event_contract",
            "currency": currency,
            "description": offered.get("description") or f"{venue_record['name']} event contract {value}",
            "listing_venue_id": venue_record["venue_id"],
            "binary_event_contract": {"payout": payout, "closes_at_ns": 0},
            "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
            "version": 1,
            "template_id": "TPL-binary-event-contract",
            "template_version": "2",
        }
    return EVENT_CONTRACTS[key]


def held_instrument(instrument_id):
    return (instrument_id in {r["instrument_id"] for r in EVENT_CONTRACTS.values()}
            or instrument_id in {r["instrument_id"] for r in MINTED.values()}
            or instrument_id in {r["instrument_id"] for r in EQUITIES.values()}
            or instrument_id in {r["instrument_id"] for r in CRYPTO.values()}
            or instrument_id == PUSD["instrument_id"]
            or instrument_id in {r["instrument_id"] for r in REFERENTIAL_RECORDS})


def event_group(asked):
    """The platform's answer to a deployment's grouping (W1.30): matched by
    the venue and its event's reference, minted on first asking, a new
    version where the flags or members changed; a member it does not hold,
    or an unknown venue, its staff's queue. A retry of a request key is
    answered as first answered."""
    key = asked.get("request_key", "")
    if key and key in ANSWERS:
        return ANSWERS[key]
    members = sorted(asked.get("member_instrument_ids", []))
    venue_id = asked.get("venue_id", "")
    ref = asked.get("venue_event_ref", "")
    if venue_id not in EVENT_VENUES or not ref or not members \
            or not all(held_instrument(m) for m in members):
        answer = {"minted": False, "pending_review": True}
    else:
        flags = {name: asked.get(name) or "EVENT_GROUP_FLAG_NOT_STATED"
                 for name in ("mutually_exclusive", "exhaustive")}
        held = EVENT_GROUPS.get((venue_id, ref))
        minted = held is None
        if held is None or held["member_instrument_ids"] != members \
                or any(held[name] != flag for name, flag in flags.items()):
            version = 1 if held is None else held["version"] + 1
            group_id = held["event_group_id"] if held else f"EVG-01JEVG{len(EVENT_GROUPS) + 1:020d}"
            held = {"event_group_id": group_id, "venue_id": venue_id, "venue_event_ref": ref,
                    "title": asked.get("title", ""), "member_instrument_ids": members,
                    **flags, "series_id": asked.get("series_id", ""), "version": version,
                    "valid_from_ns": str(asked.get("valid_from_ns") or 0),
                    "record_time_ns": str(time.time_ns()), "provisional": False}
            EVENT_GROUPS[(venue_id, ref)] = held
        answer = {"event_group": held, "minted": minted, "pending_review": False}
    if key:
        ANSWERS[key] = answer
    return answer


def match_or_mint(asked):
    """The platform's answer to a matching request (W3.4): a match, a mint
    from its own source, or its staff's queue; a retry of a request key
    answered as first answered."""
    key = asked.get("request_key", "")
    if key and key in ANSWERS:
        return ANSWERS[key]
    answer = {"minted": False, "pending_review": True}
    for identifier in asked.get("identifiers", []):
        scheme, value = identifier.get("scheme", ""), identifier.get("value", "")
        # Contract v26: a binary event contract by its venue's symbol.
        venue_record = event_venue(identifier.get("source", "")) if scheme == "ticker" else None
        if venue_record and asked.get("asset_class") == "event_contract" and value:
            fresh = (value, venue_record["venue_id"]) not in EVENT_CONTRACTS
            answer = {"minted": fresh, "instrument": event_contract(value, venue_record, asked),
                      "venues": [venue_record]}
            break
        minted = candidate_minted([identifier])
        if minted:
            fresh, held = minted
            answer = {"minted": fresh, "instrument": held,
                      "venues": [XNAS] if held["listing_venue_id"] else [US_COMPOSITE]}
            break
        if scheme in ("figi", "share_class_figi", "composite_figi") and value in EQUITIES:
            answer = {"minted": False, "instrument": EQUITIES[value], "venues": [XNAS]}
            break
        if scheme == "caip19" and value in CRYPTO:
            answer = {"minted": False, "instrument": CRYPTO[value]}
            break
        if (scheme, value, identifier.get("source", "")) in {
                (i["scheme"], i["value"], i["source"]) for i in PUSD["identifiers"]}:
            answer = {"minted": False, "instrument": PUSD}
            break
        # The seeded referential records, as the platform matches them: by
        # the administrator's code and its source, or by ISIN.
        source = identifier.get("source", "")
        held = (REFERENTIAL_BY_SYMBOL.get((value, source))
                if scheme == "symbol" and source in LICENCE["administrators"] else
                REFERENTIAL_BY_ISIN.get(value) if scheme == "isin" else None)
        if held:
            answer = {"minted": False, "instrument": held}
            break
        if scheme == "iso4217" and value.upper() in NAMES:
            answer = {"minted": False, "instrument": record(value.upper())}
            break
        if scheme in ("figi", "share_class_figi") and value in MINTS:
            answer = {"minted": True, "instrument": minted_record(value)}
            break
    if key:
        ANSWERS[key] = answer
    return answer


# Contract v28 (W1.32; ruling 612): the companies its source -- EDGAR's
# company list -- confirms, by CIK, each minted `ENT-` on first asking and
# matched after; any other CIK or LEI its staff's queue. The CIK is on the
# licence list among the SEC's identifiers (W1.32, W3.25).
COMPANY_SOURCE = {
    "0000320193": ("ENT-01JC28APPLEINC000000000001", "Apple Inc.", "HWUPKR0MPOU8FGXBT394",
                   [{"name": "APPLE COMPUTER INC", "valid_until": "2007-01-04"}]),
    "0001067983": ("ENT-01JC28BERKSHIRE00000000001", "Berkshire Hathaway Inc", "", []),
    # A rating agency, the rater of e2e-lake's rating action (contract v28, FDP Q7).
    "0001059556": ("ENT-01JC28MOODYS00000000000001", "Moody's Corp", "", []),
}
COMPANIES = {}


def company(asked):
    """The platform's answer to a company (W1.32): matched, minted from its
    source, or its staff's queue; a retry of a request key answered as first
    answered."""
    key = asked.get("request_key", "")
    if key and key in ANSWERS:
        return ANSWERS[key]
    answer = {"minted": False, "pending_review": True}
    for identifier in asked.get("identifiers", []):
        scheme, value = identifier.get("scheme", ""), identifier.get("value", "")
        cik = value if scheme == "cik" else next(
            (c for c, (_, _, lei, _) in COMPANY_SOURCE.items() if scheme == "lei" and lei == value),
            None)
        if cik in COMPANY_SOURCE:
            company_id, name, lei, former = COMPANY_SOURCE[cik]
            fresh = cik not in COMPANIES
            COMPANIES.setdefault(cik, {
                "company_id": company_id, "name": name, "version": 1,
                "identifiers": [{"scheme": "cik", "value": cik}]
                + ([{"scheme": "lei", "value": lei}] if lei else []),
                "former_names": former,
            })
            answer = {"company": COMPANIES[cik], "minted": fresh, "pending_review": False}
            break
    if key:
        ANSWERS[key] = answer
    return answer


# ── Contract v28 (W3.33; rulings 757, 775, 778): the picker's search ──────

# Ruling 779: the platform's one venue for the "US composite" venue, with
# no MIC, a ticker minted on a US composite held on it by its VEN- ID.
US_COMPOSITE = {
    "venue_id": "VEN-01JAE00000000000USCOMPOS", "name": "US composite",
    "country_code": "US", "kind": "VENUE_KIND_REPORTING_FACILITY", "identifiers": [],
    "operating_venue_id": "", "time_zone": "America/New_York", "version": 1,
}
CANDIDATE_SEEDS = {
    "SPY": {"name": "SPDR S&P 500 ETF Trust", "mic": "", "composite": "BBG000BDTBL9",
            "share_class": "BBG001S72SM3", "listing": ""},
    "QQQ": {"name": "Invesco QQQ Trust, Series 1", "mic": "XNAS", "composite": "BBG000BSWKH7",
            "share_class": "BBG001S72TT3", "listing": "BBG000BSWKJ5"},
}
CANDIDATES_MINTED = {}


def candidate_identifiers(ticker, seed):
    """What an escalation sends for a seed: its share-class FIGI, its US
    composite paired with the country, and, where its listing is known, the
    listing's FIGI and the ticker on its MIC."""
    held = [{"scheme": "share_class_figi", "value": seed["share_class"], "source": ""},
            {"scheme": "composite_figi", "value": seed["composite"], "source": "US"}]
    if seed["mic"]:
        held += [{"scheme": "figi", "value": seed["listing"], "source": seed["mic"]},
                 {"scheme": "ticker", "value": ticker, "source": seed["mic"]}]
    else:
        held.append({"scheme": "ticker", "value": ticker, "source": US_COMPOSITE["venue_id"]})
    return held


def candidates(asked):
    """The picker's search (W3.33): the record held first, then the seeds
    the query names, by ticker or a word of the name; nothing kept."""
    query = " ".join(str(asked.get("query") or "").split()).casefold()
    limit = int(asked.get("limit") or 10)
    found = []
    apple = EQUITIES["BBG000B9XRY4"]
    if query and (query == "aapl" or query in "apple inc."):
        found.append({"instrument_id": apple["instrument_id"], "ticker": "AAPL", "mic": "XNAS",
                      "country": "US", "name": apple["description"],
                      "asset_class": "ASSET_CLASS_EQUITY", "currency": "USD",
                      "composite_figi": "BBG000B9XRY4", "share_class_figi": "BBG001S5N8V8",
                      "identifiers": []})
    for ticker, seed in CANDIDATE_SEEDS.items():
        if query and (ticker.casefold().startswith(query) or query in seed["name"].casefold()):
            minted = CANDIDATES_MINTED.get(ticker)
            found.append({"instrument_id": minted["instrument_id"] if minted else "",
                          "ticker": ticker, "mic": seed["mic"], "country": "US",
                          "name": seed["name"], "asset_class": "ASSET_CLASS_FUND",
                          "currency": "USD", "composite_figi": seed["composite"],
                          "share_class_figi": seed["share_class"],
                          "identifiers": [] if minted else candidate_identifiers(ticker, seed)})
    return {"candidates": found[:limit],
            "sources": [{"source": "harness stand-in", "outcome": "found" if found else "not_found",
                         "read_at": datetime.datetime.now(datetime.timezone.utc)
                         .strftime("%Y-%m-%dT%H:%M:%SZ")}]}


def candidate_minted(identifiers):
    """A record minted once for a seed a matching request names by its
    FIGIs (778: on its US composite, no listing venue where none is known)."""
    for ticker, seed in CANDIDATE_SEEDS.items():
        figis = {seed["composite"], seed["share_class"]}
        if any(i.get("value") in figis for i in identifiers):
            fresh = ticker not in CANDIDATES_MINTED
            if fresh:
                n = len(CANDIDATES_MINTED) + 1
                CANDIDATES_MINTED[ticker] = {
                    "instrument_id": f"INS-01JCAND{n:019d}",
                    "identifiers": candidate_identifiers(ticker, seed),
                    "asset_class": "ASSET_CLASS_FUND",
                    "currency": "USD",
                    "description": seed["name"],
                    "listing_venue_id": XNAS["venue_id"] if seed["mic"] == "XNAS" else "",
                    "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE",
                    "version": 1,
                    "template_id": "TPL-fund",
                    "template_version": "1",
                }
            return fresh, CANDIDATES_MINTED[ticker]
    return None


REGISTRY = os.environ.get("HARNESS_REGISTRY", "")


def registry_file(name):
    with open(os.path.join(REGISTRY, name), encoding="utf-8") as held:
        return json.load(held)


def resolved(path):
    """`/api/v1/registry/listings/{organisation}/{plugin}/versions/{version}/resolve`."""
    parts = path.strip("/").split("/")
    if len(parts) != 9 or parts[:4] != ["api", "v1", "registry", "listings"] \
            or parts[6] != "versions" or parts[8] != "resolve":
        return None
    return registry_file("resolutions.json").get(f"{parts[4]}/{parts[5]}@{parts[7]}")


class Oci(http.server.BaseHTTPRequestHandler):
    """The OCI registry the resolutions name: manifests and blobs by digest,
    and each image's referrers, read-only and anonymous."""

    def sent(self, status, data=b"", media="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", media)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/v2/":
            return self.sent(200, b"{}")
        index = registry_file("oci/index.json")
        _, _, rest = path.partition("/v2/")
        kind, digest = (rest.split("/")[-2:] + ["", ""])[:2] if "/" in rest else ("", "")
        tag_schema_only = index.get("tag_schema_only", [])
        if kind == "referrers" and digest in index["referrers"] and digest not in tag_schema_only:
            HEARD.append({"method": "GET", "path": path})
            return self.sent(200, json.dumps(index["referrers"][digest]).encode(),
                             "application/vnd.oci.image.index.v1+json")
        # The referrers tag schema (v26), `sha256-<hex>`: what a registry
        # without the referrers API serves, as ghcr.io does, cosign keeping
        # the index there; its API answers those images 404.
        tagged = digest.replace("-", ":", 1) if digest.startswith("sha256-") else ""
        if kind == "manifests" and tagged in index["referrers"]:
            HEARD.append({"method": "GET", "path": path})
            return self.sent(200, json.dumps(index["referrers"][tagged]).encode(),
                             "application/vnd.oci.image.index.v1+json")
        hex_ = digest.partition("sha256:")[2]
        held = os.path.join(REGISTRY, "oci", hex_)
        if kind in ("manifests", "blobs") and len(hex_) == 64 and os.path.isfile(held):
            with open(held, "rb") as kept:
                data = kept.read()
            media = index["media"].get(digest, "application/octet-stream") \
                if kind == "manifests" else "application/octet-stream"
            HEARD.append({"method": "GET", "path": path})
            return self.sent(200, data, media)
        self.sent(404, b'{"errors":[{"code":"NOT_FOUND"}]}')

    def log_message(self, *_args):
        pass


def serve_oci():
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls = os.environ["HARNESS_REGISTRY_TLS"]
    context.load_cert_chain(os.path.join(tls, "platform.pem"), os.path.join(tls, "platform-key.pem"))
    server = http.server.ThreadingHTTPServer(("0.0.0.0", 8443), Oci)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


class Handler(http.server.BaseHTTPRequestHandler):
    def answer(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        asked = urllib.parse.parse_qs(url.query)
        one = lambda name: asked.get(name, [""])[0]
        code = one("value").upper()
        if url.path == "/e2e/heard":
            return self.answer(200, {"heard": HEARD})
        if url.path == "/e2e/event-groups":
            # Contract v26: what it minted, for a run to read.
            return self.answer(200, {"event_groups": list(EVENT_GROUPS.values()),
                                     "event_contracts": list(EVENT_CONTRACTS.values())})
        if url.path == "/api/v1/registry/catalogue" and REGISTRY:
            HEARD.append({"method": "GET", "path": self.path})
            return self.answer(200, registry_file("catalogue.json"))
        if url.path in ("/api/v1/reference/templates", "/api/v1/reference/licence-list",
                        "/api/v1/reference/replacements"):
            HEARD.append({"method": "GET", "path": self.path})
            if not V24:
                return self.answer(404, {"detail": "Not Found"})
            if url.path.endswith("/templates"):
                # As the platform: after_version only beside a subject
                # (template_id or asset_class; W3.24, ruled 2026-10-14).
                template_id, asset_class = one("template_id"), one("asset_class")
                if one("after_version") and not (template_id or asset_class):
                    return self.answer(400, {"detail": "after_version needs a subject: "
                                             "name template_id or asset_class"})
                after = int(one("after_version") or 0)
                return self.answer(200, {"templates": [
                    t for t in TEMPLATES
                    if int(t["template_version"]) > after
                    and (not template_id or t["template_id"] == template_id)
                    and (not asset_class or t.get("asset_class") == asset_class)]})
            if url.path.endswith("/licence-list"):
                return self.answer(200, {**LICENCE, "version": str(LICENCE["version"])})
            after = int(one("after_sequence") or 0)
            return self.answer(200, {"entries": [
                {**entry, "sequence": str(entry["sequence"])}
                for entry in REPLACEMENTS if entry["sequence"] > after][:1000]})
        if url.path == "/api/v1/reference/calendars":
            answer = calendars_for(one, lambda name: asked.get(name, []))
            found = answer["calendars"] or answer["rules"]
            body = json.dumps(answer if found else {"found": False}).encode()
            self.send_response(200 if found else 404)
        elif (url.path == "/api/v1/reference/identifiers/resolve"
                and one("scheme") == "iso4217" and code in NAMES):
            time.sleep(SLOW.get(code, 0))
            body = json.dumps({"found": True, "instrument": record(code), "venues": []}).encode()
            self.send_response(200)
        elif (url.path == "/api/v1/reference/identifiers/resolve"
                and one("scheme") == "symbol" and one("value") == "polymarket"
                and one("source") in ("", "polymarket")):
            # Contract v26: Polymarket, which has no MIC, by its own symbol.
            body = json.dumps({"found": True, "venues": [POLYMARKET]}).encode()
            self.send_response(200)
        elif (url.path == "/api/v1/reference/identifiers/resolve"
                and one("scheme") == "ticker" and event_venue(one("source"))
                and (one("value"), event_venue(one("source"))["venue_id"]) in EVENT_CONTRACTS):
            # Contract v26: an event contract minted already, by its symbol.
            venue_record = event_venue(one("source"))
            body = json.dumps({"found": True,
                               "instrument": EVENT_CONTRACTS[(one("value"), venue_record["venue_id"])],
                               "venues": [venue_record]}).encode()
            self.send_response(200)
        elif (url.path == "/api/v1/reference/identifiers/resolve"
                and one("scheme") == "symbol" and one("source") in LICENCE["administrators"]
                and (one("value"), one("source")) in REFERENTIAL_BY_SYMBOL):
            # Contract v27 (ruling 545): a seeded referential record by its
            # administrator's code, found only with that administrator named.
            body = json.dumps({"found": True,
                               "instrument": REFERENTIAL_BY_SYMBOL[(one("value"), one("source"))],
                               "venues": []}).encode()
            self.send_response(200)
        elif (url.path == "/api/v1/reference/identifiers/resolve"
                and one("scheme") == "iso10383" and code in VENUES):
            # A venue by its MIC: the one venue, and no instrument (W3.15).
            body = json.dumps({"found": True, "venues": [VENUES[code]]}).encode()
            self.send_response(200)
        elif (url.path == "/api/v1/reference/identifiers/resolve"
                and one("scheme") in ("figi", "share_class_figi") and one("value") in MINTS and V24):
            # A security its source now confirms (contract v24): its record,
            # minted on first asking, as a person's ask finds it (W3.3).
            body = json.dumps({"found": True, "instrument": minted_record(one("value")),
                               "venues": []}).encode()
            self.send_response(200)
        elif (url.path == "/api/v1/reference/identifiers/resolve"
                and one("scheme") == "figi" and one("value") in EQUITIES):
            # The equity with the venue it is listed on (W3.3).
            body = json.dumps({"found": True, "instrument": EQUITIES[one("value")],
                               "venues": [XNAS]}).encode()
            self.send_response(200)
        else:
            body = json.dumps({"found": False}).encode()
            self.send_response(404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        length = int(self.headers.get("Content-Length") or 0)
        try:
            asked = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            return self.answer(400, {"detail": "not JSON"})
        if REGISTRY and url.path == "/api/v1/registry/usage-reports":
            HEARD.append({"method": "POST", "path": url.path, "body": asked})
            return self.answer(200, {"received_at_ns": str(time.time_ns())})
        if REGISTRY and url.path.startswith("/api/v1/registry/listings/"):
            HEARD.append({"method": "POST", "path": url.path, "body": asked})
            answer = resolved(url.path)
            return self.answer(200, answer) if answer else self.answer(404, {"detail": "Not Found"})
        if url.path == "/e2e/licence":
            LICENCE["schemes"] = asked.get("schemes", LICENCE["schemes"])
            LICENCE["administrators"] = asked.get("administrators", LICENCE["administrators"])
            LICENCE["version"] += 1
            return self.answer(200, LICENCE)
        if url.path == "/e2e/confirm":
            # The platform's source now confirms a security its staff held.
            MINTS.update(asked.get("figis", []))
            return self.answer(200, {"mints": sorted(MINTS)})
        if url.path == "/e2e/replacements":
            REPLACEMENTS.extend(asked.get("entries", []))
            return self.answer(200, {"entries": REPLACEMENTS})
        if url.path == "/api/v1/reference/escalate-event-group":
            # Contract v26 (W1.30): a venue's grouping, matched or minted.
            HEARD.append({"method": "POST", "path": url.path, "body": asked})
            return self.answer(200, event_group(asked))
        if url.path == "/api/v1/reference/match-company":
            # Contract v28 (W1.32; ruling 612): a company, matched or minted.
            HEARD.append({"method": "POST", "path": url.path, "body": asked})
            return self.answer(200, company(asked))
        if url.path == "/api/v1/reference/instrument-candidates" and V24:
            # Contract v28 (W3.33): the picker's search; what was typed is
            # neither kept nor heard (ruling 761).
            return self.answer(200, candidates(asked))
        if url.path in ("/api/v1/reference/escalate-instrument", "/api/v1/reference/usage-reports"):
            HEARD.append({"method": "POST", "path": url.path, "body": asked})
            if not V24:
                return self.answer(404, {"detail": "Not Found"})
            if url.path.endswith("/usage-reports"):
                return self.answer(200, {"received": True})
            return self.answer(200, match_or_mint(asked))
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *_args):
        pass


if REGISTRY:
    threading.Thread(target=serve_oci, daemon=True).start()
http.server.ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
