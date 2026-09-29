"""A plugin that serves a page, and says what reached it.

Registers with its sidecar declaring a page on loopback, the way a plugin does
(W6.9), then answers every request with what it was told: the claims in the
one Meridian-Caller its sidecar forwarded, and whether anything else claiming
to say who is asking got through. It verifies nothing -- that is the
sidecar's, and the point of the run is that the plugin never has to.

A POST to /write records a holding for the person the request came from
(W4.9): the header it was handed, handed back on the command, so the sidecar
decides whether that person may write the account.

A POST to /report says, as the plugin itself, which accounts its connection
reaches (W2.8) and why the linked one is not current (W2.1): the dashboard
counts the unlinked on its health, and shows the sync state with what to do.

Its admin pages link them (W6.4), as a plugin's own admin page does: a GET of
/accounts reads the deployment's accounts, and a POST to /link sends a link
-- to an existing account, a new one named, or neither to remove it -- each
acting for the person the request came from, whom the sidecar admits only
when they administer the deployment. /link with "as_itself" sends it as the
plugin, which the sidecar refuses. It declares three admin pages, which the
dashboard's admin view of it shows as tabs (W4.8, W6.9); a GET of one is a
small page on the UI kit saying which it is and who asked, and the Accounts
page what it would offer to link.

It declares two settings at registration (W4.1): a required secret, the way a
venue's API key is, and a number. It watches them on the stream its sidecar
serves (W4.7), and a GET of /settings says what it holds: the names, what is
still missing, and a digest of the secret -- never the secret, which a page
must not carry -- and when this process started and how often it registered,
so the runner can tell it was not restarted to get it.

Runs in the SDK's image, in the sidecar's network namespace, as a plugin runs
in its sidecar's pod.
"""
import base64
import hashlib
import http.server
import json
import os
import threading
import time
import uuid

import grpc

from meridian.plugin.v1 import operations_pb2, operations_pb2_grpc
from meridian.v1 import sidecar_pb2, sidecar_pb2_grpc

PORT = 8000
SIDECAR = os.environ.get("MERIDIAN_SIDECAR_ADDRESS", "127.0.0.1:9191")
EXTERNAL_ACCOUNT = "ext-e2e"
# Reached and never linked, so it stays on the dashboard's list.
OTHER_ACCOUNT = "ext-e2e-roth"

STARTED_AT_NS = time.time_ns()
# Its admin pages, in the order the dashboard's admin view shows them as tabs.
ADMIN_PAGES = [
    sidecar_pb2.PageDeclaration(path="/admin/connections", title="Connections"),
    sidecar_pb2.PageDeclaration(path="/admin/accounts", title="Accounts"),
    sidecar_pb2.PageDeclaration(path="/admin/holdings", title="Holdings"),
]
DECLARED = [
    sidecar_pb2.SettingDeclaration(
        name="api_key", type=sidecar_pb2.SETTING_TYPE_STRING, required=True, secret=True,
        description="The venue's API key."),
    sidecar_pb2.SettingDeclaration(
        name="poll_minutes", type=sidecar_pb2.SETTING_TYPE_INTEGER,
        description="How often to read the venue, in minutes."),
]
# What the settings stream last delivered, as this plugin would say it.
HELD = {"deliveries": 0, "values": {}, "missing_required": [], "registrations": 0}
HELD_LOCK = threading.Lock()


def register():
    stub = sidecar_pb2_grpc.SidecarServiceStub(grpc.insecure_channel(SIDECAR))
    request = sidecar_pb2.RegisterRequest(
        schema_version="v2",
        interface=sidecar_pb2.InterfaceDeclaration(
            loopback_port=PORT, title="Plugin page", admin_pages=ADMIN_PAGES),
        settings=DECLARED,
    )
    for _ in range(60):
        try:
            reply = stub.Register(request, timeout=2)
        except grpc.RpcError:
            time.sleep(1)
            continue
        if not reply.admitted:
            raise SystemExit(f"refused: {reply.refusal_reason}")
        with HELD_LOCK:
            HELD["registrations"] += 1
        print("registered, serving a page on loopback", flush=True)
        return
    raise SystemExit("the sidecar never answered")


def watch_settings():
    """Hold what the sidecar delivers, for as long as the process runs."""
    stub = sidecar_pb2_grpc.SidecarServiceStub(grpc.insecure_channel(SIDECAR))
    while True:
        try:
            for delivery in stub.WatchSettings(sidecar_pb2.WatchSettingsRequest()):
                with HELD_LOCK:
                    HELD["deliveries"] += 1
                    HELD["values"] = {v.name: v.value for v in delivery.values}
                    HELD["missing_required"] = list(delivery.missing_required)
        except grpc.RpcError:
            time.sleep(1)


