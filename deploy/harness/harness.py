"""The plugin harness's runner: what a person does in the dashboard, done as
the harness's deployment admin, through the dashboard's own pages and forms.

Standard library only. Run as the compose file's `runner` service:

    docker compose ... run --rm runner <command> [arguments]

Each command signs in afresh, does one thing, prints what it found, and exits
0; or exits non-zero saying why. Every wait has a bound (`--seconds`). Each
acts on the first plugin plugins.yaml lists, or on another with `--instance
NAME`. The admin signs in with the password the compose file's `keys` drew
for this run, read from the `secrets` volume and never printed.

And one command that is not the runner's, run before anything starts, in
the same image with the list on stdin:

  compose < plugins.json > plugins.yaml
      Writes the compose file's plugins for a JSON list of any number,
      `[{"instance": NAME, "image": IMAGE, "roles": [ROLE, ...]}, ...]`: for
      each, its sidecar as instance NAME holding those roles, and the plugin
      as the service NAME in its sidecar's network namespace, restarted when
      it fails, as a pod's container is; the broker configured for them all,
      and a password drawn for each. A plugin holding an edge role is also
      given its storage (decisions/028): a volume of its own, kept while the
      run's volumes are, at the path MERIDIAN_STORAGE_DIR names. The first is
      the runner's plugin.

  ready [--seconds N]
      Until the plugin has registered with its sidecar and the dashboard lists
      it (healthy or not): the first command of a run.

  settings NAME=VALUE ... [--seconds N] [--expect TEXT]
      Sets the plugin's settings in its settings form, as its admin does
      (W6.11), once the plugin has declared each. A secret goes in its secret
      field; a developer's setting is accepted because the harness's dashboard
      is a development one. A table setting's cell is NAME[ROW].COLUMN=VALUE,
      the rows given its whole (contract v14). --expect reads the Settings
      tab after and fails unless it says TEXT.

  account NAME [--seconds N]
      Defines an account (W6.3) and prints its ID, for a plugin whose page
      links only to an account that exists.

  page --level admin|write|read PATH [--until TEXT] [--seconds N]
      Opens a session on the plugin's own host at that level -- Manage, Open
      or View -- and GETs PATH there. Prints the status, then the body. With
      --until, again until the body says TEXT.

  form --level L --page PATH --post PATH [--csrf-field NAME] [--from-page NAME ...]
       [--expect TEXT] FIELD=VALUE ...
      The same session: reads PATH, takes its CSRF field (`csrf`, as the SDK
      names it; a FIELD given by that name wins) when it has one, and each
      field --from-page names (repeated, or comma separated) with the value
      the page gives it, as a person posting the page's own form sends what
      it holds -- a proposal's digest, say; and posts the fields urlencoded
      to --post, a field repeated as often as given. A FIELD given wins over
      one taken from the page; a field the page does not have fails. Prints
      the status, then the body. With --expect, fails unless the body says
      TEXT. This is how a plugin's own link page is driven: a link is the
      plugin's to send, for an admin (W6.4), never the harness's.

  unlinked [--expect N] [--seconds N]
      Prints how many external accounts the plugin reported that nothing
      links, as the dashboard counts them on its line (W6.4, W6.10). With
      --expect, waits until it is N.

  grant --level read|write|admin
      Grants the harness's admin that level on the plugin, on All accounts,
      as a deployment admin does in the dashboard (W6.5 to W6.8): a user group
      holding the admin, an access group giving the plugin the level, and the
      permission joining them. A plugin that writes for a person -- an
      operations plugin confirming an opening balance (W9.1) -- is opened at
      write by someone granted write; the admin is granted nothing on a
      plugin until this is run.

  instruments [--expect N] [--seconds N]
      Prints the instrument records the book cannot use yet -- no asset class
      or no currency -- one per line, its ID, a tab, and its identifiers as the
      dashboard's Instruments page lists them (W3.11, contract v10). With
      --expect, waits until it lists N.

  instrument (ID | --identifier TEXT) asset_class=CLASS currency=CODE
             [description=TEXT] source=TEXT [note=TEXT] [--seconds N]
      Completes one record on its page, as the deployment admin does (W3.10):
      the record named by its ID, or the first listed whose identifiers say
      TEXT (`symbol: AAPL`), waiting for it to be listed. CLASS is one of
      equity, debt, fund, derivative, crypto_asset, event_contract, cash; each
      value goes with the source given, and the dashboard stamps the admin.

  mcp connect [--covers deployment_admin] [--covers INSTANCE:LEVEL ...] [--client NAME]
      Connects an MCP client as the admin, as an agent's client does (W6.17,
      W6.20): registers it (named "harness agent" unless --client says), sends
      her through the dashboard's authorisation for the `/mcp` resource with
      a PKCE challenge, signs her in afresh, consents to only what --covers
      names -- the deployment admin's capabilities, and each plugin and level
      -- on All accounts, for 30 days, and exchanges the code for a token
      pair, kept for the run in the runner's state. Prints the delegation and
      how many tools it reaches.

  mcp list [--expect NAME ...]
      Prints the tools the connected delegation reaches, one name per line,
      and fails unless each NAME --expect gives is among them.

  mcp call NAME [JSON] [--from SAVED[.PATH]] [--set PATH=VALUE ...] [--save SAVED]
           [--expect-outcome made|unchanged|refused] [--expect TEXT]
      Calls a tool, the arguments a JSON object, or what an earlier call
      saved (--from, at PATH in its answer), each --set changing one field by
      the data dictionary's path grammar (`positions[label=BTC].lots[0].cost`
      picks the position whose label is BTC; a missing row is added). Prints
      the tool's typed answer as JSON; --save keeps it for a later --from.
      Refreshes the token pair when the access token has lapsed.

  mcp complete --identifier TEXT asset_class=CLASS currency=CODE source=TEXT note=TEXT
               [instrument_type=... fund_category=... fund_investors=... fund_nav=...
                fund_liquidity_fee=...]
      Completes the record listed with that identifier through core's tools,
      dashboard__list_instruments_to_complete then
      dashboard__complete_instruments, against the version listed.

  mcp calls [--expect N]
      Prints how many calls Connected clients lists for the admin, as she
      reads it in a browser; with --expect, fails unless it is N or more.

  ticket file [--concerns PART] title=TEXT [seen=TEXT] [kind=KIND]
              [operation=ROW] [reason=CODE] [--expect-refused PATH]
      Files a ticket as a person does, pressing "Report a problem" (W6.21,
      contract v13): about the runner's plugin, or a part of core with
      --concerns (dashboard, bor, street, instrument, conductor, chart, cli,
      sdk, platform). Prints the ticket's ID. With --expect-refused, fails
      unless the dashboard refuses it naming PATH.

  ticket list [--concerns X] [--state S] [--expect N] [--seconds N]
      Prints the tickets the person may see, one per line: the ID, a tab, its
      state, a tab, its title as the list answers it -- withheld while it is
      held as suspect. With --expect, waits until it lists N.

  ticket read ID [--page] [--expect TEXT ...] [--expect-status N]
      Prints one ticket as its row answers it (JSON, a suspect text
      withheld), or with --page the ticket's page, where a person reads it.

  ticket note ID TEXT
      Adds a note on the ticket's page.

  ticket work ID act=ACT [owner=NAME] [due=DATE] [resolution=R] [cites=C]
              [release=ticket|N] [--expect-status N]
      Takes one act on the ticket's page (W6.23): assign (owner a person's
      name, as --as takes it), due, resolve, close, reopen or release, against
      the notes the page showed.

  inbox [--expect N] [--expect-kind KIND ...]
      Prints the person's notices new since their pages last read them, one
      per line: the ticket's ID, a tab, the change's kind (W6.24). With
      --expect, fails unless there are N.

Every command acts as the harness's admin, or, given --as NAME, as one of
the people `keys` drew a password for (MERIDIAN_HARNESS_PEOPLE, `name=Display
Name` comma separated), whose account the `people` service made: a run that
needs more than one person names them there. `grant --to NAME` grants such a
person a level, and `--accounts ID,...` on only those accounts.

Reading core's HTML is this runner's alone, and only because it is published
at the same commit as the dashboard it reads, and core's gate runs it against
that dashboard at every commit. A plugin never parses core's pages itself.
"""
import base64
import hashlib
import html
import json
import os
import secrets
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

