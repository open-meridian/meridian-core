"""A plugin that serves a page, and says what reached it.

Registers with its sidecar declaring a page on loopback, the way a plugin does
(W6.9), then answers every request with what it was told: the claims in the
one Meridian-Caller its sidecar forwarded, and whether anything else claiming
to say who is asking got through. It verifies nothing -- that is the
sidecar's, and the point of the run is that the plugin never has to.

Runs in the SDK's image, in the sidecar's network namespace, as a plugin runs
in its sidecar's pod.
"""
import base64
import http.server
import json
import os
import time

import grpc

from meridian.v1 import sidecar_pb2, sidecar_pb2_grpc

PORT = 8000


def register():
    stub = sidecar_pb2_grpc.SidecarServiceStub(
        grpc.insecure_channel(os.environ.get("MERIDIAN_SIDECAR_ADDRESS", "127.0.0.1:9191"))
    )
    request = sidecar_pb2.RegisterRequest(
        schema_version="v1",
        interface=sidecar_pb2.InterfaceDeclaration(loopback_port=PORT, title="Plugin page"),
    )
    for _ in range(60):
        try:
            reply = stub.Register(request, timeout=2)
        except grpc.RpcError:
            time.sleep(1)
            continue
        if not reply.admitted:
            raise SystemExit(f"refused: {reply.refusal_reason}")
        print("registered, serving a page on loopback", flush=True)
        return
    raise SystemExit("the sidecar never answered")


def decoded(header):
    padded = header + "=" * (-len(header) % 4)
    assertion = sidecar_pb2.CallerAssertion.FromString(base64.urlsafe_b64decode(padded))
    claims = sidecar_pb2.CallerClaims.FromString(assertion.claims)
    return {
        "key_id": assertion.key_id,
        "subject": claims.subject,
        "display_name": claims.display_name,
        "audience": claims.audience_instance_id,
        "lifetime_ns": claims.expires_at_ns - claims.issued_at_ns,
        "assertion_id": claims.assertion_id,
        "access": [
            {"tag": held.tag, "read": list(held.read_account_ids), "write": list(held.write_account_ids)}
            for held in claims.access
        ],
    }


class Page(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        callers = self.headers.get_all("Meridian-Caller") or []
        seen = {
            "path": self.path,
            "callers": len(callers),
            "caller": decoded(callers[0]) if callers else None,
            # Handed back so the runner can try presenting it a second time.
            "raw": callers[0] if callers else None,
            "cookie": self.headers.get("Cookie"),
            "authorization": self.headers.get("Authorization"),
        }
        body = json.dumps(seen).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Set-Cookie", "meridian_session=chosen-by-the-plugin; Path=/")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    register()
    http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Page).serve_forever()
