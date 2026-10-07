"""The stand-in as a plugin at the edge keeping raw records, built at
contract v16 (STAND_IN_ARCHIVE; `make e2e-archive`;
plans/an-edge-plugins-older-records-move-to-the-archive).

What an SDK's helper does, done by hand so core's run proves the
deployment's half and not an SDK's: it declares two kinds of raw record,
"activity" (archivable, a window of 2,555 days) and "responses" (archivable,
30 days), and the two window settings the SDK declares for each, and seeds
its storage with four units: two months of an account's activity received
long ago, one received a few weeks ago, and a day of responses. Each
heartbeat says what its storage holds of each kind (W4.5).

Its routes, each answering what its sidecar said, as JSON:

- POST /archive (unit, and force to report a move with no archive): as the
  plugin itself, a window's move: the unit written to its archive
  (MERIDIAN_ARCHIVE_DIR), checked by size and digest, the move reported
  naming the window's setting and value, and only then removed from
  storage; with no archive, kept, unless forced to report anyway.
- POST /archive/restore (kind, unit): for the person the request came from,
  the route the SDK declares (the names' choice b): copied back to a restore
  area in storage, and the restore reported for them, carrying the
  assertion it was handed as its `meridian-caller` metadata.
- POST /archive/return (unit): as itself, after the restore period: the
  restore area's copy removed, the return reported.
- POST /archive/delete (unit, and for_person to send the header): a
  deletion reported before anything is deleted, so a refusal -- inside the
  hold, or an archived unit deleted by a window -- keeps it.
- GET /record?key=KIND/ACCOUNT/MONTH#N: what a row's raw record resolves
  to (requirement 6): in storage, archived and restorable, restored and
  readable, or deleted -- never nothing.
- GET /archive/read?unit=: a restored unit read back from the restore area.
- GET /archive/list: the index the helper keeps in storage.

Nothing of a record's content is ever in a move: a count, two times and the
unit's key.
"""
import base64
import hashlib
import json
import os
import threading
import time

import grpc

from meridian.v1 import sidecar_pb2, sidecar_pb2_grpc

SIDECAR = os.environ.get("MERIDIAN_SIDECAR_ADDRESS", "127.0.0.1:9191")
STORAGE = os.environ.get("MERIDIAN_STORAGE_DIR", "/var/lib/meridian/storage")
ARCHIVE = os.environ.get("MERIDIAN_ARCHIVE_DIR", "")
DAY_NS = 86_400 * 1_000_000_000
# name, label, default window, archivable
KINDS = [
    ("activity", "Reported activity", 2555, True),
    ("responses", "Raw responses", 30, True),
]
WINDOW = {name: window for name, _, window, _ in KINDS}
INDEX = os.path.join(STORAGE, "index.json")
LOCK = threading.Lock()
# What the settings stream last delivered: each kind's window, as its admin
# set it.
SETTINGS = {}


def registration(port):
    """What a plugin built at v16 holding custody sends: a page at every
    level, the window settings the SDK declares for each kind, and its
    storage with its kinds."""
    write, read, admin = (sidecar_pb2.ACCESS_LEVEL_WRITE, sidecar_pb2.ACCESS_LEVEL_READ,
                          sidecar_pb2.ACCESS_LEVEL_ADMIN)
    settings = []
    for name, label, window, _ in KINDS:
        settings.append(sidecar_pb2.SettingDeclaration(
            name=f"{name}_window_days", type=sidecar_pb2.SETTING_TYPE_INTEGER,
            label=f"{label}: window", unit="days", default_value=str(window),
            description=f"How long {label.lower()} stays in this plugin's storage."))
        settings.append(sidecar_pb2.SettingDeclaration(
            name=f"{name}_past_window", type=sidecar_pb2.SETTING_TYPE_CHOICE,
            label=f"{label}: past its window", default_value="kept",
            choices=[sidecar_pb2.SettingChoice(value=v, label=v.capitalize())
                     for v in ("archived", "kept", "deleted")],
            description=f"What is done with {label.lower()} past its window."))
    return sidecar_pb2.RegisterRequest(
        schema_version="v16",
        interface=sidecar_pb2.InterfaceDeclaration(
            loopback_port=port, title="Raw records",
            pages=[sidecar_pb2.PageDeclaration(path="/", title="Raw records",
                                               levels=[admin, write, read])]),
        settings=settings,
        declaration=sidecar_pb2.PluginDeclaration(
            storage=sidecar_pb2.StorageDeclaration(
                retention_days=2555,
                record_kinds=[sidecar_pb2.RawRecordKind(name=n, label=l, window_days=w, archivable=a)
                              for n, l, w, a in KINDS]))).SerializeToString()


def file_of(base, unit):
    return os.path.join(base, "units", unit.replace("/", "__") + ".jsonl")


