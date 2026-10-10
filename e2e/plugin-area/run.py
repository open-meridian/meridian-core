"""`make e2e-plugin-area`: core's plugin area is at parity on the MCP, end to
end on the plugin harness (contract v17; plans/cores-plugin-area-is-at-parity-
on-the-mcp, "What the tests prove", the e2e; W6.20).

Core's stand-in as one plugin at the edge holding custody and operations
(e2e/plugin-page/archiving.py's area_registration): two kinds of raw record,
the window settings serving custody, a secret serving custody, a table
setting serving custody whose columns are a plan code, an external account
and an instrument, and a number serving both roles. Two people beside the
harness's admin, who holds the deployment admin's capabilities and All
plugins (admin): Cat, admin on custody alone, and Ben, write on custody and
read on operations. Each acts through an agent's client on a delegation
narrowed as the plan's tests ask, through `/mcp` alone:

1. A custody admin's agent (Cat's, covering admin on custody) is listed the
   plugin area's reads and the settings tool and none of the deployment
   admin's; reads the Summary, the moves and the settings, each setting
   saying whether she may set it and, for the one serving both roles, why
   not; sets a window against the version read, with a note, and replaces
   the table's rows; a change against the version it read before is refused
   naming against_updated_at_ns, and so is one naming none (contract v18);
   a cell naming no instrument is refused at
   table.plan_code_links[0].instrument, the setting serving operations too
   at value.poll_minutes; and the admin portal's Settings form says she last
   changed them.
2. The same agent is refused the secret's value by name, either way, and a
   change without a note; and clears the secret, as the form's Clear.
3. Ben's agent, write and read, is listed none of the area's tools but the
   plugins, and a call to the settings by name is refused.
4. A deployment admin's agent (the admin's, covering the deployment admin's
   capabilities alone) is listed the seven and reads the Overview's parts,
   nothing of Settings; allows the archive with a bound, told the instance
   restarts, and Cat's agent reads it on the Summary; withdraws it; sets a
   hold, which the settings tool then refuses a window below, as the page
   does; and reads the holds.
5. The same reads the catalogue, and a stop and a launch go to the conductor
   and come back refused as the terminal's would: the harness has no
   launcher, so no instance here was launched from the catalogue (core's
   unit tests launch and stop through a stand-in launcher, each record
   naming the delegation and client).
6. No tool list read in the run holds an access change, and no answer
   carries the secret's value; `store moves` prints each change its own
   record, naming the person, the delegation, the client and the note.

Throughout (contract v17's fixes): each agent connects through the consent
page, which must list exactly the tools tools/list then lists for what was
allowed -- a custody admin's, a writer and reader's, a deployment admin's
(`mcp connect --check-consent`); and each change's note reads back through
the read tools beside who made it and through which client: a setting's last
change, the secret's clear, the archive allowed and withdrawn, and the hold.
A launch's note is held by core's unit tests, the harness having no
launcher.

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import subprocess
import sys
import time

PROJECT = "meridian-core-plugin-area"
PEOPLE = "cat=Cat Ruiz,ben=Ben Ito"
LOG = ".e2e-plugin-area.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/plugin-area/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/plugin-page"))
INSTANCE = "area"
# Typed at the Settings form by the admin, as only a person does: never in
# an answer through /mcp.
SECRET = "sk-test-plugin-area-not-a-real-key"
GIB_50 = 53_687_091_200
AREA_READS = ["dashboard__list_plugins", "dashboard__read_plugin_summary",
              "dashboard__read_moves", "dashboard__read_plugin_settings",
              "dashboard__read_plugin_access"]
DEPLOYMENT_ADMINS = ["dashboard__allow_archive", "dashboard__withdraw_archive",
                     "dashboard__read_holds", "dashboard__set_hold",
                     "dashboard__read_plugin_catalogue", "dashboard__launch_plugin",
                     "dashboard__stop_plugin"]
ACCESS_CHANGES = ("grant", "permission", "access_group", "user_group", "define")


class Failed(Exception):
    pass


def log(text):
    with open(LOG, "a", encoding="utf-8") as kept:
        kept.write(text + "\n")


def compose(*args, check=True):
    done = subprocess.run(COMPOSE + list(args), env=ENV, capture_output=True, text=True,
                          timeout=600)
    log(f"$ compose {' '.join(args)[:300]}\n{done.stdout}{done.stderr}")
    if check and done.returncode != 0:
        raise Failed(f"compose {' '.join(args)[:200]}: {(done.stdout + done.stderr).strip()[-600:]}")
    return done


def runner(*args, person=None, check=True):
    extra = ["--as", person] if person else []
    done = compose("run", "--rm", "-T", "runner", *args, *extra, check=check)
    return done if not check else done.stdout


def must(condition, why):
    if not condition:
        raise Failed(why)


# Every answer and every tool list read, for step 6.
ANSWERS = []
LISTS = []


def connect(person, client, *covers):
    """Connected through the consent page, which must list exactly the tools
    tools/list then lists (--check-consent)."""
    said = runner("mcp", "connect", *[a for c in covers for a in ("--covers", c)],
                  "--client", client, "--check-consent", person=person)
    must("the consent page listed the" in said, f"{client}: the consent page was not checked: {said}")
    delegation = said.split("through delegation ", 1)[1].split(",", 1)[0].strip()
    must(delegation, f"{client} connected through no delegation: {said}")
    return delegation


def tools(person, client):
    names = runner("mcp", "list", "--client", client, person=person).split()
    LISTS.append(names)
    return names


def call(person, client, tool, arguments, outcome):
    """One tool call through `client`'s delegation, its typed answer, which
    must be `outcome`."""
    done = runner("mcp", "call", tool, json.dumps(arguments), "--client", client,
                  person=person, check=False)
    try:
        answer = json.loads(done.stdout)
    except ValueError:
        raise Failed(f"{tool} through {client}: no answer: {(done.stdout + done.stderr)[-400:]}")
    ANSWERS.append(done.stdout)
    must(answer.get("outcome") == outcome,
         f"{tool} through {client} answered {answer.get('outcome')}, not {outcome}: {done.stdout[-700:]}")
    return answer


def refused_at(answer, path):
    return any(field.get("path") == path for field in answer.get("fields", []))


def version_read(person, client):
    """updated_at_ns as the settings tool reads it now: what every change
    names as against_updated_at_ns (contract v18, W6.11)."""
    return call(person, client, "dashboard__read_plugin_settings",
                {"plugin_instance_id": INSTANCE}, "unchanged")["data"]["updated_at_ns"]


def setting(data, name):
    found = [d for d in data["declared_settings"] if d["name"] == name]
    must(found, f"no setting {name} in {json.dumps(data)[:400]}")
    return found[0]


def run():
    started = time.time()
    compose("down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    runner("ready")
    # The accounts its connection reaches, said again now the dashboard
    # listens (a report made before it subscribed is not heard), until the
    # table's account column offers them.
    runner("form", "--level", "admin", "--page", "/", "--post", "/report")
    runner("view", "--tab", "setting-plan_code_links", "--until", "ext-e2e", "--seconds", "60")
    # The secret, at the form, as a person types it; and the grants.
    runner("settings", f"api_key={SECRET}")
    for person, role, level in [("cat", "custody", "admin"), ("ben", "custody", "write"),
                                ("ben", "operations", "read")]:
        runner("grant", "--to", person, "--role", role, "--level", level)

    cat = connect("cat", "Cat's agent", f"{INSTANCE}:custody:admin")
    ben = connect("ben", "Ben's agent", f"{INSTANCE}:custody:write", f"{INSTANCE}:operations:read")
    admin = connect(None, "Admin's agent", "deployment_admin")
    as_cat = ("cat", "Cat's agent")
    as_admin = (None, "Admin's agent")

    # 1. A custody admin reads and sets, refused by path.
    listed = tools(*as_cat)
    for name in AREA_READS + ["dashboard__set_plugin_settings"]:
        must(name in listed, f"step 1, {name} is listed to a custody admin: {listed}")
    for name in DEPLOYMENT_ADMINS:
        must(name not in listed, f"step 1, {name} is a deployment admin's: {listed}")
    summary = call(*as_cat, "dashboard__read_plugin_summary", {"plugin_instance_id": INSTANCE},
                   "unchanged")["data"]
    must(any(s["record_kind"] == "activity" for s in summary.get("stored", [])),
         f"step 1, the Summary says what storage holds: {json.dumps(summary)[:500]}")
    must("declared_tools" in summary and summary.get("declaration"),
         "step 1, the Summary's own parts, for an admin of its roles")
    call(*as_cat, "dashboard__read_moves", {"plugin_instance_id": INSTANCE}, "unchanged")
    read = call(*as_cat, "dashboard__read_plugin_settings", {"plugin_instance_id": INSTANCE},
                "unchanged")["data"]
    must(setting(read, "activity_window_days")["may_set"] is True, "step 1, a custody setting is hers")
    both = setting(read, "poll_minutes")
    must(both["may_set"] is False and "operations" in both.get("detail", ""),
         f"step 1, the setting serving both says why not: {both}")
    must("api_key" in read["secrets_set"], "step 1, the secret is said to be set")
    keyed = [c for c in read["changes"] if c["name"] == "api_key"]
    must(keyed and keyed[0]["changed_by"] == "local|harness" and keyed[0]["client_name"] == "",
         f"step 1, the secret's last change, by whom and when, set in a browser: {keyed}")
    version = read["updated_at_ns"]
    call(*as_cat, "dashboard__set_plugin_settings",
         {"plugin_instance_id": INSTANCE, "value": {"activity_window_days": 2600},
          "against_updated_at_ns": version, "note": "Keep activity a little past seven years."},
         "made")
    stale = call(*as_cat, "dashboard__set_plugin_settings",
                 {"plugin_instance_id": INSTANCE, "value": {"activity_window_days": 2700},
                  "against_updated_at_ns": version, "note": "From an old read."}, "refused")
    must(refused_at(stale, "against_updated_at_ns"),
         f"step 1, a change against an older version is refused naming the field: {stale}")
    call(*as_cat, "dashboard__set_plugin_settings",
         {"plugin_instance_id": INSTANCE,
          "table": {"plan_code_links": [{"plan_code": "OQKR", "account": "ext-e2e"}]},
          "against_updated_at_ns": version_read(*as_cat),
          "note": "The 401(k)'s money market fund code."}, "made")
    # Every change names the version it read (contract v18; v17's security
    # review, I1): left out, it is refused by name.
    unguarded = call(*as_cat, "dashboard__set_plugin_settings",
                     {"plugin_instance_id": INSTANCE, "value": {"activity_window_days": 2600},
                      "note": "No version named."}, "refused")
    must(refused_at(unguarded, "against_updated_at_ns"),
         f"step 1, a change naming no version is refused by name: {unguarded}")
    bad = call(*as_cat, "dashboard__set_plugin_settings",
               {"plugin_instance_id": INSTANCE,
                "table": {"plan_code_links": [{"plan_code": "OQKR", "account": "ext-e2e",
                                               "instrument": "LCL-none"}]},
                "against_updated_at_ns": version_read(*as_cat), "note": "n"}, "refused")
    must(refused_at(bad, "table.plan_code_links[0].instrument"),
         f"step 1, a cell naming no record is refused at its path: {bad}")
    served = call(*as_cat, "dashboard__set_plugin_settings",
                  {"plugin_instance_id": INSTANCE, "value": {"poll_minutes": 5},
                   "against_updated_at_ns": version_read(*as_cat), "note": "n"},
                  "refused")
    must(refused_at(served, "value.poll_minutes") and "operations" in served.get("detail", ""),
         f"step 1, a setting serving operations too is refused naming it: {served}")
    tabled = call(*as_cat, "dashboard__read_plugin_settings", {"plugin_instance_id": INSTANCE},
                  "unchanged")["data"]
    rows = [v["value"] for v in tabled["values"] if v["name"] == "plan_code_links"]
    must(rows and rows[0][0]["plan_code"] == "OQKR" and rows[0][0]["changed_by"] == "local|cat",
         f"step 1, the table's rows, stamped with who: {rows}")
    # Each setting's last change reads back with who, through what, and why.
    last = {c["name"]: c for c in tabled["changes"]}
    for name, note in [("activity_window_days", "Keep activity a little past seven years."),
                       ("plan_code_links", "The 401(k)'s money market fund code.")]:
        must(last.get(name, {}).get("note") == note and last[name]["changed_by"] == "local|cat"
             and last[name]["client_name"] == "Cat's agent"
             and last[name]["acting_through_delegation"] == cat,
             f"step 1, {name}'s last change names Cat, her client and the note: {last.get(name)}")
    runner("view", "--tab", "settings", "--until", "Last changed by Cat Ruiz")

    # 2. A secret: never its value, either way; cleared as the form's Clear.
    for arguments, path in [
        ({"value": {"api_key": "sk-typed-by-an-agent"}, "note": "n"}, "value.api_key"),
        ({"secret": {"api_key": "sk-typed-by-an-agent"}, "note": "n"}, "secret.api_key"),
        ({"value": {"activity_window_days": 2650}}, "note"),
    ]:
        refused = call(*as_cat, "dashboard__set_plugin_settings",
                       {"plugin_instance_id": INSTANCE,
                        "against_updated_at_ns": version_read(*as_cat), **arguments}, "refused")
        must(refused_at(refused, path), f"step 2, refused at {path}: {refused}")
    call(*as_cat, "dashboard__set_plugin_settings",
         {"plugin_instance_id": INSTANCE, "clear": {"api_key": True},
          "against_updated_at_ns": version_read(*as_cat),
          "note": "The key was shown in a screenshot; cleared it."}, "made")
    cleared = call(*as_cat, "dashboard__read_plugin_settings", {"plugin_instance_id": INSTANCE},
                   "unchanged")["data"]
    must("api_key" not in cleared["secrets_set"], "step 2, the secret is cleared")
    keyed = [c for c in cleared["changes"] if c["name"] == "api_key"]
    must(keyed and keyed[0]["changed_by"] == "local|cat"
         and keyed[0]["acting_through_delegation"] == cat
         and keyed[0]["client_name"] == "Cat's agent"
         and keyed[0]["note"] == "The key was shown in a screenshot; cleared it.",
         f"step 2, its last change names Cat, her delegation, her client and why: {keyed}")

    # 3. Write and read: none of the area's tools but the plugins.
    listed = tools("ben", "Ben's agent")
    area = [n for n in listed if n in AREA_READS + DEPLOYMENT_ADMINS + ["dashboard__set_plugin_settings"]]
    must(area == ["dashboard__list_plugins"], f"step 3, write and read list the plugins alone: {listed}")
    entries = call("ben", "Ben's agent", "dashboard__list_plugins", {}, "unchanged")["data"]["plugins"]
    held = {(e["role"], e["level"]) for p in entries if p["plugin_instance_id"] == INSTANCE
            for e in p["entries"]}
    must(held == {("custody", "ACCESS_LEVEL_WRITE"), ("operations", "ACCESS_LEVEL_READ")},
         f"step 3, what Ben holds on each role through the delegation: {held}")
    refused = call("ben", "Ben's agent", "dashboard__read_plugin_settings",
                   {"plugin_instance_id": INSTANCE}, "refused")
    must(refused.get("reason") == "not_listed", f"step 3, refused by name: {refused}")

    # 4. A deployment admin: the seven, and the Overview's parts.
    listed = tools(*as_admin)
    for name in DEPLOYMENT_ADMINS + ["dashboard__read_plugin_summary", "dashboard__read_plugin_access"]:
        must(name in listed, f"step 4, {name} is the deployment admin's: {listed}")
    for name in ["dashboard__read_plugin_settings", "dashboard__set_plugin_settings",
                 "dashboard__read_moves"]:
        must(name not in listed, f"step 4, {name} is an admin's of its roles: {listed}")
    overview = call(*as_admin, "dashboard__read_plugin_summary", {"plugin_instance_id": INSTANCE},
                    "unchanged")["data"]
    must(overview["registered"] and "figures" not in overview and "declared_tools" not in overview,
         f"step 4, the Overview's parts alone: {json.dumps(overview)[:500]}")
    access = call(*as_admin, "dashboard__read_plugin_access", {"plugin_instance_id": INSTANCE},
                  "unchanged")["data"]
    must(access["permissions"], f"step 4, who holds access, read: {access}")
    allowed = call(*as_admin, "dashboard__allow_archive",
                   {"instance_id": INSTANCE, "most_bytes": GIB_50,
                    "note": "Seven years of activity, in the archive past its window."}, "made")
    must("restarts" in allowed.get("detail", ""), f"step 4, it says the instance restarts: {allowed}")
    archived = call(*as_cat, "dashboard__read_plugin_summary", {"plugin_instance_id": INSTANCE},
                    "unchanged")["data"].get("archive") or {}
    must(archived.get("allowed") is True and archived.get("most_bytes") == GIB_50
         and archived.get("client_name") == "Admin's agent"
         and archived.get("updated_by") == "local|harness"
         and archived.get("note") == "Seven years of activity, in the archive past its window.",
         f"step 4, the Summary's raw records show the archive allowed, by whom and why: {archived}")
    call(*as_admin, "dashboard__withdraw_archive",
         {"instance_id": INSTANCE, "note": "Not until the bucket is in place."}, "made")
    call(*as_admin, "dashboard__set_hold",
         {"role": "custody", "days": 2190, "note": "The records rule: six years."}, "made")
    below = call(*as_cat, "dashboard__set_plugin_settings",
                 {"plugin_instance_id": INSTANCE, "value": {"activity_window_days": 30},
                  "against_updated_at_ns": version_read(*as_cat), "note": "Shorter."}, "refused")
    must(refused_at(below, "value.activity_window_days") and "below the hold" in below.get("detail", ""),
         f"step 4, a window below the hold is refused on the tool as on the page: {below}")
    holds = call(*as_admin, "dashboard__read_holds", {}, "unchanged")["data"]["holds"]
    must(holds and holds[0]["days"] == 2190 and holds[0]["client_name"] == "Admin's agent"
         and holds[0]["acting_through_delegation"] == admin
         and holds[0]["updated_by"] == "local|harness"
         and holds[0]["note"] == "The records rule: six years.",
         f"step 4, the hold, set through the admin's client, and why: {holds}")
    after = call(*as_cat, "dashboard__read_moves", {"plugin_instance_id": INSTANCE},
                 "unchanged")["data"].get("archive") or {}
    must(after.get("allowed") is False and after.get("note") == "Not until the bucket is in place.",
         f"step 4, the archive's withdrawal reads back with why: {after}")

    # 5. The catalogue, a stop and a launch, as the terminal's.
    catalogue = call(*as_admin, "dashboard__read_plugin_catalogue", {}, "unchanged")["data"]
    must(catalogue["launches"] == [], f"step 5, nothing here was launched from it: {catalogue}")
    stopped = call(*as_admin, "dashboard__stop_plugin",
                   {"instance_id": INSTANCE, "note": "Stopping it for the upgrade."}, "refused")
    must(f"no launch of {INSTANCE} is live" in stopped.get("detail", ""),
         f"step 5, the conductor's answer to a stop: {stopped}")
    launched = call(*as_admin, "dashboard__launch_plugin",
                    {"name": "stand-in", "version": "1.0.0", "instance_id": "area-2",
                     "approved_roles": ["custody"], "note": "A second one."}, "refused")
    must("not in the catalogue" in launched.get("detail", ""),
         f"step 5, the conductor's answer to a launch: {launched}")

    # 6. No access change listed, no secret answered, each change its record.
    for names in LISTS:
        for name in names:
            must(not (name.startswith("dashboard__") and any(w in name for w in ACCESS_CHANGES)),
                 f"step 6, {name} changes access")
    must(not any(SECRET in answer for answer in ANSWERS), "step 6, an answer carried the secret")
    store = compose("run", "--rm", "-T", "store", "moves").stdout.split("\n")
    settings = [line for line in store if line.startswith(f"setting|{INSTANCE}|")]

    def recorded(prefix, suffix):
        return any(line.startswith(prefix) and line.endswith(suffix) for line in settings)

    must(recorded(f"setting|{INSTANCE}|activity_window_days|set|false|local|cat|{cat}|Cat's agent|",
                  "Keep activity a little past seven years."),
         f"step 6, the window's change names Cat, her delegation, client and note: {settings}")
    must(recorded(f"setting|{INSTANCE}|plan_code_links|set|false|local|cat|{cat}|Cat's agent|",
                  "The 401(k)'s money market fund code."), f"step 6, the table's change: {settings}")
    must(recorded(f"setting|{INSTANCE}|api_key|set|true|local|harness|||", ""),
         f"step 6, the secret set in a browser, only as set: {settings}")
    must(recorded(f"setting|{INSTANCE}|api_key|cleared|false|local|cat|{cat}|Cat's agent|",
                  "cleared it."), f"step 6, the secret cleared through Cat's agent: {settings}")
    must(not any(SECRET in line for line in store), "step 6, the store printed the secret")
    must(f"hold|custody|2190|false|local|harness|{admin}|Admin's agent|The records rule: six years."
         in store, f"step 6, the hold names the admin, the delegation, the client and why: {store}")
    archives = [line for line in store if line.startswith(f"archive|{INSTANCE}|")]
    must(archives == sorted([
        f"archive|{INSTANCE}|true|{GIB_50}|local|harness|{admin}|Admin's agent|"
        "Seven years of activity, in the archive past its window.",
        f"archive|{INSTANCE}|false|{GIB_50}|local|harness|{admin}|Admin's agent|"
        "Not until the bucket is in place."]),
         f"step 6, each archive change its own record: {archives}")
    must(ben, "Ben's delegation")

    took = int(time.time() - started)
    print(f"e2e-plugin-area OK in {took}s: on the plugin harness, an edge plugin holding custody "
          "and operations, worked through /mcp alone: a custody admin's agent reads its Summary, "
          "moves and settings, each saying whether she may set it, sets a window against the "
          "version read and replaces a table's rows, is refused an older version, a cell naming no "
          "record and a setting serving operations too, each by its path, and the form says she "
          "changed them; the secret's value is refused by name either way and a change without a "
          "note, and the secret is cleared; write and read list only the plugins; a deployment "
          "admin's agent lists the seven and reads the Overview's parts, allows the archive and "
          "withdraws it, sets a hold the settings tool then holds a window to, and reads the "
          "catalogue, a stop and a launch answered by the conductor; no tool list held an access "
          "change, no answer the secret, and each change is its own record naming the person, the "
          "delegation, the client and the note; each consent page listed exactly what tools/list "
          "then listed, and each change's note read back beside who made it")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError, IndexError, TypeError) as failed:
        compose("logs", "--no-color", check=False)
        compose("down", "-v", "--remove-orphans", check=False)
        print(f"e2e-plugin-area FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
