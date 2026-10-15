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
the replacement feed; and the usage statistics. See V24 below."""
import datetime
import http.server
import json
import os
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
LICENCE = {
    "version": 1,
    "schemes": [s for s in (os.environ.get("HARNESS_LICENCE")
                            or "figi,lei,ticker_and_venue,caip19,iso4217,sec_series,sec_class").split(",") if s],
    "pace": {"batch_size": 100, "batches_per_minute": 6},
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
        if scheme in ("figi", "share_class_figi", "composite_figi") and value in EQUITIES:
            answer = {"minted": False, "instrument": EQUITIES[value], "venues": [XNAS]}
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
        if url.path in ("/api/v1/reference/templates", "/api/v1/reference/licence-list",
                        "/api/v1/reference/replacements"):
            HEARD.append({"method": "GET", "path": self.path})
            if not V24:
                return self.answer(404, {"detail": "Not Found"})
            if url.path.endswith("/templates"):
                after = int(one("after_version") or 0)
                return self.answer(200, {"templates": [t for t in TEMPLATES
                                                       if int(t["template_version"]) > after]})
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
        if url.path == "/e2e/licence":
            LICENCE["schemes"] = asked.get("schemes", LICENCE["schemes"])
            LICENCE["version"] += 1
            return self.answer(200, LICENCE)
        if url.path == "/e2e/confirm":
            # The platform's source now confirms a security its staff held.
            MINTS.update(asked.get("figis", []))
            return self.answer(200, {"mints": sorted(MINTS)})
        if url.path == "/e2e/replacements":
            REPLACEMENTS.extend(asked.get("entries", []))
            return self.answer(200, {"entries": REPLACEMENTS})
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


http.server.ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
