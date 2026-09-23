"""The two platform calls a deployment makes while it is being set up.

`POST /api/v1/reference/deployments/keys/enrol` registers the key the
conductor generated, with the one-time code the install carried (W5.24), and
`POST .../claim-codes/redeem` honours a claim code for the purpose it was
issued for (W5.22). Honouring a first-run code returns the first
administrator's, issued in the same act.

The note each call carries is checked for its claims and not its signature:
this stand-in holds no registered key, and the platform's verification is the
platform's to test. `GET /e2e/calls` is the runner's evidence.
"""
import base64
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEPLOYMENT = os.environ["E2E_DEPLOYMENT_ID"]
ADDRESS = os.environ["E2E_PLATFORM_ADDRESS"]
ENROLMENT_CODE = os.environ["E2E_ENROLMENT_CODE"]
FIRST_RUN_CODE = os.environ["E2E_FIRST_RUN_CODE"]
FIRST_ADMIN_CODE = os.environ["E2E_FIRST_ADMIN_CODE"]

ENROL = "/api/v1/reference/deployments/keys/enrol"
REDEEM = "/api/v1/reference/deployments/claim-codes/redeem"

lock = threading.Lock()
calls = []
state = {"enrolled": None, "spent_codes": []}


def claims_of(header):
    if not header or not header.startswith("Meridian "):
        return None, "no Meridian assertion"
    parts = header[len("Meridian "):].split(".")
    if len(parts) != 3:
        return None, "the assertion is not a signed note"
    try:
        claims = json.loads(base64.urlsafe_b64decode(parts[1] + "=" * (-len(parts[1]) % 4)))
    except ValueError:
        return None, "the assertion's claims do not decode"
    if claims.get("iss") != DEPLOYMENT:
        return None, f"the assertion names {claims.get('iss')!r}"
    if claims.get("aud") != ADDRESS:
        return None, f"the assertion is for {claims.get('aud')!r}"
    if not claims.get("exp") or claims["exp"] < time.time():
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
        if self.path == "/e2e/calls":
            with lock:
                return self.reply(200, {"calls": calls, "state": state})
        self.reply(404, {"error": "not here"})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        _, refusal = claims_of(self.headers.get("Authorization"))

        if self.path == ENROL:
            # Signed by the key it enrols, which this cannot verify and the
            # platform can. What it does check is the code and the identifier.
            answer, status = self.enrol(body, refusal)
        elif self.path == REDEEM:
            answer, status = self.redeem(body, refusal)
        else:
            answer, status = {"error": "not here"}, 404

        with lock:
            calls.append({"path": self.path, "body_keys": sorted(body.keys()),
                          "status": status, "answer": answer})
        self.reply(status, answer)

    def enrol(self, body, refusal):
        if refusal:
            return {"error": refusal}, 401
        if body.get("deployment_id") != DEPLOYMENT:
            return {"error": "another deployment"}, 401
        if body.get("enrolment_code") != ENROLMENT_CODE:
            return {"error": "no such enrolment code"}, 403
        with lock:
            if state["enrolled"]:
                return {"error": "already used"}, 403
            state["enrolled"] = body.get("public_key_pem", "")[:64]
        return {"key_id": "KEY-e2e", "fingerprint": "SHA256:e2e-fingerprint"}, 201

    def redeem(self, body, refusal):
        if refusal:
            return {"error": refusal}, 401
        code, purpose = body.get("code"), body.get("purpose")
        with lock:
            if code in state["spent_codes"]:
                return {"redeemed": False, "refusal_reason": "already used"}, 200
            if code == FIRST_RUN_CODE:
                if purpose != "CLAIM_CODE_PURPOSE_FIRST_RUN":
                    return {"redeemed": False, "refusal_reason": "issued for another purpose"}, 200
                state["spent_codes"].append(code)
                return {"redeemed": True, "first_admin_code": FIRST_ADMIN_CODE,
                        "first_admin_code_expires_at_ns": 0}, 200
            return {"redeemed": False, "refusal_reason": "no such claim code"}, 200

    def log_message(self, fmt, *args):
        print(self.path, fmt % args, flush=True)


ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
