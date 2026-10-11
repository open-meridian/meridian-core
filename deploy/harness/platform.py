"""A stand-in for the platform's master, for the harness (contract v20, W3.1;
the 2026-10-11 ruling: a currency the master holds is pulled, never minted).

It holds a handful of currencies, each by its ISO 4217 code, and answers a
resolve by that code with its record; anything else, that it holds nothing.
`HARNESS_PLATFORM_SLOW` names codes it answers late, `JPY=4` four seconds
after it is asked, so a test can show a slow master mints nothing.
Standard library only; it reads no assertion, as the harness's deployment is
registered nowhere."""
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


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        asked = urllib.parse.parse_qs(url.query)
        one = lambda name: asked.get(name, [""])[0]
        code = one("value").upper()
        if (url.path == "/api/v1/reference/identifiers/resolve"
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