DASHBOARD = os.environ.get("MERIDIAN_HARNESS_DASHBOARD", "http://dashboard:8080")
INSTANCE = os.environ.get("MERIDIAN_HARNESS_INSTANCE", "plugin-1")
ADMIN = os.environ.get("MERIDIAN_HARNESS_ADMIN", "harness")
# Where `keys` put the admin's password, drawn at random for this run.
PASSWORD_FILE = os.environ.get("MERIDIAN_HARNESS_ADMIN_PASSWORD_FILE", "/secrets/admin/password")
# The admin's login as the deployment names its people: the dashboard's own
# account, as the conductor was told at its start (compose.yaml).
LOGIN = os.environ.get("MERIDIAN_HARNESS_ADMIN_LOGIN", f"local|{ADMIN}")
LEVELS = ("admin", "write", "read")
# Who a command acts as: the admin, or with --as one of the run's people.
AS = None
# The built-in account group holding every account (W6.6).
ALL_ACCOUNTS = "all-accounts"
# The asset classes as the Instruments page's form numbers them (W1's list).
CLASSES = {"equity": 1, "debt": 2, "fund": 3, "derivative": 4, "crypto_asset": 5,
           "event_contract": 6, "cash": 7}
# An instrument's type within its class, and a money market fund's SEC rule
# 2a-7 attributes, by the dashboard's form's numbers (contract v11).
TYPES = {"money_market_fund": 1}
FUND = {
    "fund_category": {"government": 1, "prime": 2, "tax_exempt": 3},
    "fund_investors": {"retail": 1, "institutional": 2},
    "fund_nav": {"stable": 1, "floating": 2},
    "fund_liquidity_fee": {"mandatory": 1, "discretionary": 2},
}


def acting_on(instance):
    """The plugin a command acts on: its own host, below the dashboard's
    (decisions/021) -- a browser resolves it by name, and this sends to the
    dashboard naming the host, which is the same request -- and its view."""
    global INSTANCE, PLUGIN_HOST, VIEW
    INSTANCE = instance
    PLUGIN_HOST = f"{INSTANCE}.plugins.{urllib.parse.urlparse(DASHBOARD).netloc}"
    VIEW = f"/admin/plugins/{INSTANCE}"


acting_on(INSTANCE)


class Failed(Exception):
    """Why a command did not do what it was asked."""


class Reply:
    def __init__(self, status, body, location):
        self.status = status
        self.body = body
        self.location = location


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


class Browser:
    """Cookies for one host, and every redirect left to the caller."""

    def __init__(self, host=None):
        self.host = host
        self.cookies = {}

    def send(self, method, path, fields=None):
        data = urllib.parse.urlencode(fields).encode() if fields is not None else None
        request = urllib.request.Request(DASHBOARD + path, data=data, method=method)
        if self.host:
            request.add_header("Host", self.host)
        if fields is not None:
            request.add_header("Content-Type", "application/x-www-form-urlencoded")
        if self.cookies:
            request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in self.cookies.items()))
        try:
            response = urllib.request.build_opener(NoRedirect).open(request, timeout=30)
            status, body, headers = response.status, response.read(), response.headers
        except urllib.error.HTTPError as refused:
            status, body, headers = refused.code, refused.read(), refused.headers
        for value in headers.get_all("Set-Cookie") or []:
            name, _, rest = value.partition("=")
            self.cookies[name] = rest.split(";")[0]
        return Reply(status, body.decode("utf-8", errors="replace"), headers.get("Location"))

    def get(self, path):
        return self.send("GET", path)

    def post(self, path, fields):
        return self.send("POST", path, fields)


def until(seconds, attempt, what):
    """`attempt()`'s first answer that is not None, tried every second for up
    to `seconds`; or a failure naming `what` and the last reason given."""
    deadline = time.monotonic() + seconds
    last = None
    while True:
        try:
            answer = attempt()
        except (OSError, Failed) as failed:
            answer, last = None, failed
        if answer is not None:
            return answer
        if time.monotonic() > deadline:
            raise Failed(f"{what}, in {seconds} seconds" + (f": {last}" if last else ""))
        time.sleep(1)


def sentence(reply):
    """What a page says first, for a failure to quote."""
    main = reply.body.split("<main", 1)[-1]
    found = re.findall(r"<(?:h1|p)[^>]*>(.*?)</(?:h1|p)>", main, re.S)[:2] or [main[:300]]
    return ": ".join(" ".join(re.sub(r"<[^>]+>", "", said).split()) for said in found)


def form_token(page):
    found = re.search(r'name="form_token" value="([^"]+)"', page.body)
    return found.group(1) if found else ""


def who():
    """The name a command signs in with, and its login."""
    if AS is None:
        return ADMIN, LOGIN
    return AS, f"local|{AS}"


def drawn_password():
    """The password of whoever the command acts as, as `keys` drew it when
    the run started. Read, never printed: a failure names the file, not what
    is in it."""
    password_file = PASSWORD_FILE if AS is None else f"/secrets/people/{AS}/password"
    try:
        with open(password_file, encoding="utf-8") as drawn:
            password = drawn.read().strip()
    except OSError as unread:
        raise Failed(f"the password was not drawn: {password_file}: {unread.strerror}") from None
    if not password:
        raise Failed(f"the password was not drawn: {password_file} is empty")
    return password


def signed_in(seconds=120):
    """The admin, signed in to the dashboard, once the dashboard answers and
    her account is the deployment's admin: the conductor names her at its
    start, and the dashboard reads that a moment later."""
    def attempt():
        admin = Browser()
        if admin.get("/healthz").status != 200:
            raise Failed("the dashboard is not serving yet")
        admin.get("/sign-in")
        name, _ = who()
        signed = admin.post("/sign-in", {"name": name, "password": drawn_password()})
        if signed.status != 303:
            raise Failed(f"signing in as {name}: {signed.status} {sentence(signed)}")
        if AS is not None:
            return admin
        settings = admin.get("/admin")
        if settings.status != 200:
            raise Failed(f"{ADMIN} is not yet the deployment's admin: /admin {settings.status}")
        return admin
    return until(seconds, attempt, f"{who()[0]} was not signed in")


def plugin_line(page):
    """The plugin's row in the admin portal's list of plugins, or None."""
    table = re.search(r'<table class="list plugins"[^>]*>(.*?)</table>', page.body, re.S)
    if table is None:
        return None
    rows = table.group(1)
    found = re.search(r'<tr data-id="' + re.escape(INSTANCE) + r'">(.*?)</tr>', rows, re.S)
    return found.group(1) if found else None