def settings_said():
    """The settings as this plugin holds them, fit for a page: a secret by its
    digest alone."""
    with HELD_LOCK:
        values = dict(HELD["values"])
        said = {k: HELD[k] for k in ("deliveries", "missing_required", "registrations")}
    secret = values.pop("api_key", None)
    said["names"] = sorted(values) + (["api_key"] if secret is not None else [])
    said["values"] = values
    said["api_key_sha256"] = hashlib.sha256(secret.encode()).hexdigest() if secret else None
    said["started_at_ns"] = STARTED_AT_NS
    return said


def assertion_of(header):
    padded = header + "=" * (-len(header) % 4)
    return sidecar_pb2.CallerAssertion.FromString(base64.urlsafe_b64decode(padded))


def write_for(header):
    """A statement of one holding, sent for the person the header names."""
    ops = operations_pb2_grpc.PluginOperationsStub(grpc.insecure_channel(SIDECAR))
    person = assertion_of(header)
    try:
        opened = ops.RecordHoldingsStatement(
            operations_pb2.RecordHoldingsStatementParams(
                source="e2e", external_statement_id=str(uuid.uuid4()),
                as_of_date="2026-09-26", read_at_ns=time.time_ns(), expected_rows=1,
                acting_for=person),
            timeout=10)
        held = ops.RecordHolding(
            operations_pb2.RecordHoldingParams(
                statement_id=opened.statement_id,
                unresolved_identifiers=[operations_pb2.Identifier(
                    scheme="symbol", value="E2E", source="e2e")],
                # One unit worth 1 USD: an integer and its scale (decisions/023).
                side=operations_pb2.HOLDING_SIDE_LONG,
                quantity=operations_pb2.Decimal(low=1),
                market_value=operations_pb2.Money(
                    amount=operations_pb2.Decimal(low=1), currency_code="USD"),
                external_account_id=EXTERNAL_ACCOUNT, acting_for=person),
            timeout=10)
    except grpc.RpcError as refused:
        return {"ok": False, "code": refused.code().name, "detail": refused.details()}
    return {"ok": True, "holding_id": held.holding_id}


def report():
    """The accounts the connection reaches, and the linked one's sync state.

    Neither is refused for want of a link: saying which accounts there are is
    how one gets linked, and a sync state describes the connection, not data
    recorded against the account (ruled 2026-09-28)."""
    ops = operations_pb2_grpc.PluginOperationsStub(grpc.insecure_channel(SIDECAR))
    said = {}
    try:
        ops.ReportExternalAccounts(
            operations_pb2.ReportExternalAccountsParams(accounts=[
                operations_pb2.ExternalAccount(
                    external_account_id=EXTERNAL_ACCOUNT, name="E2E Brokerage",
                    venue_account_type="Individual"),
                operations_pb2.ExternalAccount(
                    external_account_id=OTHER_ACCOUNT, name="E2E Roth",
                    venue_account_type="Roth IRA"),
            ]),
            timeout=10)
        said["accounts"] = "published"
    except grpc.RpcError as refused:
        said["accounts"] = f"{refused.code().name}: {refused.details()}"
    try:
        ops.ReportSyncStatus(
            operations_pb2.ReportSyncStatusParams(
                source="e2e", external_account_id=EXTERNAL_ACCOUNT,
                state=operations_pb2.SYNC_STATE_NEEDS_SIGN_IN, connection_healthy=False,
                status_detail="the daily sign-in has lapsed",
                holdings_as_of_ns=1_790_380_800_000_000_000,
                observed_at_ns=time.time_ns()),
            timeout=10)
        said["sync"] = "published"
    except grpc.RpcError as refused:
        said["sync"] = f"{refused.code().name}: {refused.details()}"
    return said


def refused_as(refused):
    return {"ok": False, "code": refused.code().name, "detail": refused.details()}


def accounts_for(header):
    """The deployment's accounts, read for the person the header names."""
    ops = operations_pb2_grpc.PluginOperationsStub(grpc.insecure_channel(SIDECAR))
    try:
        read = ops.ReadAccountsForLinking(
            operations_pb2.ReadAccountsForLinkingParams(acting_for=assertion_of(header)),
            timeout=10)
    except grpc.RpcError as refused:
        return refused_as(refused)
    return {"ok": True, "accounts": [
        {"account_id": a.account_id, "name": a.name, "state": a.state,
         "custodian": a.custodian, "account_type": a.account_type, "owner": a.owner,
         "note": a.note} for a in read.accounts]}


