"""The one platform call redeeming a claim code makes, answered for the test.

POST /api/v1/reference/deployments/claim-codes/redeem with {"code": "..."} and
`Authorization: Meridian <note>`, as crates/conductor/src/platform.rs sends it.
One known code is accepted once; anything else is refused with a reason, as
the platform would. The note's claims are checked (issuer, subject, audience,
expiry) but not its signature: this stand-in holds no registered public key,
and the platform's verification is the platform's to test.

GET /e2e/redemptions lists every redemption attempt, for the runner's
evidence. Everything else is 404, which the conductor's periodic reporting
logs and carries on past.
"""
import base64
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CODE = os.environ["E2E_CLAIM_CODE"]
DEPLOYMENT = os.environ["E2E_DEPLOYMENT_ID"]
ADDRESS = os.environ["E2E_PLATFORM_ADDRESS"]
REDEEM = "/api/v1/reference/deployments/claim-codes/redeem"

lock = threading.Lock()
spent = False
attempts = []


def note_claims(header):
    """The note's claims, or the reason it is unacceptable."""
    if not header or not header.startswith("Meridian "):
        return None, "no Meridian assertion"
    parts = header[len("Meridian "):].split(".")
    if len(parts) != 3:
        return None, "the assertion is not a signed note"
    try:
        claims = json.loads(base64.urlsafe_b64decode(parts[1] + "=" * (-len(parts[1]) % 4)))
    except ValueError:
        return None, "the assertion's claims do not decode"
    now = time.time()
    if claims.get("iss") != DEPLOYMENT or claims.get("sub") != DEPLOYMENT:
        return None, f"the assertion names {claims.get('iss')!r}, not {DEPLOYMENT!r}"
    if claims.get("aud") != ADDRESS:
        return None, f"the assertion is for {claims.get('aud')!r}, not {ADDRESS!r}"
    if not claims.get("exp") or claims["exp"] < now:
        return None, "the assertion has expired"
    return claims, None


class Handler(BaseHTTPRequestHandler):
    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/e2e/redemptions":
            with lock:
                return self.reply(200, {"attempts": attempts})
        self.reply(404, {"detail": "not served by the stand-in platform"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        if self.path != REDEEM:
            return self.reply(404, {"detail": "not served by the stand-in platform"})
        claims, refusal = note_claims(self.headers.get("Authorization"))
        if refusal:
            with lock:
                attempts.append({"code": None, "status": 401, "reason": refusal})
            return self.reply(401, {"detail": refusal})
        try:
            code = json.loads(raw)["code"]
        except (ValueError, KeyError, TypeError):
            return self.reply(400, {"detail": "the body is not {\"code\": ...}"})
        global spent
        with lock:
            if code != CODE:
                answer = {"redeemed": False, "refusal_reason": "no such claim code"}
            elif spent:
                answer = {"redeemed": False, "refusal_reason": "this claim code has already been used"}
            else:
                spent = True
                answer = {"redeemed": True}
            attempts.append({"code": code, "status": 200, "deployment": claims["iss"],
                             "body_keys": sorted(json.loads(raw).keys()), **answer})
        self.reply(200, answer)

    def log_message(self, fmt, *args):
        print("fake-platform:", fmt % args, flush=True)


ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