def ready(args):
    seconds = number(args, "--seconds", 120)
    admin = signed_in(seconds)

    def registered():
        line = plugin_line(admin.get("/admin"))
        if line is None:
            raise Failed(f"{INSTANCE} is not listed: its sidecar has not reported")
        if ">Healthy<" in line or ">Not healthy<" in line:
            return "healthy" if ">Healthy<" in line else "not healthy"
        state = re.search(r'class="badge[^"]*">([^<]+)<', line)
        raise Failed(f"{INSTANCE} is listed, {state.group(1) if state else 'and not registered'}")

    said = until(seconds, registered, f"{INSTANCE} did not register")
    print(f"ready: {INSTANCE} is registered and {said}")


def settings(args):
    seconds = number(args, "--seconds", 60)
    wanted = option(args, "--expect", None)
    asked = pairs(args)
    if args or not asked:
        raise Failed("settings takes NAME=VALUE, at least one")
    admin = signed_in()
    # A table setting's cell is NAME[ROW].COLUMN=VALUE (contract v14): the
    # rows given are the table's whole, as the form posts it.
    base = lambda name: name.split("[", 1)[0]

    # A table setting is on a tab of its own beside Settings (core 2026-10-05),
    # its grid named for it; every other setting is on Settings.
    def page(name):
        tab = f"setting-{base(name)}" if "[" in name else "settings"
        return admin.get(f"{VIEW}?tab={tab}")

    def offers(name, body):
        if "[" in name:
            return f'name="table.{base(name)}"' in body
        return f'data-setting="{name}"' in body

    def declared():
        form = admin.get(f"{VIEW}?tab=settings")
        if form.status != 200:
            raise Failed(f"its settings: {form.status} {sentence(form)}")
        missing = sorted({base(name) for name, _ in asked if not offers(name, page(name).body)})
        if missing:
            raise Failed(f"the plugin has not declared {', '.join(missing)}")
        return form

    form = until(seconds, declared, "the settings form never offered them")
    fields = {"form_token": form_token(form)}
    for name, value in asked:
        if "[" in name:
            fields[f"table.{base(name)}"] = "1"
            fields[f"table.{name}"] = value
            continue
        secret = f'name="secret.{name}"' in form.body
        fields[("secret." if secret else "value.") + name] = value
    saved = admin.post(f"{VIEW}/settings", fields)
    if saved.status != 303:
        raise Failed(f"the settings were not saved: {saved.status} {sentence(saved)}")
    print(f"settings: saved {', '.join(sorted({base(name) for name, _ in asked}))}")
    if wanted is not None:
        shown = admin.get(f"{VIEW}?tab=settings")
        if wanted not in shown.body:
            raise Failed(f"the settings page does not say {wanted!r}")
        said = re.search(r"data-last-changed>([^<]*)<", shown.body)
        print(f"settings: {said.group(1) if said else wanted}")


def account(args):
    seconds = number(args, "--seconds", 60)
    if len(args) != 1:
        raise Failed("account takes a name")
    name = args[0]
    admin = signed_in()
    page = admin.get("/admin")
    done = admin.post("/admin/accounts",
                      {"account_id": "", "name": name, "form_token": form_token(page)})
    if done.status != 303:
        raise Failed(f"the account was not defined: {done.status} {sentence(done)}")

    def listed():
        table = admin.get("/admin").body.split("<h2>Accounts</h2>", 1)[-1].split("</table>", 1)[0]
        found = re.search(r'<tr data-id="([^"]+)" data-name="' + re.escape(html.escape(name)) + '"', table)
        if not found:
            raise Failed(f"{name} is not listed yet")
        return found.group(1)

    print(until(seconds, listed, f"{name} was never listed"))


def session_at(admin, level):
    """A session on the plugin's own host at `level`, entered as the
    dashboard's frame enters it."""
    if level not in LEVELS:
        raise Failed(f"--level is one of {', '.join(LEVELS)}, not {level!r}")
    opened = admin.get(f"/plugins/{INSTANCE}/enter?level={level}")
    if opened.status != 303:
        raise Failed(f"no session at {level}: {opened.status} {sentence(opened)}")
    host = Browser(PLUGIN_HOST)
    prefix = f"http://{PLUGIN_HOST}"
    redeemed = host.get((opened.location or "")[len(prefix):])
    if redeemed.status != 303:
        raise Failed(f"the plugin's host did not redeem the session: {redeemed.status}")
    return host


def followed(host, reply):
    """A reply, its redirects within the plugin's host followed, as a
    browser follows them."""
    for _ in range(5):
        if reply.status not in (301, 302, 303, 307, 308) or not reply.location:
            return reply
        where = urllib.parse.urlparse(reply.location)
        if where.netloc and where.netloc != PLUGIN_HOST:
            return reply
        reply = host.get(where.path + (f"?{where.query}" if where.query else ""))
    return reply


def page(args):
    level = option(args, "--level", "admin")
    text = option(args, "--until", None)
    seconds = number(args, "--seconds", 60)
    if len(args) != 1:
        raise Failed("page takes one path")
    path = args[0]
    admin = signed_in()
    host = session_at(admin, level)

    def answered():
        reply = followed(host, host.get(path))
        if text is not None and text not in reply.body:
            raise Failed(f"{path} answered {reply.status} without {text!r}")
        return reply

    reply = until(seconds if text is not None else 0, answered, f"{path} never said {text!r}")
    print(reply.status)
    print(reply.body)


def form(args):
    level = option(args, "--level", "admin")
    shown = option(args, "--page", None)
    target = option(args, "--post", None)
    csrf = option(args, "--csrf-field", "csrf")
    text = option(args, "--expect", None)
    taken = [name for value in options(args, "--from-page") for name in value.split(",") if name]
    if not shown or not target:
        raise Failed("form takes --page PATH and --post PATH")
    fields = pairs(args)
    if args:
        raise Failed(f"form takes FIELD=VALUE, not {args[0]!r}")
    admin = signed_in()
    host = session_at(admin, level)
    read = followed(host, host.get(shown))
    if read.status != 200:
        raise Failed(f"{shown}: {read.status} {sentence(read)}")
    given = {name for name, _ in fields}
    for name in taken:
        if name in given:
            continue
        value = field_of(read.body, name)
        if value is None:
            raise Failed(f"{shown} has no field {name!r} to take")
        fields.append((name, value))
        given.add(name)
    if csrf not in given:
        token = field_of(read.body, csrf)
        if token is not None:
            fields.append((csrf, token))
    reply = followed(host, host.post(target, fields))
    print(reply.status)
    print(reply.body)
    if reply.status >= 400:
        raise Failed(f"{target} refused the form: {reply.status}")
    if text is not None and text not in reply.body:
        raise Failed(f"{target} did not say {text!r}")


def field_of(body, name):
    """The value the page's first form field named `name` holds, as a
    browser would post it: an input's value, a textarea's text, or a
    select's selected option (else its first); None when it has none."""
    named = r'\bname="' + re.escape(html.escape(name)) + '"'
    for field in re.finditer(r"<(input|textarea|select)\b([^>]*)>", body):
        kind, attributes = field.group(1), field.group(2)
        if not re.search(named, attributes):
            continue
        if kind == "input":
            found = re.search(r'\bvalue="([^"]*)"', attributes)
            return html.unescape(found.group(1)) if found else ""
        if kind == "textarea":
            text = body[field.end():].split("</textarea>", 1)[0]
            return html.unescape(text)
        listed = re.findall(r"<option\b([^>]*)>([^<]*)",
                              body[field.end():].split("</select>", 1)[0])
        chosen = next((o for o in listed if re.search(r"\bselected\b", o[0])),
                      listed[0] if listed else None)
        if chosen is None:
            return ""
        found = re.search(r'\bvalue="([^"]*)"', chosen[0])
        return html.unescape(found.group(1) if found else chosen[1].strip())
    return None