def link_for(header, asked):
    """A link as the plugin's admin page sends it, for the person the header
    names, or as the plugin itself when asked to."""
    ops = operations_pb2_grpc.PluginOperationsStub(grpc.insecure_channel(SIDECAR))
    params = operations_pb2.LinkExternalAccountParams(
        external_account_id=asked.get("external_account_id", ""),
        account_id=asked.get("account_id", ""),
        new_account_name=asked.get("new_account_name", ""),
        new_account_custodian=asked.get("new_account_custodian", ""),
        new_account_type=asked.get("new_account_type", ""),
        new_account_owner=asked.get("new_account_owner", ""),
        new_account_note=asked.get("new_account_note", ""))
    if not asked.get("as_itself"):
        params.acting_for.CopyFrom(assertion_of(header))
    try:
        linked = ops.LinkExternalAccount(params, timeout=10)
    except grpc.RpcError as refused:
        return refused_as(refused)
    return {"ok": True, "account_id": linked.account_id,
            "plugin_instance_id": linked.plugin_instance_id}


KIT = "/.meridian/ui/0.1.0/meridian.css"


def admin_page(path, header):
    """One of its admin pages, on the kit: which it is, for whom, and on the
    Accounts page what it reaches and the deployment's accounts it offers."""
    import html
    title = next(page.title for page in ADMIN_PAGES if page.path == path)
    caller = decoded(header) if header else {}
    rows = ""
    if path == "/admin/accounts":
        read = accounts_for(header) if header else {"ok": False}
        offered = ", ".join(html.escape(a["name"]) for a in read.get("accounts", [])) or "none"
        rows = "".join(
            f"<tr><td>{html.escape(external)}</td><td>{html.escape(name)}</td><td>{offered}</td></tr>"
            for external, name in ((EXTERNAL_ACCOUNT, "E2E Brokerage"), (OTHER_ACCOUNT, "E2E Roth")))
        rows = ("<table><thead><tr><th>External account</th><th>At the venue</th>"
                f"<th>Could link to</th></tr></thead><tbody>{rows}</tbody></table>")
    return (f"<!doctype html><html><head><meta charset=\"utf-8\"><link rel=\"stylesheet\" href=\"{KIT}\">"
            f"<title>{html.escape(title)}</title></head><body><main class=\"page\">"
            f"<h1>{html.escape(title)}</h1><p>A stand-in plugin's admin page, <code>{html.escape(path)}</code>, "
            f"served to {html.escape(caller.get('display_name', 'nobody'))}, deployment admin: "
            f"{'yes' if caller.get('deployment_admin') else 'no'}.</p>{rows}</main></body></html>")


def decoded(header):
    assertion = assertion_of(header)
    claims = sidecar_pb2.CallerClaims.FromString(assertion.claims)
    return {
        "key_id": assertion.key_id,
        "subject": claims.subject,
        "display_name": claims.display_name,
        "audience": claims.audience_instance_id,
        "lifetime_ns": claims.expires_at_ns - claims.issued_at_ns,
        "assertion_id": claims.assertion_id,
        "deployment_admin": claims.deployment_admin,
        "access": {"read": list(claims.read_account_ids), "write": list(claims.write_account_ids)},
    }


class Page(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        callers = self.headers.get_all("Meridian-Caller") or []
        if self.path in ("/settings", "/accounts"):
            said = settings_said() if self.path == "/settings" else accounts_for(callers[0])
            body = json.dumps(said).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.split("?", 1)[0] in {page.path for page in ADMIN_PAGES}:
            body = admin_page(self.path.split("?", 1)[0], callers[0] if callers else None).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
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

    def do_POST(self):
        callers = self.headers.get_all("Meridian-Caller") or []
        sent = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if self.path == "/report":
            done = report()
        elif self.path == "/link":
            done = link_for(callers[0], json.loads(sent or b"{}"))
        else:
            done = write_for(callers[0]) if callers else {"ok": False, "detail": "nobody"}
        body = json.dumps(done).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args):
        pass


if __name__ == "__main__":
    register()
    threading.Thread(target=watch_settings, daemon=True).start()
    http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Page).serve_forever()
