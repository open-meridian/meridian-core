"""The stand-in as an operations plugin whose autonomous work runs as its
role's service account, built at contract v21 (STAND_IN_SERVICE_ACCOUNT;
`make e2e-service-account`; plans/a-service-account-runs-a-plugins-
autonomous-work, "What the tests prove").

What an SDK's `meridian.runs_as(role)` does, done by hand on the wire so
core's run proves the deployment's half and not an SDK's: it registers at
v21 holding `operations`, its page and these acts (the plugin's own names,
as sample operations' are in the fixtures):

- `close_cleared_breaks` (POST /tool/close-cleared), checked by
  `check_close_cleared_breaks` (POST /tool/check-close-cleared): closes a
  break as cleared in the book (W9.7), for whoever the request's assertion
  names -- a person making a prepared change, or the service account the
  dashboard admitted -- citing the statement where the difference was gone;
- `record_sweep_note` (POST /tool/sweep-note), checked by
  `check_sweep_note` (POST /tool/check-sweep-note): changes nothing in the
  deployment, answered made, for the levels and the gate to stand at;
- `note_without_check` (POST /tool/no-check): names no check tool, so it
  cannot be prepared;
- `save_own_settings_as_it` (POST /tool/own-settings): no check tool; its
  route saves a person's own settings with the assertion it was handed --
  the deployment's configuration -- which the sidecar refuses a service
  account, whatever it holds, and answers what it was told.

And the work itself: POST /sweep (tool, and the act's fields) starts it.
The request is a test's way to say "now", and nothing of it is carried: the
plugin calls `SidecarService.CallAsServiceAccount` as itself, with no
`meridian-caller`, naming the role, the tool, the arguments as JSON and a
reason, and answers what its sidecar said: the outcome, the prepared
change's ID, the act's own answer, or the refusal's status, its words and
the fields it names.
"""
import base64
import json
import os

import grpc

from meridian.plugin.v1 import operations_pb2, operations_pb2_grpc
from meridian.v1 import sidecar_pb2

SIDECAR = os.environ.get("MERIDIAN_SIDECAR_ADDRESS", "127.0.0.1:9191")
ROLE = "operations"
REFUSAL = "meridian-refusal-bin"

# route: (name, title, reads, checked_by)
ACTS = {
    "/tool/close-cleared": ("close_cleared_breaks", "Close breaks whose difference has gone", False,
                            "check_close_cleared_breaks"),
    "/tool/check-close-cleared": ("check_close_cleared_breaks", "Check closing breaks as cleared",
                                  True, ""),
    "/tool/sweep-note": ("record_sweep_note", "Record the sweep's note", False, "check_sweep_note"),
    "/tool/check-sweep-note": ("check_sweep_note", "Check the sweep's note", True, ""),
    "/tool/no-check": ("note_without_check", "Note something, with no check", False, ""),
    "/tool/own-settings": ("save_own_settings_as_it", "Save its own settings", False, ""),
}


def registration(port):
    """What a plugin built at v21 holding operations sends: its page, and
    its acts, each naming its role, the checked ones their check tool and
    the account their input names."""
    write, read = sidecar_pb2.ACCESS_LEVEL_WRITE, sidecar_pb2.ACCESS_LEVEL_READ
    schema = json.dumps({"type": "object", "properties": {
        "account_id": {"type": "string"},
        "break_ids": {"type": "array", "items": {"type": "string"}},
        "statement_id": {"type": "string"},
        "as_of_date": {"type": "string"},
        "note": {"type": "string"}}})
    tools = []
    for path, (name, title, reads, checked_by) in ACTS.items():
        tools.append(sidecar_pb2.ToolDeclaration(
            name=name, title=title, description=f"{title}, as the plugin's work or for a person.",
            method="POST", path=path, levels=[write] if not reads else [write, read],
            input_schema=schema, output_schema=json.dumps({"type": "object"}) if reads else "",
            roles=[ROLE], reads=reads, checked_by=checked_by,
            accounts=["account_id"] if name in ("close_cleared_breaks", "check_close_cleared_breaks")
            else []))
    return sidecar_pb2.RegisterRequest(
        schema_version="v21",
        interface=sidecar_pb2.InterfaceDeclaration(
            loopback_port=port, title="Reconciliation",
            pages=[sidecar_pb2.PageDeclaration(path="/", title="Reconciliation",
                                               levels=[write, read], roles=[ROLE])]),
        tools=tools).SerializeToString()


def assertion_of(header):
    padded = header + "=" * (-len(header) % 4)
    return sidecar_pb2.CallerAssertion.FromString(base64.urlsafe_b64decode(padded))


def refused_typed(refused):
    return {"outcome": "refused", "reason": refused.code().name.lower(), "fields": [],
            "detail": refused.details()}