def unlinked(args):
    wanted = option(args, "--expect", None)
    seconds = number(args, "--seconds", 60)
    if args:
        raise Failed(f"unlinked takes no {args[0]!r}")
    admin = signed_in()

    def counted():
        line = plugin_line(admin.get("/admin"))
        if line is None:
            raise Failed(f"{INSTANCE} is not listed")
        found = re.search(r'data-flag="unlinked" data-count="(\d+)"', line)
        count = int(found.group(1)) if found else 0
        if wanted is not None and count != int(wanted):
            raise Failed(f"the dashboard counts {count}")
        return count

    print(until(seconds if wanted is not None else 0, counted,
                f"the dashboard never counted {wanted} not linked"))


def grant(args):
    level = option(args, "--level", None)
    to = option(args, "--to", None)
    accounts = [a for a in (option(args, "--accounts", "") or "").split(",") if a]
    if level not in LEVELS:
        raise Failed(f"grant takes --level {'|'.join(LEVELS)}")
    if args:
        raise Failed(f"grant takes no {args[0]!r}")
    global AS
    acting, AS = AS, None
    try:
        admin = signed_in()
    finally:
        AS = acting
    holder, login = (ADMIN, LOGIN) if to is None else (to, f"local|{to}")

    def post(path, fields):
        token = form_token(admin.get("/admin"))
        done = admin.send("POST", path, fields + [("form_token", token)])
        if done.status != 303:
            raise Failed(f"{path}: {done.status} {sentence(done)}")

    def defined(path, field, name, fields):
        """The group named `name`, defined if it is not: the conductor mints a
        new group's identifier, and an identifier sent names one to edit."""
        found = minted(name)
        post(path, ([(field, found)] if found else []) + [("name", name)] + fields)
        found = minted(name)
        if not found:
            raise Failed(f"{path}: {name!r} is not listed once defined")
        return found

    def minted(name):
        page = admin.get("/admin").body
        found = re.search(r'<tr data-id="([^"]+)" data-name="' + re.escape(html.escape(name)) + '"',
                          page)
        return found.group(1) if found else ""

    user_group = defined("/admin/user-groups", "user_group_id",
                         "Harness admin" if to is None else f"Harness {to}",
                         [("login", login)])
    access_group = defined("/admin/access-groups", "access_group_id",
                           f"Harness {level} on {INSTANCE}",
                           [("plugin", INSTANCE), (f"level.{INSTANCE}", level)])
    # An admin reaches no account's data, so its permission names none.
    account_group = "" if level == "admin" else ALL_ACCOUNTS
    if accounts and level != "admin":
        account_group = defined("/admin/account-groups", "account_group_id",
                                f"Harness accounts of {holder}",
                                [("account_ids", account) for account in accounts])
    post("/admin/permissions", [("user_group_id", user_group),
                                ("account_group_id", account_group),
                                ("access_group_id", access_group)])
    print(f"grant: {holder} holds {level} on {INSTANCE}, on "
          + ("no account" if level == "admin" else ", ".join(accounts) or "All accounts"))


def instrument_rows(admin):
    """The Instruments page's records: each ID, its identifiers as listed, and
    whether the book can use it."""
    page = admin.get("/admin/instruments")
    if page.status != 200:
        raise Failed(f"/admin/instruments: {page.status} {sentence(page)}")
    table = re.search(r'<table class="list" id="instruments-table">(.*?)</table>', page.body, re.S)
    rows = []
    for row in re.findall(r"<tr>(.*?)</tr>", table.group(1) if table else "", re.S):
        found = re.search(r'<a href="/admin/instruments/([^"]+)">', row)
        if not found:
            continue
        listed = re.search(r'<span class="id">(.*?)</span>', row, re.S)
        rows.append((html.unescape(found.group(1)),
                     html.unescape(listed.group(1)) if listed else "",
                     "the book cannot use it" not in row))
    return rows


def instruments(args):
    wanted = option(args, "--expect", None)
    seconds = number(args, "--seconds", 60)
    if args:
        raise Failed(f"instruments takes no {args[0]!r}")
    admin = signed_in()

    def listed():
        waiting = [row for row in instrument_rows(admin) if not row[2]]
        if wanted is not None and len(waiting) != int(wanted):
            raise Failed(f"the page lists {len(waiting)} the book cannot use")
        return waiting

    for instrument_id, identifiers, _ in until(seconds if wanted is not None else 0, listed,
                                               f"the page never listed {wanted}"):
        print(f"{instrument_id}\t{identifiers}")


def instrument(args):
    by_identifier = option(args, "--identifier", None)
    seconds = number(args, "--seconds", 60)
    asked = dict(pairs(args))
    named = args.pop(0) if args else None
    if args or (named is None) == (by_identifier is None):
        raise Failed("instrument takes an ID or --identifier TEXT, and NAME=VALUE")
    source = asked.get("source", "")
    if asked.get("asset_class") not in CLASSES or not asked.get("currency") or not source:
        raise Failed(f"instrument takes asset_class={'|'.join(CLASSES)}, currency=CODE and source=TEXT")
    admin = signed_in()

    def found():
        for instrument_id, identifiers, _ in instrument_rows(admin):
            if instrument_id == named or (by_identifier and by_identifier in identifiers):
                return instrument_id
        raise Failed(f"no record listed is {named or by_identifier!r}")

    instrument_id = until(seconds, found, "the record was never listed")
    page = admin.get(f"/admin/instruments/{urllib.parse.quote(instrument_id)}")
    version = re.search(r'name="against_version" value="(\d+)"', page.body)
    if page.status != 200 or version is None:
        raise Failed(f"{instrument_id}'s page: {page.status} {sentence(page)}")
    fields = {
        "form_token": form_token(page),
        "instrument_id": instrument_id,
        "against_version": version.group(1),
        "asset_class": str(CLASSES[asked["asset_class"]]),
        "asset_class_source": source,
        "currency": asked["currency"],
        "currency_source": source,
        "note": asked.get("note", ""),
    }
    if asked.get("description"):
        fields["description"] = asked["description"]
        fields["description_source"] = source
    if asked.get("instrument_type"):
        if asked["instrument_type"] not in TYPES:
            raise Failed(f"instrument_type is {'|'.join(TYPES)}")
        fields["instrument_type"] = str(TYPES[asked["instrument_type"]])
        fields["instrument_type_source"] = source
    stated = [name for name in FUND if asked.get(name)]
    if stated:
        if len(stated) != len(FUND):
            raise Failed(f"a money market fund's attributes are stated together: {', '.join(FUND)}")
        for name, words in FUND.items():
            if asked[name] not in words:
                raise Failed(f"{name} is {'|'.join(words)}")
            fields[name] = str(words[asked[name]])
        fields["fund_source"] = source
    done = admin.post("/admin/instruments/complete", fields)
    if done.status != 303:
        raise Failed(f"{instrument_id} was not completed: {done.status} {sentence(done)}")
    print(f"instrument: {instrument_id} completed, {asked['asset_class']} in {asked['currency']}")


def option(args, name, default):
    """Takes `name VALUE` out of `args`."""
    if name not in args:
        return default
    at = args.index(name)
    if at + 1 >= len(args):
        raise Failed(f"{name} needs a value")
    value = args[at + 1]
    del args[at:at + 2]
    return value


