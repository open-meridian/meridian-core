"""A firm's own OpenID Connect provider, standing in for one.

Decision 018: where a firm has a provider, the dashboard federates to it and
nothing of ours signs anybody in. What this stands in for is therefore
somebody else's software, which is exactly what a stand-in should be -- and it
has to outlive the bundled Zitadel, which is being deleted and which the
OpenID Connect cases used to borrow as a provider.

Signed with HS256, keyed by the client secret. That is a real, specified
alternative to a JWKS and the only one this can do: nothing in these images
has an asymmetric crypto library, and `hmac` is in the standard library. The
dashboard verifies it because a client configured with a secret is a
confidential client, and a confidential client's verifier accepts the
symmetric algorithms.

Endpoints beyond the protocol's, all under /e2e:
  POST /e2e/stale      the next ID token claims a sign-in from an hour ago
  POST /e2e/fresh      undo that
  GET  /e2e/issued     what was handed out, for a runner to assert on
"""
import base64
import hashlib
import hmac
import json
import os
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ISSUER = os.environ.get("E2E_IDP_ISSUER", "http://fake-idp:8100")
CLIENT_ID = os.environ.get("E2E_IDP_CLIENT_ID", "meridian-dashboard")
CLIENT_SECRET = os.environ.get("E2E_IDP_CLIENT_SECRET", "idp-dev-only-secret")
PORT = int(os.environ.get("E2E_IDP_PORT", "8100"))

# Whoever signs in. One person is enough: what these cases are about is the
# dashboard's handling of a token, not the directory's idea of people.
SUBJECT = os.environ.get("E2E_IDP_SUBJECT", "8812")
NAME = os.environ.get("E2E_IDP_NAME", "Ada Park")
GROUPS = os.environ.get("E2E_IDP_GROUPS", "meridian-admins").split(",")

STATE = {"stale": False, "issued": [], "pending": {}}
LOCK = threading.Lock()


def b64(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def jwt(claims):
    header = {"alg": "HS256", "typ": "JWT"}
    signing = f"{b64(json.dumps(header).encode())}.{b64(json.dumps(claims).encode())}"
    signature = hmac.new(CLIENT_SECRET.encode(), signing.encode(), hashlib.sha256).digest()
    return f"{signing}.{b64(signature)}"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, headers=None):
        raw = json.dumps(body).encode() if not isinstance(body, bytes) else body
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        path, _, query = self.path.partition("?")
        fields = urllib.parse.parse_qs(query)

        if path == "/.well-known/openid-configuration":
            return self.reply(200, {
                "issuer": ISSUER,
                "authorization_endpoint": f"{ISSUER}/authorize",
                "token_endpoint": f"{ISSUER}/token",
                "jwks_uri": f"{ISSUER}/jwks",
                "response_types_supported": ["code"],
                "subject_types_supported": ["public"],
                # HS256, keyed by the client secret. No JWKS to publish.
                "id_token_signing_alg_values_supported": ["HS256"],
                "scopes_supported": ["openid", "profile", "email"],
                "claims_supported": ["sub", "iss", "aud", "exp", "iat", "auth_time",
                                     "nonce", "name", "email", "groups"],
                "token_endpoint_auth_methods_supported": ["client_secret_post",
                                                          "client_secret_basic"],
            })

        if path == "/jwks":
            # Empty, and correct: nothing is signed asymmetrically here.
            return self.reply(200, {"keys": []})

        if path == "/authorize":
            code = base64.urlsafe_b64encode(os.urandom(12)).rstrip(b"=").decode()
            with LOCK:
                STATE["pending"][code] = {
                    "nonce": (fields.get("nonce") or [""])[0],
                    "stale": STATE["stale"],
                }
            back = (fields.get("redirect_uri") or [""])[0]
            state = (fields.get("state") or [""])[0]
            where = f"{back}?code={urllib.parse.quote(code)}&state={urllib.parse.quote(state)}"
            self.send_response(302)
            self.send_header("Location", where)
            self.end_headers()
            return None

        if path == "/e2e/issued":
            with LOCK:
                return self.reply(200, {"issued": STATE["issued"]})

        return self.reply(404, {"error": "not found"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length).decode()
        fields = urllib.parse.parse_qs(raw)
        path = self.path.partition("?")[0]

        if path == "/e2e/stale":
            with LOCK:
                STATE["stale"] = True
            return self.reply(200, {"stale": True})

        if path == "/e2e/fresh":
            with LOCK:
                STATE["stale"] = False
            return self.reply(200, {"stale": False})

        if path == "/token":
            code = (fields.get("code") or [""])[0]
            with LOCK:
                pending = STATE["pending"].pop(code, None)
            if pending is None:
                return self.reply(400, {"error": "invalid_grant"})

            now = int(time.time())
            # The one thing these cases turn on: when the person actually
            # authenticated, as against when the token was minted. A provider
            # with its own session re-issues a token without asking anybody
            # anything, and `auth_time` is what says so.
            authenticated_at = now - 3600 if pending["stale"] else now
            claims = {
                "iss": ISSUER,
                "sub": SUBJECT,
                "aud": CLIENT_ID,
                "exp": now + 300,
                "iat": now,
                "auth_time": authenticated_at,
                "nonce": pending["nonce"],
                "name": NAME,
                "email": f"{SUBJECT}@example.org",
                "groups": GROUPS,
            }
            with LOCK:
                STATE["issued"].append({"auth_time": authenticated_at, "iat": now})
            return self.reply(200, {
                "access_token": "e2e-access-token",
                "token_type": "Bearer",
                "expires_in": 300,
                "id_token": jwt(claims),
            })

        return self.reply(404, {"error": "not found"})


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