def close_cleared(header, asked):
    """CloseBreaksAsCleared for whoever the assertion names (W9.7)."""
    ops = operations_pb2_grpc.PluginOperationsStub(grpc.insecure_channel(SIDECAR))
    try:
        closed = ops.CloseBreaksAsCleared(
            operations_pb2.CloseBreaksAsClearedParams(
                account_id=asked.get("account_id", ""),
                break_ids=asked.get("break_ids", []),
                cleared_at=operations_pb2.StreetRecordRef(
                    statement_id=asked.get("statement_id", ""),
                    as_of_date=asked.get("as_of_date", "")),
                reason="The difference was gone at the next statement",
                idempotency_key="cleared:" + ",".join(asked.get("break_ids", [])),
                acting_for=assertion_of(header)),
            timeout=10)
    except grpc.RpcError as refused:
        return refused_typed(refused)
    return {"outcome": "made", "data": {"entry_id": closed.entry.entry_id}}


def own_settings_as_it(header):
    """A person's own settings saved with the assertion handed -- the
    deployment's configuration, which the sidecar refuses a service account,
    whatever it holds (W4.9)."""
    from meridian.v1 import sidecar_pb2_grpc
    stub = sidecar_pb2_grpc.SidecarServiceStub(grpc.insecure_channel(SIDECAR))
    try:
        stub.SetOwnSettings(sidecar_pb2.SetOwnSettingsRequest(),
                            metadata=[("meridian-caller", header)], timeout=10)
    except grpc.RpcError as refused:
        return refused_typed(refused)
    return {"outcome": "made", "data": {"saved": True}}


def put_varint(value):
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def text_field(number, text):
    data = text.encode()
    return put_varint(number << 3 | 2) + put_varint(len(data)) + data


def read_strings(data):
    said, at = {}, 0
    while at < len(data):
        tag, at = varint_at(data, at)
        number, kind = tag >> 3, tag & 7
        if kind == 0:
            _, at = varint_at(data, at)
            continue
        length, at = varint_at(data, at)
        said.setdefault(number, []).append(data[at:at + length])
        at += length
    return said


def varint_at(data, at):
    value, shift = 0, 0
    while True:
        byte = data[at]
        at += 1
        value |= (byte & 0x7F) << shift
        shift += 7
        if not byte & 0x80:
            return value, at


def sweep(form):
    """The plugin's autonomous work: one of its acts called as its role's
    service account, through its sidecar, as itself (W4.15). The request
    and the reply written on the wire by hand -- CallAsServiceAccountRequest
    is role 1, tool_name 2, arguments_json 3, reason 4; the reply outcome 1,
    prepared_call_id 2, answer_json 3 -- so it does not matter which
    bindings the SDK's image carries."""
    tool = form.pop("tool", "close_cleared_breaks")
    reason = form.pop("reason", "The sweep found the difference gone")
    role = form.pop("role", ROLE)
    arguments = {k: v for k, v in form.items() if k != "csrf"}
    if "break_id" in arguments:
        arguments["break_ids"] = [arguments.pop("break_id")]
    request = (text_field(1, role) + text_field(2, tool)
               + text_field(3, json.dumps(arguments)) + text_field(4, reason))
    call = grpc.insecure_channel(SIDECAR).unary_unary(
        "/meridian.v1.SidecarService/CallAsServiceAccount",
        request_serializer=lambda raw: raw,
        response_deserializer=lambda raw: raw,
    )
    try:
        reply = call(request, timeout=60)
    except grpc.RpcError as refused:
        fields = []
        for key, value in refused.trailing_metadata() or ():
            if key == REFUSAL:
                fields = [f.decode() for f in read_strings(value).get(2, [])]
        return {"ok": False, "code": refused.code().name, "detail": refused.details(),
                "fields": fields}
    said = read_strings(reply)
    first = lambda n: said.get(n, [b""])[0].decode()  # noqa: E731
    return {"ok": True, "outcome": first(1), "prepared_call_id": first(2), "answer": first(3)}


def post(path, body, header):
    """A POST this module answers, or None."""
    if path == "/sweep":
        import urllib.parse
        form = {name: values[0] for name, values in urllib.parse.parse_qs(body.decode()).items()}
        return sweep(form)
    if path not in ACTS:
        return None
    name = ACTS[path][0]
    asked = json.loads(body or b"{}")
    if not header:
        return {"outcome": "refused", "reason": "unauthenticated", "fields": [],
                "detail": "an act is made for whoever the request's assertion names"}
    if name == "check_close_cleared_breaks":
        if not asked.get("break_ids"):
            return {"outcome": "refused", "reason": "invalid_argument",
                    "fields": [{"path": "break_ids"}], "detail": "break_ids: names no break"}
        return {"outcome": "unchanged",
                "detail": f"would close {len(asked['break_ids'])} break(s) as cleared"}
    if name == "check_sweep_note":
        return {"outcome": "unchanged", "detail": "the note would be kept"}
    if name == "close_cleared_breaks":
        return close_cleared(header, asked)
    if name == "save_own_settings_as_it":
        return own_settings_as_it(header)
    return {"outcome": "made", "data": {"noted": asked.get("note", "")}}