def options(args, name):
    """Takes every `name VALUE` out of `args`, in order."""
    found = []
    while name in args:
        found.append(option(args, name, None))
    return found


def number(args, name, default):
    value = option(args, name, None)
    if value is None:
        return default
    if not value.isdigit():
        raise Failed(f"{name} takes a number of seconds, not {value!r}")
    return int(value)


def pairs(args):
    """Takes every NAME=VALUE out of `args`, in order."""
    taken = [arg for arg in args if "=" in arg and not arg.startswith("-")]
    for arg in taken:
        args.remove(arg)
    return [tuple(arg.split("=", 1)) for arg in taken]


# What a plugin instance may be named: a broker user, a host name below the
# dashboard's and a compose service, so lower case, digits and inner hyphens.
INSTANCE_NAME = re.compile(r"[a-z](?:[a-z0-9-]{0,30}[a-z0-9])?")
ROLE_NAME = re.compile(r"[a-z][a-z0-9-]*")
# Names the deployment already holds: compose.yaml's services, and the broker
# users that are not plugins. A sidecar is the service `sidecar-<instance>`.
TAKEN = {"keys", "postgres", "broker-config", "nats", "migrate", "street", "bor",
         "instrument", "conductor", "dashboard", "runner", "store", "storage",
         "runtime", "first-run", "dashboard-1"}
RUNTIME_IMAGE = "${MERIDIAN_RUNTIME_IMAGE:?set MERIDIAN_RUNTIME_IMAGE to the runtime image of the harness's commit}"
# The roles whose plugins own storage for their raw external records
# (decisions/028), as the chart's `meridian-runtime.edgeRoles` says them; a
# plugin holding any is given a volume of its own, where the chart gives it a
# claim of its own, at the same path.
EDGE_ROLES = ("ccm", "custody", "dgm", "match", "reporting", "servicing", "settlement")
STORAGE_DIR = "/var/lib/meridian/storage"


def plugins_listed(text):
    """The plugins a compose file is written for, checked: a non-empty JSON
    list of {instance, image, roles}, each instance named once."""
    try:
        listed = json.loads(text)
    except ValueError as unread:
        raise Failed(f"the plugins are not JSON: {unread}") from None
    if not isinstance(listed, list) or not listed:
        raise Failed('the plugins are a JSON list, at least one: [{"instance": ..., "image": ..., "roles": [...]}]')
    seen = set()
    for at, plugin in enumerate(listed):
        where = f"plugin {at + 1}"
        if not isinstance(plugin, dict) or set(plugin) != {"instance", "image", "roles"}:
            raise Failed(f"{where} is an object of instance, image and roles, and nothing else")
        name, image, roles = plugin["instance"], plugin["image"], plugin["roles"]
        if not isinstance(name, str) or not INSTANCE_NAME.fullmatch(name):
            raise Failed(f"{where}'s instance {name!r} is not lower case letters, digits and inner hyphens, "
                         "at most 32, starting with a letter")
        if name in TAKEN or name.startswith("sidecar-"):
            raise Failed(f"{where}'s instance {name!r} is a name the harness already holds")
        if name in seen:
            raise Failed(f"{where}'s instance {name!r} is named twice")
        seen.add(name)
        if not isinstance(image, str) or not image or re.search(r"[\s$]", image):
            raise Failed(f"{where}'s image {image!r} is not an image reference")
        if not isinstance(roles, list) or not all(isinstance(r, str) and ROLE_NAME.fullmatch(r) for r in roles):
            raise Failed(f"{where}'s roles are a list of role names, empty for a plugin holding none")
    return listed


def compose(args):
    """The plugins' half of the deployment, as a compose file (JSON, which
    compose reads as YAML), on stdout."""
    if args:
        raise Failed(f"compose takes the plugins on stdin, not {args[0]!r}")
    listed = plugins_listed(sys.stdin.read())
    names = [plugin["instance"] for plugin in listed]
    instances = [{"instance_id": plugin["instance"], "roles": plugin["roles"]} for plugin in listed]
    instances.append({"instance_id": "dashboard-1", "component": "dashboard"})
    stored = [plugin["instance"] for plugin in listed
              if any(role in EDGE_ROLES for role in plugin["roles"])]
    services = {
        "keys": {"environment": {"MERIDIAN_HARNESS_BROKER_USERS": " ".join(names)}},
        "broker-config": {"environment": {
            "MERIDIAN_HARNESS_INSTANCES": json.dumps({"instances": instances})}},
        "runner": {"environment": {"MERIDIAN_HARNESS_INSTANCE": names[0]}},
    }
    if stored:
        # Each edge plugin's volume made writable before its plugin starts,
        # whichever user the plugin's image runs as: a fresh volume is
        # root's, and only the plugin's own container mounts it after this.
        services["storage"] = {
            "image": "alpine/openssl:3.3.2",
            "entrypoint": ["sh", "-c"],
            "command": ["chmod 0777 /storage/*"],
            "volumes": [f"storage-{name}:/storage/{name}" for name in stored],
            "networks": ["harness"],
        }
    for plugin in listed:
        name = plugin["instance"]
        # Its sidecar, as instance `name` holding the plugin's roles. No
        # registration step: a sidecar with an instance and a broker
        # credential is enough, and the conductor learns what the plugin
        # declares from its report. The dashboard's front door address makes
        # `sidecar-<instance>` of the instance, which is this service's name.
        services[f"sidecar-{name}"] = {
            "image": RUNTIME_IMAGE,
            "entrypoint": ["sh", "-c"],
            "command": [
                "set -e\n"
                f"broker=$$(cat /secrets/nats/{name})\n"
                f"export MERIDIAN_BROKER_URL=nats://{name}:$$broker@nats:4222\n"
                "exec meridian-sidecar\n"],
            "depends_on": {"nats": {"condition": "service_started"},
                           "keys": {"condition": "service_completed_successfully"}},
            "environment": {
                "MERIDIAN_DEPLOYMENT_ID": "DEP-harness",
                "MERIDIAN_PLUGIN_INSTANCE_ID": name,
                "MERIDIAN_PLUGIN_ROLES": ",".join(plugin["roles"]),
                "MERIDIAN_INSTANCE_ID": f"sidecar-{name}",
                "MERIDIAN_FRONT_DOOR_ADDRESS": "0.0.0.0:9292",
                "MERIDIAN_DASHBOARD_KEYS_DIR": "/keys/public",
            },
            "volumes": ["keys:/keys:ro", "secrets:/secrets:ro"],
            "networks": ["harness"],
        }
        # The plugin, in its sidecar's network namespace as a plugin is in
        # its sidecar's pod, and started again when it exits failing, as a
        # pod's container is: a plugin whose first call comes before the
        # deployment serves is refused, exits, and on its next start finds it
        # serving (kernel/the-harness-restarts-its-plugin). A plugin adds
        # environment, a command or a volume with its own `-f` override of
        # this service.
        services[name] = {
            "image": plugin["image"],
            "depends_on": {f"sidecar-{name}": {"condition": "service_started"}},
            "environment": {"MERIDIAN_SIDECAR_ADDRESS": "127.0.0.1:9191"},
            "network_mode": f"service:sidecar-{name}",
            "restart": "on-failure",
        }
        # An edge plugin's storage (decisions/028): its own volume, which a
        # restart or a recreated container keeps and `down -v` removes.
        if name in stored:
            services[name]["depends_on"]["storage"] = {"condition": "service_completed_successfully"}
            services[name]["environment"]["MERIDIAN_STORAGE_DIR"] = STORAGE_DIR
            services[name]["volumes"] = [f"storage-{name}:{STORAGE_DIR}"]
    written = {
        "x-harness": "written by harness.py compose for " + ", ".join(names)
                     + "; written again, never edited, for another list",
        "services": services,
    }
    if stored:
        written["volumes"] = {f"storage-{name}": {} for name in stored}
    print(json.dumps(written, indent=2))