def index():
    try:
        with open(INDEX, encoding="utf-8") as kept:
            return json.load(kept)
    except (OSError, ValueError):
        return {}


def keep(held):
    with open(INDEX + ".new", "w", encoding="utf-8") as kept:
        json.dump(held, kept, indent=1, sort_keys=True)
    os.replace(INDEX + ".new", INDEX)


def write_unit(base, unit, lines):
    path = file_of(base, unit)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as out:
        out.writelines(json.dumps(line) + "\n" for line in lines)
    return path


def digest(path):
    with open(path, "rb") as held:
        data = held.read()
    return len(data), hashlib.sha256(data).hexdigest()


def seed():
    """Four units, once: their records as received, and the index."""
    with LOCK:
        if index():
            return
        now = time.time_ns()
        # kind, unit, records, the first received, and the days they span:
        # a month of an account's activity, or a few days of responses.
        units = [
            ("activity", "activity/ACC-1/2010-01", 120, 1_262_304_000_000_000_000, 31),
            ("activity", "activity/ACC-1/2019-03", 214, 1_551_398_400_000_000_000, 31),
            ("activity", "activity/ACC-1/recent", 40, now - 25 * DAY_NS, 20),
            ("responses", "responses/recent", 12, now - 2 * DAY_NS, 1),
        ]
        held = {}
        for kind, unit, count, first, days in units:
            step = days * DAY_NS // (count - 1)
            lines = [{"received_ns": first + n * step, "record": f"{unit}#{n}"} for n in range(count)]
            write_unit(STORAGE, unit, lines)
            held[unit] = {"kind": kind, "where": "storage", "restored": False, "count": count,
                          "first": first, "last": first + (count - 1) * step}
        keep(held)


def archived_bytes(held, name):
    """The bytes a kind's units use of the archive (StoredSpan.bytes), as
    the SDK sums them from its index: each archived unit's file."""
    total = 0
    for unit, entry in held.items():
        if entry["kind"] == name and entry["where"] == "archive" and ARCHIVE:
            try:
                total += os.path.getsize(file_of(ARCHIVE, unit))
            except OSError:
                pass
    return total


def stored():
    """What storage holds of each declared kind, and what each uses of the
    archive, for the heartbeat."""
    held = index()
    spans = []
    for name, _, _, _ in KINDS:
        units = [u for u in held.values() if u["kind"] == name and u["where"] == "storage"]
        span = sidecar_pb2.StoredSpan(record_kind=name, record_count=sum(u["count"] for u in units),
                                      bytes=archived_bytes(held, name))
        if units:
            span.first_received_ns = min(u["first"] for u in units)
            span.last_received_ns = max(u["last"] for u in units)
        spans.append(span)
    return spans


def heartbeat_forever():
    stub = sidecar_pb2_grpc.SidecarServiceStub(grpc.insecure_channel(SIDECAR))
    while True:
        try:
            stub.Heartbeat(sidecar_pb2.HeartbeatRequest(healthy=True, stored=stored()), timeout=5)
        except grpc.RpcError as failed:
            print(f"archiving: a heartbeat was refused: {failed.details()}", flush=True)
        time.sleep(3)


def watch_windows():
    stub = sidecar_pb2_grpc.SidecarServiceStub(grpc.insecure_channel(SIDECAR))
    while True:
        try:
            for delivery in stub.WatchSettings(sidecar_pb2.WatchSettingsRequest()):
                SETTINGS.clear()
                SETTINGS.update({v.name: v.value for v in delivery.values})
        except grpc.RpcError:
            time.sleep(1)


def start():
    seed()
    threading.Thread(target=heartbeat_forever, daemon=True).start()
    threading.Thread(target=watch_windows, daemon=True).start()


def reported(unit, held, outcome, rule, header=None):
    """The move reported through the sidecar: {ok} or what it refused, with
    the refusal's code where it carried one."""
    stub = sidecar_pb2_grpc.SidecarServiceStub(grpc.insecure_channel(SIDECAR))
    request = sidecar_pb2.RecordMoveRequest(
        record_kind=held["kind"], unit=unit, record_count=held["count"],
        first_received_ns=held["first"], last_received_ns=held["last"],
        outcome=outcome, rule=rule)
    metadata = [("meridian-caller", header)] if header else []
    try:
        stub.RecordMove(request, metadata=metadata, timeout=10)
    except grpc.RpcError as refused:
        reason = ""
        for key, value in refused.trailing_metadata() or ():
            if key == "meridian-refusal-bin":
                code = sidecar_pb2.Refusal.FromString(value).reason
                reason = sidecar_pb2.RefusalReason.Name(code)
        return {"ok": False, "code": refused.code().name, "detail": refused.details(),
                "reason": reason}
    return {"ok": True}


def window_rule(kind):
    return f"{kind}_window_days {SETTINGS.get(f'{kind}_window_days') or WINDOW[kind]}"


