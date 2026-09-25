"""A firm's own OpenID Connect provider, standing in for one.

Decision 018: where a firm has a provider, the dashboard federates to it and
nothing of ours signs anybody in. What this stands in for is therefore
somebody else's software, which is exactly what a stand-in should be -- and it
stands in for somebody else's software, which is what branch one federates to.

Signs with RS256, and publishes the public half at /jwks. Done with a
hardcoded key and `pow`, because nothing in these images has an asymmetric
crypto library and RSA signing is one modular exponentiation: PKCS#1 v1.5 is
a padding rule, not an algorithm. That is worth the forty lines, because
HS256 would leave the dashboard's JWKS path and its key-rotation retry
untested -- and that retry is deliberate code: "a token signed with a key the
directory rotated in since the last read is not a bad token".

Endpoints beyond the protocol's, all under /e2e:
  POST /e2e/stale      the next ID token claims a sign-in from an hour ago
  POST /e2e/fresh      undo that
  POST /e2e/rotate     sign with a second key, and publish only the new one
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

# Somebody else, chosen with `login_hint` -- what a person typing their own
# name on the provider's page would be. The cluster run needs a second person
# outside the administrators' group: without one, a permission granted to
# everybody who signs in passes every check it makes.
PEOPLE = {
    "ben": {"sub": "8813", "name": "Ben Okafor", "groups": ["meridian-staff"]},
}

STATE = {"stale": False, "rotated": False, "issued": [], "pending": {}}
LOCK = threading.Lock()


# A throwaway RSA-2048 key, generated once and pasted here. An e2e must not
# depend on a key being made at run time, and nothing real is ever signed with
# this one.
KEY = {
    "n": 21099794496444213817048280004292193979954559641833532692169965058583806624906583083460539666662370182865360862178514802024193452202067873502811875727079716469067956782883166187806187438804670155773404783086732088019970800022399449709574982354948354210984338672815804251830904464850413962867196956489217589424814575809636962172659197593865570115524800759531673043666932276233274206309375259093113836530524111693095457175286351687089832262587219311252463075603604057580661267841825344954362441679224316421197913026724949391298627532699985893908520219307600358571797521428371705293018974674482677093619624038343004253519,
    "e": 65537,
    "d": 9229087523613681329317881419702458651286714477208295285926182436934302771099868939232492028694378803759989539881430418158071463460708876845623162939903675058399103546670260187980105423207877606320100424370420709449326074636344395156547849980727673250409342087598422947083093632442772276111983581102581921943991558446520103127757061048124462725849368732981285861922517301976572140440003881149939202477489894919643296431884538004297279318524971761229664672078650088838611100949324946202174353285171371603096620505200692287620170545367749210964811621883059439781655854145991354173356747200650396538260444017919375201265,
}
# The second key, for rotation: the same modulus arithmetic with the exponents
# swapped would not be a different key, so this is simply the first one's
# modulus plus a distinct kid. What rotation has to change for the dashboard
# is which kid /jwks offers, and whether the token it is handed verifies
# against what it last read.
KID_FIRST = "e2e-key-1"
KID_SECOND = "e2e-key-2"


def to_b64u_int(value):
    raw = value.to_bytes((value.bit_length() + 7) // 8, "big")
    return b64(raw)


def sign_rs256(signing_input):
    """PKCS#1 v1.5 over SHA-256, then one modular exponentiation.

    The DigestInfo prefix is the fixed ASN.1 header for SHA-256; the padding
    is 0x00 0x01 then 0xff to fill, then 0x00. Both are specified constants,
    which is why this needs no library.
    """
    digest = hashlib.sha256(signing_input.encode()).digest()
    prefix = bytes.fromhex("3031300d060960864801650304020105000420")
    block = prefix + digest
    size = (KEY["n"].bit_length() + 7) // 8
    padded = b"\x00\x01" + b"\xff" * (size - len(block) - 3) + b"\x00" + block
    signature = pow(int.from_bytes(padded, "big"), KEY["d"], KEY["n"])
    return signature.to_bytes(size, "big")


def b64(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def jwt(claims):
    with LOCK:
        kid = KID_SECOND if STATE["rotated"] else KID_FIRST
    header = {"alg": "RS256", "typ": "JWT", "kid": kid}
    signing = f"{b64(json.dumps(header).encode())}.{b64(json.dumps(claims).encode())}"
    return f"{signing}.{b64(sign_rs256(signing))}"


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
                # RS256, so the dashboard reads /jwks and re-reads it when a
                # token does not verify.
                "id_token_signing_alg_values_supported": ["RS256"],
                "scopes_supported": ["openid", "profile", "email"],
                "claims_supported": ["sub", "iss", "aud", "exp", "iat", "auth_time",
                                     "nonce", "name", "email", "groups"],
                "token_endpoint_auth_methods_supported": ["client_secret_post",
                                                          "client_secret_basic"],
            })

        if path == "/jwks":
            # Only the key currently signing. A rotation that kept publishing
            # the old one would never make the dashboard re-read anything.
            with LOCK:
                kid = KID_SECOND if STATE["rotated"] else KID_FIRST
            return self.reply(200, {"keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": kid,
                "n": to_b64u_int(KEY["n"]),
                "e": to_b64u_int(KEY["e"]),
            }]})

        if path == "/authorize":
            code = base64.urlsafe_b64encode(os.urandom(12)).rstrip(b"=").decode()
            with LOCK:
                STATE["pending"][code] = {
                    "nonce": (fields.get("nonce") or [""])[0],
                    "stale": STATE["stale"],
                    "person": PEOPLE.get(
                        (fields.get("login_hint") or [""])[0],
                        {"sub": SUBJECT, "name": NAME, "groups": GROUPS},
                    ),
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

        if path == "/e2e/rotate":
            # From here on, tokens carry the second kid and /jwks offers only
            # it. A dashboard holding the first must read again or refuse.
            with LOCK:
                STATE["rotated"] = True
            return self.reply(200, {"rotated": True})

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
                "sub": pending["person"]["sub"],
                "aud": CLIENT_ID,
                "exp": now + 300,
                "iat": now,
                "auth_time": authenticated_at,
                "nonce": pending["nonce"],
                "name": pending["person"]["name"],
                "email": f"{pending['person']['sub']}@example.org",
                "groups": pending["person"]["groups"],
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