# ── An MCP client, as an agent's (W6.17, W6.20, contract v12) ───────────────

STATE = os.environ.get("MERIDIAN_HARNESS_STATE", "/state")
MCP_STATE = os.path.join(STATE, "mcp.json")


def mcp_state_for(client):
    """Where a client keeps its token pair: the admin's harness agent where
    it always has, any other person's or client's beside it."""
    if AS is None and client == "harness agent":
        return os.path.join(STATE, "mcp.json")
    slug = re.sub(r"[^a-z0-9]+", "-", f"{AS or 'admin'} {client}".lower()).strip("-")
    return os.path.join(STATE, f"mcp-{slug}.json")
CALLBACK = "http://127.0.0.1:53682/callback"
ENUM_CLASSES = {name: "ASSET_CLASS_" + name.upper() for name in CLASSES}
ENUM_FUND = {
    "fund_category": ("category", "MONEY_MARKET_FUND_CATEGORY_"),
    "fund_investors": ("investors", "MONEY_MARKET_FUND_INVESTORS_"),
    "fund_nav": ("nav", "MONEY_MARKET_FUND_NAV_"),
    "fund_liquidity_fee": ("liquidity_fee", "LIQUIDITY_FEE_REGIME_"),
}


def send_json(method, path, body=None, form=None, bearer=None):
    """A JSON (or form) request to the dashboard: its status, its JSON or
    None, and where it redirects."""
    data, kind = None, None
    if body is not None:
        data, kind = json.dumps(body).encode(), "application/json"
    elif form is not None:
        data, kind = urllib.parse.urlencode(form).encode(), "application/x-www-form-urlencoded"
    request = urllib.request.Request(DASHBOARD + path, data=data, method=method)
    if kind:
        request.add_header("Content-Type", kind)
    if bearer:
        request.add_header("Authorization", f"Bearer {bearer}")
    try:
        response = urllib.request.build_opener(NoRedirect).open(request, timeout=60)
        status, raw, headers = response.status, response.read(), response.headers
    except urllib.error.HTTPError as refused:
        status, raw, headers = refused.code, refused.read(), refused.headers
    try:
        said = json.loads(raw) if raw else None
    except ValueError:
        said = None
    return status, said, headers.get("Location")


def mcp_state():
    try:
        with open(MCP_STATE, encoding="utf-8") as kept:
            return json.load(kept)
    except OSError:
        raise Failed("no MCP client is connected: run `mcp connect` first") from None


def keep_state(state):
    os.makedirs(STATE, exist_ok=True)
    with open(MCP_STATE + ".new", "w", encoding="utf-8") as kept:
        json.dump(state, kept)
    os.replace(MCP_STATE + ".new", MCP_STATE)


def mcp_connect(args):
    covers = options(args, "--covers")
    client_name = option(args, "--client", "harness agent")
    if args:
        raise Failed(f"mcp connect takes no {args[0]!r}")
    status, said, _ = send_json("POST", "/oauth/register",
                                {"client_name": client_name, "redirect_uris": [CALLBACK]})
    if status != 201 or not said:
        raise Failed(f"the client was not registered: {status} {said}")
    client_id = said["client_id"]
    verifier = secrets.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
    query = urllib.parse.urlencode({
        "response_type": "code", "client_id": client_id, "redirect_uri": CALLBACK,
        "code_challenge": challenge, "code_challenge_method": "S256", "state": "harness",
        "resource": DASHBOARD + "/mcp",
    })
    person = Browser()
    asked = person.get(f"/oauth/authorize?{query}")
    found = re.search(r'name="authorize" value="([^"]+)"', asked.body)
    if asked.status != 200 or not found:
        raise Failed(f"the authorisation did not ask her to sign in: {asked.status} {sentence(asked)}")
    consent = person.post("/sign-in", {"name": who()[0], "password": drawn_password(),
                                       "authorize": found.group(1)})
    request = re.search(r'name="request" value="([^"]+)"', consent.body)
    confirm = re.search(r'name="confirm" value="([^"]+)"', consent.body)
    if consent.status != 200 or not request or not confirm:
        raise Failed(f"she was not shown the consent page: {consent.status} {sentence(consent)}")
    fields = [("request", request.group(1)), ("confirm", confirm.group(1)), ("decision", "allow"),
              ("covers", "some"), ("days", "30")]
    # Every account she may tick: All accounts once she holds a grant on it.
    for offered in re.findall(r'name="account_group" value="([^"]+)"', consent.body):
        fields.append(("account_group", html.unescape(offered)))
    for covered in covers:
        if covered == "deployment_admin":
            fields.append(("deployment_admin", "1"))
        elif re.fullmatch(r"[a-z0-9-]+:(admin|write|read)", covered):
            fields.append(("level", covered))
        else:
            raise Failed(f"--covers is deployment_admin or INSTANCE:LEVEL, not {covered!r}")
    data = urllib.parse.urlencode(fields).encode()
    decided = urllib.request.Request(DASHBOARD + "/oauth/authorize", data=data, method="POST")
    decided.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        response = urllib.request.build_opener(NoRedirect).open(decided, timeout=30)
        status, location = response.status, response.headers.get("Location")
    except urllib.error.HTTPError as refused:
        status, location = refused.code, refused.headers.get("Location")
    code = urllib.parse.parse_qs(urllib.parse.urlsplit(location or "").query).get("code", [""])[0]
    if status != 302 or not code:
        offered = re.findall(r'name="(?:level|account_group|deployment_admin)" value="([^"]+)"',
                             consent.body)
        raise Failed(f"the consent did not send back a code: {status} {location}; it offered {offered}")
    status, issued, _ = send_json("POST", "/oauth/token", form={
        "grant_type": "authorization_code", "code": code, "code_verifier": verifier,
        "redirect_uri": CALLBACK, "client_id": client_id, "resource": DASHBOARD + "/mcp"})
    if status != 200 or not issued:
        raise Failed(f"the code was not exchanged: {status} {issued}")
    keep_state({"client_id": client_id, "access": issued["access_token"],
                "refresh": issued["refresh_token"], "delegation": issued["delegation_id"],
                "saved": {}})
    tools = rpc("tools/list")["tools"]
    print(f"mcp: connected through delegation {issued['delegation_id']}, {len(tools)} tools")


def rpc(method, params=None):
    """One JSON-RPC message on the delegation, refreshing once on a 401."""
    state = mcp_state()
    message = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}
    status, said, _ = send_json("POST", "/mcp", message, bearer=state["access"])
    if status == 401:
        refreshed, issued, _ = send_json("POST", "/oauth/token", form={
            "grant_type": "refresh_token", "refresh_token": state["refresh"],
            "client_id": state["client_id"]})
        if refreshed != 200 or not issued:
            raise Failed(f"the token pair was not refreshed: {refreshed} {issued}")
        state["access"], state["refresh"] = issued["access_token"], issued["refresh_token"]
        keep_state(state)
        status, said, _ = send_json("POST", "/mcp", message, bearer=state["access"])
    if status != 200 or not said or "result" not in said:
        raise Failed(f"/mcp {method}: {status} {said}")
    return said["result"]