def archive(form):
    unit = form.get("unit", "")
    with LOCK:
        held = index()
        if unit not in held or held[unit]["where"] != "storage":
            return {"ok": False, "detail": f"{unit} is not in storage"}
        entry = held[unit]
        if not ARCHIVE and not form.get("force"):
            return {"ok": False, "detail": "no archive: its records past their window are kept"}
        landed = None
        if ARCHIVE:
            source = file_of(STORAGE, unit)
            with open(source, encoding="utf-8") as kept:
                lines = [json.loads(line) for line in kept]
            landed = write_unit(ARCHIVE, unit, lines)
            if digest(landed) != digest(source):
                os.remove(landed)
                return {"ok": False, "detail": "the archive's copy did not land whole"}
        said = reported(unit, entry, sidecar_pb2.MOVE_OUTCOME_ARCHIVED, window_rule(entry["kind"]))
        if not said["ok"]:
            if landed:
                os.remove(landed)
            return said
        os.remove(file_of(STORAGE, unit))
        entry["where"] = "archive"
        keep(held)
        return said


def restore(form, header):
    unit = form.get("unit", "")
    if not header:
        return {"ok": False, "detail": "a restore is asked for by a person"}
    with LOCK:
        held = index()
        if unit not in held or held[unit]["where"] != "archive":
            return {"ok": False, "detail": f"{unit} is not in the archive"}
        with open(file_of(ARCHIVE, unit), encoding="utf-8") as kept:
            lines = [json.loads(line) for line in kept]
        copy = write_unit(os.path.join(STORAGE, "restore"), unit, lines)
        said = reported(unit, held[unit], sidecar_pb2.MOVE_OUTCOME_RESTORED, "", header)
        if not said["ok"]:
            os.remove(copy)
            return said
        held[unit]["restored"] = True
        keep(held)
        return said


def give_back(form):
    unit = form.get("unit", "")
    with LOCK:
        held = index()
        if unit not in held or not held[unit]["restored"]:
            return {"ok": False, "detail": f"{unit} is not restored"}
        said = reported(unit, held[unit], sidecar_pb2.MOVE_OUTCOME_RETURNED, "restore period 7 days")
        if not said["ok"]:
            return said
        os.remove(file_of(os.path.join(STORAGE, "restore"), unit))
        held[unit]["restored"] = False
        keep(held)
        return said


def delete(form, header):
    unit = form.get("unit", "")
    for_person = bool(form.get("for_person"))
    with LOCK:
        held = index()
        if unit not in held or held[unit]["where"] == "deleted":
            return {"ok": False, "detail": f"{unit} is not kept"}
        entry = held[unit]
        rule = "" if for_person else f"{entry['kind']}_past_window deleted"
        said = reported(unit, entry, sidecar_pb2.MOVE_OUTCOME_DELETED, rule,
                        header if for_person else None)
        if not said["ok"]:
            return said
        base = STORAGE if entry["where"] == "storage" else ARCHIVE
        os.remove(file_of(base, unit))
        entry["where"] = "deleted"
        keep(held)
        return said


def resolves(key):
    """A row's raw record, as this plugin's page resolves it."""
    unit = key.split("#", 1)[0]
    entry = index().get(unit)
    if entry is None:
        return {"key": key, "resolves": "not a record this plugin keeps"}
    where = entry["where"]
    said = {"storage": "in storage", "archive": "archived, restorable", "deleted": "deleted"}[where]
    if where == "archive" and entry["restored"]:
        said = "restored, readable"
    return {"key": key, "resolves": said}


def read_back(unit):
    try:
        with open(file_of(os.path.join(STORAGE, "restore"), unit), encoding="utf-8") as kept:
            lines = [json.loads(line) for line in kept]
    except OSError:
        return {"ok": False, "detail": f"{unit} is not restored"}
    return {"ok": True, "records": len(lines), "first": lines[0]["record"] if lines else None}


def get(path, asked):
    """A GET this module answers, or None."""
    if path == "/record":
        return resolves(asked.get("key", ""))
    if path == "/archive/read":
        return read_back(asked.get("unit", ""))
    if path == "/archive/list":
        return {"ok": True, "index": index(), "archive": bool(ARCHIVE)}
    return None


def post(path, form, header):
    """A POST this module answers, or None."""
    if path == "/archive":
        return archive(form)
    if path == "/archive/restore":
        return restore(form, header)
    if path == "/archive/return":
        return give_back(form)
    if path == "/archive/delete":
        return delete(form, header)
    return None


def caller_said(header):
    """Who the header names, for a log line: never the assertion itself."""
    padded = header + "=" * (-len(header) % 4)
    claims = sidecar_pb2.CallerClaims.FromString(
        sidecar_pb2.CallerAssertion.FromString(base64.urlsafe_b64decode(padded)).claims)
    return claims.subject
