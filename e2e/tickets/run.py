"""`make e2e-tickets`: tickets inside a deployment, end to end on the plugin
harness (contract v13; plans/tickets-inside-a-deployment, "The e2e on the
plugin harness").

Core's stand-in as `custody`, `operations` and a plugin holding no role
(`restarted`); two people, Ada (`operations` write on her account alone) and
Ben (`operations` read on his); and the harness's admin, the deployment admin,
who is the `operations` admin and reaches no account. Each step drives the
dashboard's own pages, `/mcp` and the stand-in's page as a person does, and
reads `store tickets`:

1. A person files, on the operations area, an account named in her text.
2. Her agent files through `/mcp`, recorded as her through it.
3. The stand-in files for a person, never as itself; a repeat folds; about
   another plugin it is refused; for a reader at read it is admitted; the
   plugin holding no role files too.
4. The rules' advice appears, an agent adds advice, and neither changes a
   ticket's state, owner or due date.
5. Only a person's acts work a ticket, each a change note with her name; no
   tool is listed for one; the admin works the unreferenced ticket and may
   not see the referenced one. A ticket about core naming Ada's account is
   worked by no admin who cannot read it, and by the admin once granted
   read on it (after step 7, so steps 6 and 7 read the admin as before).
6. Visibility follows accounts, and a delegation narrowed to custody lists
   none of operations'.
7. The inbox shows each change once per client, and nothing to Ben.
8. Every red-team case on a deployment channel, filed through the page,
   `/mcp` and the stand-in: refused by a bound or held as suspect and
   withheld from the tool; every other record unchanged.
9. Nothing leaves: no ticket topic in the conductor's log.

Run from the repository's root by the Makefile, which builds the images and
copies the harness out first; standard library only.
"""
import json
import os
import re
import subprocess
import sys
import time

PROJECT = "meridian-core-tickets"
PEOPLE = "ada=Ada Park,ben=Ben Ito"
LOG = ".e2e-tickets.log"
COMPOSE = ["docker", "compose", "-p", PROJECT, "-f", ".harness/compose.yaml",
           "-f", ".harness/plugins.yaml", "-f", "e2e/harness/stand-in.yaml"]
ENV = dict(os.environ, MERIDIAN_HARNESS_PEOPLE=PEOPLE,
           MERIDIAN_HARNESS_STAND_IN=os.path.abspath("e2e/plugin-page"))
WITHHELD = "withheld until a person releases it on the ticket's page"


class Failed(Exception):
    pass


def log(text):
    with open(LOG, "a", encoding="utf-8") as kept:
        kept.write(text + "\n")


def compose(*args, check=True, stdin=None):
    done = subprocess.run(COMPOSE + list(args), env=ENV, capture_output=True, text=True,
                          input=stdin, timeout=600)
    log(f"$ compose {' '.join(args)[:300]}\n{done.stdout}{done.stderr}")
    if check and done.returncode != 0:
        raise Failed(f"compose {' '.join(args)[:200]}: {done.stderr.strip()[-600:]}")
    return done


def runner(*args, person=None, instance=None, check=True):
    """One runner command; its stdout."""
    extra = []
    if person:
        extra += ["--as", person]
    if instance:
        extra += ["--instance", instance]
    done = compose("run", "--rm", "-T", "runner", *args, *extra, check=check)
    return done.stdout if check else done


def store():
    return compose("run", "--rm", "-T", "store", "tickets").stdout.splitlines()


def psql(sql):
    return compose("exec", "-T", "postgres", "psql", "-U", "meridian", "-d", "meridian",
                   "-At", "-v", "ON_ERROR_STOP=1", "-c", sql).stdout


def ticket_lines(lines, ticket_id):
    return [line for line in lines if line.split("|")[1:2] == [ticket_id]]


def ticket_row(lines, ticket_id):
    rows = [line for line in lines if line.startswith(f"ticket|{ticket_id}|")]
    if len(rows) != 1:
        raise Failed(f"store tickets holds {len(rows)} rows for {ticket_id}")
    return rows[0].split("|")


def stand_in(person, level, instance="operations", expect=None, **fields):
    """The stand-in's page posting /ticket for the person whose page it is."""
    said = runner("form", "--level", level, "--page", "/", "--post", "/ticket",
                  *[f"{k}={v}" for k, v in fields.items()],
                  *(["--expect", expect] if expect else []),
                  person=person, instance=instance)
    body = said.split("\n", 1)[1] if "\n" in said else said
    return json.loads(body)


def mcp(person, name, arguments, client=None, outcome=None):
    args = ["mcp", "call", name, json.dumps(arguments)]
    if outcome:
        args += ["--expect-outcome", outcome]
    if client:
        args += ["--client", client]
    return json.loads(runner(*args, person=person))


