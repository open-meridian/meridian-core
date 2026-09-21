"""Zitadel's API, from inside the compose network, with the standard library.

Shared by the bootstrap and the runner. Adapted from meridian-design's
reference/zitadel-group-trial/zt.py.
"""
import base64
import json
import os
import time
import urllib.error
import urllib.request

BASE = os.environ.get("ZITADEL", "http://zitadel:8080")
BOOTSTRAP = "/bootstrap"


class ApiError(RuntimeError):
    def __init__(self, method, path, status, body):
        super().__init__(f"{method} {path} -> {status}: {body}")
        self.status = status
        self.body = body


def token(name):
    with open(os.path.join(BOOTSTRAP, name)) as f:
        return f.read().strip()


def call(method, path, body=None, pat="admin.pat", ok=(200, 201)):
    """One API call as the named bootstrap token; JSON in, JSON out."""
    data = None if body is None else json.dumps(body).encode()
    headers = {"Authorization": f"Bearer {token(pat)}", "Content-Type": "application/json",
               "Accept": "application/json"}
    request = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            text, status = response.read().decode(), response.status
    except urllib.error.HTTPError as failed:
        text, status = failed.read().decode(), failed.code
    try:
        parsed = json.loads(text) if text else {}
    except ValueError:
        parsed = {"_text": text}
    if status not in ok:
        raise ApiError(method, path, status, text)
    return parsed


def wait_ready(seconds=180):
    """Until Zitadel answers ready and has written the first instance's tokens."""
    deadline = time.time() + seconds
    last = None
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(BASE + "/debug/ready", timeout=5) as response:
                if response.status == 200 and all(
                        os.path.exists(os.path.join(BOOTSTRAP, n)) for n in ("admin.pat", "login-client.pat")):
                    # Ready also means the instance answers its own host name.
                    with urllib.request.urlopen(BASE + "/.well-known/openid-configuration", timeout=5):
                        return
        except (urllib.error.URLError, OSError) as failed:
            last = failed
        time.sleep(2)
    raise RuntimeError(f"Zitadel was not ready within {seconds}s: {last}")


def jwt_payload(compact):
    part = compact.split(".")[1]
    return json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