def mcp_list(args):
    wanted = options(args, "--expect")
    if args:
        raise Failed(f"mcp list takes no {args[0]!r}")
    names = [tool["name"] for tool in rpc("tools/list")["tools"]]
    missing = [name for name in wanted if name not in names]
    for name in names:
        print(name)
    if missing:
        raise Failed(f"the delegation does not reach {', '.join(missing)}")


STEP = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)((?:\[[^\]]*\])*)")


SET = re.compile(r"((?:[^=\[]|\[[^\]]*\])+)=(.*)", re.S)


def put(tree, path, value):
    """Sets `path` in `tree`, by the dictionary's grammar, `[key=value]`
    picking the row whose key is that value; a missing row is added."""
    node = tree
    parts = path.split(".")
    for depth, part in enumerate(parts):
        matched = STEP.fullmatch(part)
        if not matched:
            raise Failed(f"{path!r} is not a path")
        name, indices = matched.group(1), re.findall(r"\[([^\]]*)\]", matched.group(2))
        last = depth == len(parts) - 1
        if not indices:
            if last:
                node[name] = value
                return
            node = node.setdefault(name, {})
            continue
        rows = node.setdefault(name, [])
        for at, index in enumerate(indices):
            if "=" in index:
                key, wanted = index.split("=", 1)
                found = [row for row in rows if str(row.get(key)) == wanted]
                if not found:
                    raise Failed(f"no row of {name} has {key} {wanted!r}")
                row = found[0]
            else:
                number = int(index)
                while len(rows) <= number:
                    rows.append({})
                row = rows[number]
            if last and at == len(indices) - 1:
                raise Failed(f"{path!r} names a row, not a field")
            node = row


def pick(tree, path):
    for part in [p for p in path.split(".") if p]:
        matched = STEP.fullmatch(part)
        if not matched:
            raise Failed(f"{path!r} is not a path")
        tree = tree[matched.group(1)]
        for index in re.findall(r"\[([^\]]*)\]", matched.group(2)):
            tree = tree[int(index)]
    return tree


def mcp_call(args):
    taken = option(args, "--from", None)
    sets = options(args, "--set")
    save = option(args, "--save", None)
    outcome = option(args, "--expect-outcome", None)
    expected = options(args, "--expect")
    name = args.pop(0) if args else None
    given = args.pop(0) if args else None
    if not name or args:
        raise Failed("mcp call takes a tool's name, and its arguments as JSON")
    state = mcp_state()
    arguments = {}
    if taken:
        saved, _, at = taken.partition(".")
        if saved not in state["saved"]:
            raise Failed(f"nothing was saved as {saved!r}")
        arguments = json.loads(json.dumps(pick(state["saved"][saved], at)))
    if given:
        arguments.update(json.loads(given))
    for each in sets:
        # The path ends at the first = outside its brackets, so a row picked
        # by `[key=value]` is one path: positions[label=BTC].lots[0].cost=1.
        assigned = SET.fullmatch(each)
        if not assigned:
            raise Failed(f"--set takes PATH=VALUE, not {each!r}")
        put(arguments, assigned.group(1), assigned.group(2))
    result = rpc("tools/call", {"name": name, "arguments": arguments})
    answer = result.get("structuredContent") or {}
    text = json.dumps(answer, indent=1, sort_keys=True)
    print(text)
    if save:
        state = mcp_state()
        state["saved"][save] = answer
        keep_state(state)
    if outcome and answer.get("outcome") != outcome:
        raise Failed(f"{name} answered {answer.get('outcome')}, not {outcome}: {answer.get('detail', '')}")
    for words in expected:
        if words not in text:
            raise Failed(f"{name}'s answer does not say {words!r}")


def identifiers_said(record):
    said = []
    for identifier in record.get("identifiers", []):
        if identifier.get("source"):
            said.append(f"{identifier['scheme']} ({identifier['source']}): {identifier['value']}")
        else:
            said.append(f"{identifier['scheme']}: {identifier['value']}")
    return "; ".join(said)


def mcp_complete(args):
    by_identifier = option(args, "--identifier", None)
    asked = dict(pairs(args))
    if args or not by_identifier:
        raise Failed("mcp complete takes --identifier TEXT and NAME=VALUE")
    if asked.get("asset_class") not in CLASSES or not asked.get("currency") \
            or not asked.get("source") or not asked.get("note"):
        raise Failed("mcp complete takes asset_class=, currency=, source= and note=")
    found, cursor = None, ""
    while found is None:
        page = rpc("tools/call", {"name": "dashboard__list_instruments_to_complete",
                                  "arguments": {"cursor": cursor, "page_size": 500}})
        data = (page.get("structuredContent") or {}).get("data") or {}
        for record in data.get("records", []):
            if by_identifier in identifiers_said(record):
                found = record
                break
        cursor = data.get("next_cursor", "")
        if found is None and not cursor:
            raise Failed(f"no record listed is {by_identifier!r}")
    values = [{"asset_class": ENUM_CLASSES[asked["asset_class"]]}, {"currency": asked["currency"]}]
    if asked.get("instrument_type"):
        values.append({"instrument_type": "INSTRUMENT_TYPE_" + asked["instrument_type"].upper()})
    stated = {part: prefix + asked[name].upper() for name, (part, prefix) in ENUM_FUND.items()
              if asked.get(name)}
    if stated:
        values.append({"money_market_fund": stated})
    result = rpc("tools/call", {"name": "dashboard__complete_instruments", "arguments": {
        "completions": [{"instrument_id": found["instrument_id"],
                         "against_version": found["version"], "values": values,
                         "source": asked["source"], "note": asked["note"]}]}})
    answer = result.get("structuredContent") or {}
    if answer.get("outcome") != "made":
        raise Failed(f"{found['instrument_id']} was not completed: {json.dumps(answer)}")
    print(f"mcp complete: {found['instrument_id']} completed, {asked['asset_class']} in "
          f"{asked['currency']}, through the delegation")


def mcp_calls(args):
    wanted = option(args, "--expect", None)
    if args:
        raise Failed(f"mcp calls takes no {args[0]!r}")
    admin = signed_in()
    page = admin.get("/delegations")
    table = re.search(r'<table class="calls">(.*?)</table>', page.body, re.S)
    count = len(re.findall(r"<tr data-outcome=", table.group(1))) if table else 0
    print(count)
    if wanted is not None and count < int(wanted):
        raise Failed(f"Connected clients lists {count} calls, fewer than {wanted}")


# ── Tickets and the inbox, as a person on the page (W6.21 to W6.24) ────────

def json_get(browser, path):
    """A row's JSON, as the page's own session asks it."""
    request = urllib.request.Request(DASHBOARD + path, method="GET")
    request.add_header("Accept", "application/json")
    request.add_header("Cookie", "; ".join(f"{k}={v}" for k, v in browser.cookies.items()))
    try:
        response = urllib.request.build_opener(NoRedirect).open(request, timeout=30)
        status, raw = response.status, response.read()
    except urllib.error.HTTPError as refused:
        status, raw = refused.code, refused.read()
    try:
        return status, json.loads(raw) if raw else None
    except ValueError:
        return status, None


