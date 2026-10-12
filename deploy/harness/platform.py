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
market."""
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
USD_CALENDAR = "CAL-01JAE000000000000000USD1"
XNYS_CALENDAR = "CAL-01JAE000000000000000XNYS"
USSS_CALENDAR = "CAL-01JAE0000000000000000USSS"


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
    elif one("kind") == "DATE_RULE_KIND_SETTLEMENT":
        rules = [r for r in RULES
                 if r["settlement"]["country_code"] == one("country_code")
                 and r["settlement"]["asset_class"] == one("asset_class")]
        named = {c for r in rules for c in r["settlement"]["counting_calendars"]}
        rules = [r for r in rules if r["version"] > after]
        calendars = [c for c in CALENDARS if c["calendar_id"] in named]
    calendars = [c for c in calendars if c["version"] > after or rules]
    return {"calendars": calendars, "rules": rules}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        asked = urllib.parse.parse_qs(url.query)
        one = lambda name: asked.get(name, [""])[0]
        code = one("value").upper()
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
        else:
            body = json.dumps({"found": False}).encode()
            self.send_response(404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *_args):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
