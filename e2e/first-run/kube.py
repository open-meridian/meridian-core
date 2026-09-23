"""The Kubernetes API, as much of it as first run is allowed to use.

Five calls: read and merge-patch a Secret, patch one NetworkPolicy, patch a
Deployment and its scale, and delete a RoleBinding. Everything else is 404 and
is evidence in itself, because a Job that asked for anything else would be
asking for something its Role does not name.

Over TLS with a certificate the harness generates, because that is how a pod
reaches its own API server and the code under test verifies it. `GET
/e2e/state` is the runner's evidence: what was written, and what was deleted.
"""
import base64
import json
import re
import ssl
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

lock = threading.Lock()
secrets = {}
policies = {}
deployments = {}
deleted = []
refused = []

SECRET = re.compile(r"^/api/v1/namespaces/[^/]+/secrets/([^/?]+)")
POLICY = re.compile(r"^/apis/networking\.k8s\.io/v1/namespaces/[^/]+/networkpolicies/([^/?]+)")
SCALE = re.compile(r"^/apis/apps/v1/namespaces/[^/]+/deployments/([^/?]+)/scale")
DEPLOYMENT = re.compile(r"^/apis/apps/v1/namespaces/[^/]+/deployments/([^/?]+)")
BINDING = re.compile(r"^/apis/rbac\.authorization\.k8s\.io/v1/namespaces/[^/]+/rolebindings/([^/?]+)")


class Handler(BaseHTTPRequestHandler):
    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        return json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")

    def do_GET(self):
        if self.path == "/e2e/state":
            with lock:
                # Decoded, so the runner asserts on what a component would
                # read rather than on base64.
                return self.reply(200, {
                    "secrets": {name: {key: base64.b64decode(value).decode()
                                       for key, value in data.items()}
                                for name, data in secrets.items()},
                    "policies": policies,
                    "deployments": deployments,
                    "deleted": deleted,
                    "refused": refused,
                })
        found = SECRET.match(self.path)
        if found:
            with lock:
                return self.reply(200, {"data": secrets.get(found.group(1), {})})
        return self.not_here()

    def do_PATCH(self):
        body = self.body()
        for pattern, handler in ((SECRET, self.secret), (POLICY, self.policy),
                                 (SCALE, self.scale), (DEPLOYMENT, self.deployment)):
            found = pattern.match(self.path)
            if found:
                with lock:
                    handler(found.group(1), body)
                return self.reply(200, {"ok": True})
        return self.not_here()

    def do_DELETE(self):
        found = BINDING.match(self.path)
        if found:
            with lock:
                deleted.append(found.group(1))
            return self.reply(200, {"ok": True})
        return self.not_here()

    def secret(self, name, body):
        secrets.setdefault(name, {}).update(body.get("data") or {})

    def policy(self, name, body):
        policies[name] = body.get("spec", {}).get("egress")

    def scale(self, name, body):
        deployments.setdefault(name, {})["replicas"] = body.get("spec", {}).get("replicas")

    def deployment(self, name, body):
        annotations = (body.get("spec", {}).get("template", {})
                       .get("metadata", {}).get("annotations", {}))
        deployments.setdefault(name, {})["restarted"] = bool(annotations)

    def not_here(self):
        with lock:
            refused.append(f"{self.command} {self.path}")
        self.reply(404, {"error": "this API serves only what first run may touch"})

    def log_message(self, fmt, *args):
        print(self.command, self.path, flush=True)


# The API itself is TLS, as a pod's is. The evidence is plain, on a port of
# its own, so the runner reads what happened without holding a certificate.
evidence = ThreadingHTTPServer(("0.0.0.0", 8080), Handler)
threading.Thread(target=evidence.serve_forever, daemon=True).start()

server = ThreadingHTTPServer(("0.0.0.0", 8443), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain("/pki/tls.crt", "/pki/tls.key")
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