def ticket_file(args):
    concerns = option(args, "--concerns", None)
    refused_at = option(args, "--expect-refused", None)
    fields = dict(pairs(args))
    if args:
        raise Failed(f"ticket file takes NAME=VALUE, not {args[0]!r}")
    person = signed_in()
    asked = (f"/tickets/new?concerns={concerns}" if concerns
             else f"/tickets/new?concerns=plugin&instance={INSTANCE}")
    form_page = person.get(asked)
    if form_page.status != 200:
        raise Failed(f"Report a problem: {form_page.status} {sentence(form_page)}")
    sent = {"form_token": form_token(form_page), "kind": fields.pop("kind", "defect"),
            "title": fields.pop("title", ""), "seen": fields.pop("seen", "")}
    if concerns:
        sent["concerns"] = concerns
    else:
        sent["concerns"], sent["instance"] = "plugin", INSTANCE
    sent.update(fields)
    filed = person.post("/tickets", sent)
    if refused_at is not None:
        if filed.status < 400 or refused_at not in filed.body:
            raise Failed(f"the filing was not refused naming {refused_at}: {filed.status} {sentence(filed)}")
        print(f"ticket file: refused, {sentence(filed)}")
        return
    if filed.status != 303 or not (filed.location or "").startswith("/tickets/TKT-"):
        raise Failed(f"the ticket was not filed: {filed.status} {sentence(filed)}")
    print(filed.location.split("?", 1)[0].rsplit("/", 1)[1])


def ticket_list(args):
    concerns = option(args, "--concerns", "")
    state = option(args, "--state", "")
    wanted = option(args, "--expect", None)
    seconds = number(args, "--seconds", 30)
    if args:
        raise Failed(f"ticket list takes no {args[0]!r}")
    person = signed_in()
    query = urllib.parse.urlencode({"concerns": concerns, "state": state})

    def listed():
        status, said = json_get(person, f"/tickets?{query}")
        if status != 200 or said is None:
            raise Failed(f"/tickets: {status}")
        tickets = said["tickets"]
        if wanted is not None and len(tickets) != int(wanted):
            raise Failed(f"it lists {len(tickets)}")
        return tickets

    tickets = until(seconds if wanted is not None else 0, listed, f"the list never held {wanted}")
    for ticket in tickets:
        print(f"{ticket['ticket_id']}\t{ticket['state']}\t{json.dumps(ticket['title'])}")


def ticket_read(args):
    as_page = "--page" in args
    if as_page:
        args.remove("--page")
    expected = options(args, "--expect")
    status_wanted = option(args, "--expect-status", None)
    if len(args) != 1:
        raise Failed("ticket read takes one ticket ID")
    person = signed_in()
    if as_page:
        reply = person.get(f"/tickets/{args[0]}")
        status, printed = reply.status, reply.body
    else:
        status, said = json_get(person, f"/tickets/{args[0]}")
        printed = json.dumps(said, indent=1, ensure_ascii=False) if said is not None else ""
    if status_wanted is not None:
        if status != int(status_wanted):
            raise Failed(f"/tickets/{args[0]} answered {status}, not {status_wanted}")
        print(status)
        return
    if status != 200:
        raise Failed(f"/tickets/{args[0]}: {status}")
    print(printed)
    for text in expected:
        if text not in printed:
            raise Failed(f"the ticket does not say {text!r}")


def ticket_note(args):
    if len(args) != 2:
        raise Failed("ticket note takes a ticket ID and the note")
    person = signed_in()
    shown = person.get(f"/tickets/{args[0]}")
    if shown.status != 200:
        raise Failed(f"/tickets/{args[0]}: {shown.status} {sentence(shown)}")
    noted = person.post(f"/tickets/{args[0]}/notes",
                        {"form_token": form_token(shown), "kind": "note", "note": args[1]})
    if noted.status != 303:
        raise Failed(f"the note was not added: {noted.status} {sentence(noted)}")
    print(f"ticket note: noted {args[0]}")


def ticket_work(args):
    status_wanted = option(args, "--expect-status", None)
    fields = dict(pairs(args))
    if len(args) != 1 or "act" not in fields:
        raise Failed("ticket work takes a ticket ID and act=ACT")
    if fields.get("owner") and "|" not in fields["owner"]:
        fields["owner"] = f"local|{fields['owner']}"
    person = signed_in()
    shown = person.get(f"/tickets/{args[0]}")
    if shown.status != 200 and status_wanted is not None and shown.status == int(status_wanted):
        # A ticket the person may not see is not found, its page included.
        print(f"ticket work: {shown.status}")
        return
    if shown.status != 200:
        raise Failed(f"/tickets/{args[0]}: {shown.status} {sentence(shown)}")
    notes = len(re.findall(r'<li data-number="\d+"', shown.body))
    done = person.post(f"/tickets/{args[0]}/work",
                       {"form_token": form_token(shown), "against_notes": str(notes), **fields})
    if status_wanted is not None:
        if done.status != int(status_wanted):
            raise Failed(f"the act answered {done.status}, not {status_wanted}: {sentence(done)}")
        print(f"ticket work: {done.status}")
        return
    if done.status != 303:
        raise Failed(f"the act was not taken: {done.status} {sentence(done)}")
    said = urllib.parse.parse_qs(urllib.parse.urlsplit(done.location or "").query).get("done", [""])[0]
    print(f"ticket work: {said}")


def ticket(args):
    verbs = {"file": ticket_file, "list": ticket_list, "read": ticket_read,
             "note": ticket_note, "work": ticket_work}
    if not args or args[0] not in verbs:
        raise Failed(f"ticket takes {', '.join(verbs)}")
    verbs[args.pop(0)](args)


def inbox(args):
    wanted = option(args, "--expect", None)
    kinds = options(args, "--expect-kind")
    if args:
        raise Failed(f"inbox takes no {args[0]!r}")
    person = signed_in()
    status, said = json_get(person, "/inbox")
    if status != 200 or said is None:
        raise Failed(f"/inbox: {status}")
    for notice in said["notices"]:
        print(f"{notice['ticket_id']}\t{notice['kind']}")
    if wanted is not None and len(said["notices"]) != int(wanted):
        raise Failed(f"the inbox holds {len(said['notices'])} new, not {wanted}")
    missing = [k for k in kinds if k not in [n["kind"] for n in said["notices"]]]
    if missing:
        raise Failed(f"no notice of kind {', '.join(missing)}")


def mcp(args):
    verbs = {"connect": mcp_connect, "list": mcp_list, "call": mcp_call,
             "complete": mcp_complete, "calls": mcp_calls}
    if not args or args[0] not in verbs:
        raise Failed(f"mcp takes {', '.join(verbs)}")
    verbs[args.pop(0)](args)


COMMANDS = {"ready": ready, "settings": settings, "account": account, "page": page,
            "form": form, "unlinked": unlinked, "grant": grant, "compose": compose,
            "instruments": instruments, "instrument": instrument, "mcp": mcp,
            "ticket": ticket, "inbox": inbox}


def main(argv):
    if not argv or argv[0] not in COMMANDS:
        print(__doc__, file=sys.stderr)
        return 2
    command, args = argv[0], list(argv[1:])
    global AS, MCP_STATE
    try:
        acting_on(option(args, "--instance", INSTANCE))
        AS = option(args, "--as", None)
        if AS is not None and not re.fullmatch(r"[a-z][a-z0-9_-]{0,30}", AS):
            raise Failed(f"--as names one of the run's people, not {AS!r}")
        client = "harness agent"
        if command == "mcp" and "--client" in args:
            at = args.index("--client")
            client = args[at + 1] if at + 1 < len(args) else client
            if args[:1] != ["connect"]:
                option(args, "--client", None)
        MCP_STATE = mcp_state_for(client)
        COMMANDS[command](args)
    except Failed as failed:
        print(f"harness {command} FAILED: {failed}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