def must(condition, why):
    if not condition:
        raise Failed(why)


def records_that_must_not_change():
    """The access records, the delegations and the plugins' settings, as
    they stand: what no ticket, note or advice may change."""
    held = []
    for table in ["config_access_entry", "config_access_group", "config_account",
                  "config_account_group", "config_external_account_link", "config_permission",
                  "config_plugin_setting", "config_user_group"]:
        held.append(psql(f"SELECT row_to_json(t)::text FROM {table} t ORDER BY 1"))
    held.append(psql("SELECT delegation_id || '|' || subject || '|' || covers_everything || '|' "
                     "|| covers_deployment_admin || '|' || array_to_string(covers_plugins, ',') "
                     "|| '|' || array_to_string(covers_account_groups, ',') || '|' "
                     "|| coalesce(revoked_at_ns::text, '') FROM dashboard_delegation ORDER BY 1"))
    return held


def run():
    started = time.time()
    compose("down", "-v", "--remove-orphans", check=False)
    compose("up", "-d")
    for instance in ["custody", "operations", "restarted"]:
        runner("ready", instance=instance)

    # Two accounts, Ada's and Ben's; Ada writes hers through operations, Ben
    # reads his; the admin administers operations and the role-less plugin.
    account_a = runner("account", "Growth A").strip()
    account_b = runner("account", "Beta B").strip()
    runner("grant", "--level", "write", "--to", "ada", "--accounts", account_a, instance="operations")
    runner("grant", "--level", "read", "--to", "ben", "--accounts", account_b, instance="operations")
    runner("grant", "--level", "write", "--to", "ada", "--accounts", account_a, instance="custody")
    runner("grant", "--level", "admin", instance="operations")
    runner("grant", "--level", "admin", instance="restarted")
    # Each signs in once, so the deployment knows them by name.
    for person in ["ada", "ben"]:
        runner("ticket", "list", person=person)

    # 1. A person files.
    t1 = runner("ticket", "file", "title=Cash differs from the custodian",
                f"seen=The book shows 12,400.00 USD on {account_a} and the statement shows 12,401.17 USD.",
                "kind=discrepancy", person="ada", instance="operations").strip()
    read = json.loads(runner("ticket", "read", t1, person="ada"))
    must(read["concerns"] == {"kind": "plugin", "instance": "operations"} or
         read["concerns"].get("instance") == "operations", f"step 1: {read['concerns']}")
    must(read["filed_by"] == {"provenance": "person", "person": "Ada Park"}, f"step 1: {read['filed_by']}")
    must({"kind": "account", "value": account_a, "account_id": account_a, "found_in_text": True}
         in read["references"], f"step 1: {read['references']}")

    # 2. An agent files through /mcp.
    runner("mcp", "connect", "--covers", "operations:write", person="ada")
    filed = mcp("ada", "dashboard__file_ticket",
                {"title": "Seen through my agent", "kind": "defect",
                 "concerns": {"kind": "plugin", "instance": "operations"}}, outcome="made")
    t2 = filed["data"]["ticket_id"]
    read = mcp("ada", "dashboard__read_ticket", {"ticket_id": t2})["data"]
    must(read["filed_by"]["provenance"] == "client" and read["filed_by"]["person"] == "Ada Park"
         and read["filed_by"]["client_name"] == "harness agent", f"step 2: {read['filed_by']}")
    must(int(runner("mcp", "calls", person="ada").strip()) >= 2, "step 2: the calls are not recorded")

    # 3. A plugin files for a person, never as itself.
    made = stand_in("ada", "write", title="Statement late", seen="The statement came at noon.",
                    key="late-1")
    must(made.get("outcome") == "made", f"step 3: {made}")
    t3 = made["ticket_id"]
    as_itself = stand_in("ada", "write", title="As itself", key="itself-1", as_itself="1")
    must(as_itself.get("code") == "PERMISSION_DENIED", f"step 3, as itself: {as_itself}")
    again = stand_in("ada", "write", title="Statement late", seen="The statement came at noon.",
                     key="late-1")
    must(again.get("outcome") == "unchanged" and again.get("seen_count") == 2
         and again.get("ticket_id") == t3, f"step 3, again: {again}")
    must(ticket_row(store(), t3)[10] == "2", "step 3: the repeat did not fold into one row")
    about = stand_in("ada", "write", title="About custody", key="about-1", about="custody")
    must(about.get("code") == "INVALID_ARGUMENT" and "concerns.instance: custody is another plugin"
         in about.get("detail", ""), f"step 3, about custody: {about}")
    reading = stand_in("ben", "read", title="A reader saw something", key="ben-1")
    must(reading.get("outcome") == "made", f"step 3, Ben at read: {reading}")
    t4 = reading["ticket_id"]
    read = json.loads(runner("ticket", "read", t4, person="ben"))
    must(read["filed_by"] == {"provenance": "plugin", "person": "Ben Ito", "instance": "operations"},
         f"step 3: {read['filed_by']}")
    roleless = stand_in(None, "admin", instance="restarted", title="No role, still files",
                        key="r-1")
    must(roleless.get("outcome") == "made", f"step 3, the role-less plugin: {roleless}")

    # 4. Advice appears and changes nothing.
    must(any(n["note"].startswith("Route: the firm's.") for n in
             json.loads(runner("ticket", "read", t1, person="ada"))["notes"]), "step 4: no route")
    must(any(n["note"].startswith("Likely a duplicate of") for n in
             json.loads(runner("ticket", "read", t3, person="ada"))["notes"]), "step 4: no duplicate")
    before = [line for line in store() if line.startswith("ticket|")]
    mcp("ada", "dashboard__add_ticket_note",
        {"ticket_id": t3, "kind": "advice", "note": "The custodian posts at noon on Fridays."},
        outcome="made")
    after = [line for line in store() if line.startswith("ticket|")]
    must(before == after, "step 4: advice changed a ticket's row")

    # 5. Only a person's acts.
    tools = runner("mcp", "list", person="ada").split()
    acts = [t for t in tools if t.startswith("dashboard__")
            and re.search(r"work|assign|resolve|close|reopen|release", t)]
    must(not acts, f"step 5: tools act on a ticket: {acts}")
    for act in [["act=assign", "owner=ada"], ["act=resolve", "resolution=note", "cites=1"],
                ["act=reopen"], ["act=close", "resolution=not_a_problem"]]:
        runner("ticket", "work", t3, *act, person="ada")
    changes = [line for line in ticket_lines(store(), t3)
               if line.startswith("note|") and "|change|person|Ada Park|" in line]
    must(len(changes) == 4, f"step 5: {len(changes)} change notes by Ada")
    must(ticket_row(store(), t3)[7] == "closed", "step 5: not closed")
    runner("ticket", "work", t2, "act=assign", "owner=harness")
    runner("ticket", "work", t1, "act=assign", "owner=harness", "--expect-status", "404")

    # 6. Visibility follows accounts.
    listed_ben = runner("ticket", "list", person="ben")
    must(t1 not in listed_ben, "step 6: Ben lists a ticket naming Ada's account")
    runner("ticket", "read", t1, "--expect-status", "404", person="ben")
    listed_admin = runner("ticket", "list")
    must(t1 not in listed_admin and t2 in listed_admin, f"step 6: the admin lists {listed_admin}")
    runner("mcp", "connect", "--covers", "custody:write", "--client", "custody agent", person="ada")
    narrowed = mcp("ada", "dashboard__list_tickets", {}, client="custody agent")
    must(narrowed["data"]["tickets"] == [], f"step 6: narrowed to custody: {narrowed}")

    # 7. The inbox.
    first = mcp("ada", "dashboard__read_inbox", {})["data"]["notices"]
    on_t3 = [n["kind"] for n in first if n["ticket_id"] == t3]
    for kind in ["assigned", "resolved", "reopened", "closed"]:
        must(on_t3.count(kind) == 1, f"step 7: {kind} on {t3} {on_t3.count(kind)} times: {on_t3}")
    must(mcp("ada", "dashboard__read_inbox", {})["data"]["notices"] == [], "step 7: read twice")
    runner("mcp", "connect", "--covers", "operations:write", "--client", "second agent", person="ada")
    second = mcp("ada", "dashboard__read_inbox", {}, client="second agent")["data"]["notices"]
    must([(n["ticket_id"], n["kind"]) for n in second] == [(n["ticket_id"], n["kind"]) for n in first],
         "step 7: a second client did not read the same notices")
    runner("inbox", "--expect", "0", person="ben")

    # 5, continued: core's ticket naming an account is worked by a deployment
    # admin who also reads it (ruled 2026-10-04); not by the admin before she
    # reads it, by Ben, who cannot see it, or by Ada, who filed it.
    t5 = runner("ticket", "file", "--concerns", "dashboard", "title=The dashboard shows the wrong cash",
                f"seen=The Summary for {account_a} shows yesterday's cash.", person="ada").strip()
    runner("ticket", "work", t5, "act=assign", "owner=ada", "--expect-status", "404")
    runner("ticket", "work", t5, "act=assign", "owner=ada", "--expect-status", "404", person="ben")
    runner("ticket", "work", t5, "act=assign", "owner=ada", "--expect-status", "403", person="ada")
    runner("grant", "--level", "read", "--accounts", account_a, instance="operations")
    runner("ticket", "work", t5, "act=assign", "owner=ada")
    row = ticket_row(store(), t5)
    must(row[2] == "dashboard" and row[8] == "Ada Park",
         f"step 5: core's ticket not assigned by the admin: {row}")

    # 8. Every red-team case leaves the records unchanged.
    corpus = json.load(open("deploy/prompt-attacks.json", encoding="utf-8"))
    before_tickets = store()
    before_records = records_that_must_not_change()
    replayed = 0
    for case in corpus["cases"]:
        expect = case["expect"].get("deployment")
        if not expect or case["channel"] not in ("ticket-title", "ticket-seen"):
            continue
        planted = case["planted"]
        field = "title" if case["channel"] == "ticket-title" else "seen"
        title = planted if field == "title" else f"Red-team case {case['id']}"
        seen = planted if field == "seen" else ""
        refused = expect["outcome"] == "refused"
        # The page.
        if refused:
            runner("ticket", "file", f"title={title}", f"seen={seen}", "--expect-refused", field,
                   person="ada", instance="operations")
        else:
            held = runner("ticket", "file", f"title={title}", f"seen={seen}",
                          person="ada", instance="operations").strip()
            shown = mcp("ada", "dashboard__read_ticket", {"ticket_id": held})["data"]
            must(shown["suspect"] and shown[field] == WITHHELD, f"{case['id']} on the page: {shown}")
        # /mcp.
        through = mcp("ada", "dashboard__file_ticket",
                      {"title": title, "seen": seen, "kind": "defect",
                       "concerns": {"kind": "plugin", "instance": "operations"}})
        if refused:
            must(through["outcome"] == "refused" and through["fields"][0]["path"] == field,
                 f"{case['id']} through /mcp: {through}")
        else:
            shown = mcp("ada", "dashboard__read_ticket",
                        {"ticket_id": through["data"]["ticket_id"]})["data"]
            must(shown["suspect"] and shown[field] == WITHHELD, f"{case['id']} through /mcp: {shown}")
        # The stand-in.
        by_plugin = stand_in("ada", "write", title=title, seen=seen, key=f"case-{case['id']}")
        if refused:
            must(by_plugin.get("code") == "INVALID_ARGUMENT"
                 and by_plugin.get("detail", "").startswith(f"{field}:"),
                 f"{case['id']} by the stand-in: {by_plugin}")
        else:
            shown = mcp("ada", "dashboard__read_ticket", {"ticket_id": by_plugin["ticket_id"]})["data"]
            must(shown["suspect"] and shown[field] == WITHHELD, f"{case['id']} by the stand-in: {shown}")
        replayed += 1
    on_text = [c for c in corpus["cases"] if c["expect"].get("deployment")
               and c["channel"] in ("ticket-title", "ticket-seen")]
    must(replayed == len(on_text) and replayed >= 9, f"step 8: {replayed} cases replayed")
    after_tickets = store()
    old_ids = {line.split("|")[1] for line in before_tickets}
    must([line for line in after_tickets if line.split("|")[1] in old_ids] == before_tickets,
         "step 8: a ticket filed before the corpus changed")
    must(records_that_must_not_change() == before_records,
         "step 8: the access records, the delegations or the plugins' settings changed")

    # 9. Nothing leaves.
    conductor = compose("logs", "--no-color", "conductor").stdout
    must("ticket" not in conductor.lower(), "step 9: the conductor's log names a ticket")

    took = int(time.time() - started)
    print(f"e2e-tickets OK in {took}s: on the plugin harness, a person files on a plugin's area "
          "(an account in her text a reference), her agent through /mcp (named as her through it, "
          "the call recorded), and the stand-in for a person at write and at read, never as "
          "itself, once however often it repeats, never about another plugin, and with no role; "
          "the rules' route and duplicate advice and an agent's advice change no ticket; only her "
          "acts on the page assign, resolve, reopen and close, each a change note in her name, "
          "and no tool is listed for one; the admin works the unreferenced ticket and cannot see "
          "the referenced, and works core's ticket naming Ada's account only once she reads it; "
          "Ben sees nothing naming her account, and a delegation narrowed to custody lists none "
          "of operations'; each of her clients reads each change once and Ben "
          f"none; {replayed} red-team cases through the page, /mcp and the stand-in are refused "
          "by their bound or held and withheld from tools, every other record unchanged; and the "
          "conductor never heard of a ticket")


def main():
    open(LOG, "w").close()
    try:
        run()
    except (Failed, subprocess.TimeoutExpired, KeyError, ValueError) as failed:
        compose("logs", "--no-color", check=False)
        compose("down", "-v", "--remove-orphans", check=False)
        print(f"e2e-tickets FAILED: {failed}; the run is in {LOG}", file=sys.stderr)
        return 1
    compose("logs", "--no-color", check=False)
    compose("down", "-v", "--remove-orphans", check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
