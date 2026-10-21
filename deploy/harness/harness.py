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
      run's volumes are, at the path MERIDIAN_STORAGE_DIR names. One listed
      with `"archive": true` -- an edge plugin a deployment admin allows an
      archive, as `archive allow` then records -- is given the harness's
      archive beside it (contract v16): a volume of its own at the path
      MERIDIAN_ARCHIVE_DIR names, as the launcher gives an allowed instance
      its directory of a local archive. The first is the runner's plugin.
      One listed with `"declaration": PATH` -- the SDK's JSON as
      `meridian-declaration <module:attr>` prints it from the plugin's image,
      what an upload carries -- has its registration held to it, as a
      launched plugin's is to its version's (W4.1, contract v25): its sidecar
      refuses a registration differing from it, naming the first difference.
      PATH is a file as compose reads a bind mount's source, relative to the
      directory of the first compose file. A plugin declaring v25 without
      one registers unchecked, as on any sidecar no launch made.

  ready [--seconds N]
      Until the plugin has registered with its sidecar and the dashboard lists
      it (healthy or not): the first command of a run.

  settings NAME=VALUE ... [--seconds N] [--expect TEXT]
      Sets the plugin's settings in its settings form, as its admin does
      (W6.11), once the plugin has declared each. A secret goes in its secret
      field; a developer's setting is accepted because the harness's dashboard
      is a development one. A table setting's cell is NAME[ROW].COLUMN=VALUE,
      the rows given its whole (contract v14). --expect reads the Settings
      tab after and fails unless it says TEXT. --expect-refused TEXT fails
      unless the dashboard refuses the settings saying TEXT -- a window below
      the hold, naming the setting (contract v16).

  hold DAYS [--role ROLE] [--write-once] [--expect-refused TEXT] [--prepare WHY]
      Sets the hold on raw records on the deployment's Settings, as a
      deployment admin does (W6.25, contract v16): for ROLE, an edge role, or
      every one without it; 0 days clears it. --expect-refused TEXT fails
      unless it is refused saying TEXT. With --prepare, the dialog's Prepare
      (contract v20): prepared, nothing changed, its PC- ID printed.

  archive allow [--bound-gib N] | archive withdraw   [--prepare WHY]
      Allows the plugin an archive, with a bound in GiB or none, or withdraws
      it, on its Manage page's Summary, as a deployment admin does (W8.7,
      contract v16). The harness has no launcher: the plugin listed with
      `"archive": true` already holds its archive's volume, and this records
      what a deployment admin allowed.

  summary [--until TEXT] [--seconds N]
      Prints the plugin's Summary under Manage, where its raw records are
      drawn (W6.9): with --until, again until it says TEXT.

  view [--tab TAB] [--until TEXT] [--seconds N]
      Prints a tab of the plugin's view in the admin portal, as the admin
      reads it (contract v17): settings, a table's setting-NAME, access, or
      the Overview without --tab; with --until, again until it says TEXT.

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

  grant --level read|write|admin [--role ROLE]
      Grants the harness's admin that level on the plugin, on All accounts,
      as a deployment admin does in the dashboard (W6.5 to W6.8): a user group
      holding the admin, an access group giving the plugin the level, and the
      permission joining them. From contract v15 a grant is on one role of the
      plugin (decisions/033): --role names it, as the Access editor's row
      does; without it, the plugin's one role where it holds exactly one, or
      the plugin as a whole where it holds none. A plugin that writes for a person -- an
      operations plugin confirming an opening balance (W9.1) -- is opened at
      write by someone granted write; the admin is granted nothing on a
      plugin until this is run.

  instruments [--expect N] [--seconds N]
      Prints the instrument records the book cannot use yet -- no asset class
      or no currency -- one per line, its ID, a tab, and its identifiers as the
      dashboard's Instruments page lists them (W3.11, contract v10). With
      --expect, waits until it lists N.

  instrument (ID | --identifier TEXT) asset_class=CLASS currency=CODE
             [description=TEXT] source=TEXT [note=TEXT] [--ask-platform] [--seconds N]
      Completes one record on its page, as the deployment admin does (W3.10):
      the record named by its ID, or the first listed whose identifiers say
      TEXT (`symbol: AAPL`), waiting for it to be listed. CLASS is one of
      equity, debt, fund, derivative, crypto_asset, event_contract, cash; each
      value goes with the source given, and the dashboard stamps the admin.
      With --ask-platform, first asks the platform about it from its page
      (W3.3), as the admin's button does, so what the platform answers -- a
      listing venue, contract v18 -- is kept on it.

  mcp connect [--covers everything | --covers deployment_admin] [--covers INSTANCE[:ROLE]:LEVEL ...]
              [--client NAME] [--check-consent] [--consent-says TEXT ...]
      Connects an MCP client as the admin, as an agent's client does (W6.17,
      W6.20): registers it (named "harness agent" unless --client says), sends
      her through the dashboard's authorisation for the `/mcp` resource with
      a PKCE challenge, signs her in afresh, consents to only what --covers
      names -- the deployment admin's capabilities, and each plugin and level,
      from contract v15 each plugin, role and level (a row per role, W6.17)
      -- on All accounts, for 30 days; or with `--covers everything`, all she
      holds now and is granted later (W6.17's "Everything"); and exchanges the code for a token
      pair, kept for the run in the runner's state. Prints the delegation and
      how many tools it reaches. With --check-consent, fails unless the tools
      the consent page listed under what she allowed are exactly the tools
      tools/list then lists (contract v17). With --consent-says, fails unless
      the consent page's HTML says each TEXT (contract v20: the admins' level
      beside each act).

  core-page PATH [--expect TEXT ...]
      One of core's own pages as the person signed in, its status and body
      printed; fails unless it says each TEXT.

  core-form PAGE POST FIELD=VALUE... [--expect-refused TEXT]
      One of core's own forms as the deployment admin: PAGE read for its
      form token, POST sent with the fields, a FIELD given more than once
      sent once for each; fails unless done, or refused saying TEXT. A
      page that goes back saying what became of it (`?done=`, `?failed=`)
      is read there, and what it said printed.

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

  licence DATASET kept=true|false retention_days=N derived_use=true|false
          display=true|false personal_use=true|false [default_fields=A,B] [note=TEXT]
          [--expect-refused TEXT] [--prepare]
      Sets a dataset's licence on the Data sources page, as a deployment
      admin does (W10.1, contract v18), replacing it whole; a boolean left out
      is false. --expect-refused TEXT fails unless it is refused saying TEXT.
      With --prepare, the dialog's Prepare: its PC- ID printed.

  entitle DATASET [--fields A,B] [--withdraw] [note=TEXT] [--seconds N]
      Entitles the plugin to a dataset on the Data sources page, every field
      or those named, or withdraws it (W10.1, contract v18); and writes the
      broker's configuration again with each plugin's datasets, its subjects
      on the broker, waiting until the broker has reloaded with them: before
      the entitlement is recorded, so the plugin's sidecar subscribes to a
      subject the broker admits, and after a withdrawal.

  priority DATA_TYPE [--kind KIND] DATASET ... [note=TEXT] [--against N]
           [--expect-refused TEXT]
      Sets the deployment's priority for meridian.v1.Price, one kind of it --
      close, last, nav or settlement -- or meridian.v1.Bar, on the Data
      sources page (W10.2, contract v18): the datasets first to last, replaced
      whole, against the priority as the page shows it, or --against the
      updated_at_ns given, which a priority changed since is refused.

  service-account create NAME --owner PERSON [--setting prepare_only|act] [--purpose TEXT]
  service-account set SA [--setting prepare_only|act] [--name NAME] [--owner PERSON]
  service-account disable|enable|remove SA [--expect-refused TEXT]
  service-account grant SA --role ROLE --level read|write
  service-account runs-as (SA | none) --role ROLE [--level write|read] [--expect-refused TEXT]
  service-account list
      A service account (contract v21, W6.32 to W6.34), at the deployment's
      Settings, its Service accounts tab, as a deployment admin does: created
      with a name, an owner -- one of the run's people, or `harness` -- and a
      setting, prepare only unless --setting says act, its SA- ID printed;
      its setting, name or owner changed; disabled (by its owner too, with
      --as), re-enabled or removed. `grant` grants it a level on one role of
      the plugin, on All accounts, by its ID in a user group's logins, as
      anyone is granted. `runs-as` chooses, on the plugin's Access tab, which
      account the role's autonomous work runs as, or none. `list` prints each
      account's row, one per line: its ID, name, setting and state. With
      --expect-refused, fails unless the change is refused saying TEXT.

  connectivity available [--withdraw] [--note TEXT]
  connectivity link --ems EMS [--user person:SUBJECT ...] [--note TEXT]
      From contract v22 (W11.21): makes the connectivity plugin available,
      or withdraws it, as a deployment admin does; or links it to an ems and
      chooses who may route through it -- people by subject, service
      accounts (`account:SA-...`) and user groups (`group:...`) -- as the
      ems's admin does, on its view's Overview.

  instruction VALUE (--all | --account ID | --plugin ID | --person SUBJECT |
                     --service-account SA) [--within PLUGIN] note=TEXT
                     [--expect-refused TEXT]
      From contract v22 (W11.15): sets active, pause or stop at the Trading
      instructions page, as the person acting does: a deployment admin on
      any target, a plugin's admin within their plugin (--within, or the
      plugin itself). Prints what the page then lists for it.

  attribute ACCOUNT waits=true|false note=TEXT [--expect-refused TEXT]
      From contract v22 (W9.13): whether the account's releases wait for
      compliance, at the Accounts tab's book part, as a deployment admin
      sets it.

  model-routing MODEL_ID IMPLEMENTATION note=TEXT [--expect-refused TEXT]
      From contract v26 (W12.6): routes a model id to a `pricing` plugin's
      instance, or back to `built-in`, at the Settings' Model routing tab, as
      a deployment admin does, so a run can stop the plugin and read its
      figures not evaluated. An agent's own call to
      dashboard__set_model_routing is at "may not" by default; `agents
      dashboard__set_model_routing --level may_make` loosens it first.

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
    refusal = option(args, "--expect-refused", None)
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
    # Against the settings as the form was drawn (contract v17's guard, which
    # from v18 refuses a change naming none once any are saved).
    drawn = re.search(r'name="against_updated_at_ns" value="(-?\d+)"', form.body)
    if drawn:
        fields["against_updated_at_ns"] = drawn.group(1)
    for name, value in asked:
        if "[" in name:
            fields[f"table.{base(name)}"] = "1"
            fields[f"table.{name}"] = value
            continue
        secret = f'name="secret.{name}"' in form.body
        fields[("secret." if secret else "value.") + name] = value
    saved = admin.post(f"{VIEW}/settings", fields)
    if refusal is not None:
        if saved.status == 303:
            raise Failed(f"the settings were saved, and were to be refused saying {refusal!r}")
        if refusal not in html.unescape(sentence(saved)):
            raise Failed(f"the settings were refused, not saying {refusal!r}: {sentence(saved)}")
        print(f"settings: refused: {html.unescape(sentence(saved))}")
        return
    if saved.status != 303:
        raise Failed(f"the settings were not saved: {saved.status} {sentence(saved)}")
    print(f"settings: saved {', '.join(sorted({base(name) for name, _ in asked}))}")
    if wanted is not None:
        shown = admin.get(f"{VIEW}?tab=settings")
        if wanted not in shown.body:
            raise Failed(f"the settings page does not say {wanted!r}")
        said = re.search(r"data-last-changed>([^<]*)<", shown.body)
        print(f"settings: {said.group(1) if said else wanted}")


def prepared_by_dialog(done, what):
    """A dialog's Prepare (contract v20): the post answered with the prepared
    change's page; its ID printed, nothing changed."""
    where = done.location or ""
    if done.status != 303 or "/prepared/PC-" not in where:
        raise Failed(f"{what} was not prepared: {done.status} {where} {sentence(done)}")
    ident = where.rsplit("/", 1)[-1]
    print(ident)
    return ident


def hold(args):
    role = option(args, "--role", "")
    prepare = option(args, "--prepare", None)
    write_once = "--write-once" in args
    if write_once:
        args.remove("--write-once")
    refusal = option(args, "--expect-refused", None)
    if len(args) != 1 or not args[0].isdigit():
        raise Failed("hold takes a number of days, 0 to clear")
    admin = signed_in()
    page = admin.get("/admin")
    fields = {"form_token": form_token(page), "role": role, "days": args[0]}
    if write_once:
        fields["write_once"] = "1"
    if prepare is not None:
        fields.update({"prepare": "1", "note": prepare})
    done = admin.post("/admin/holds", fields)
    covers = role or "every edge role"
    if prepare is not None and refusal is None:
        prepared_by_dialog(done, "the hold")
        return
    if refusal is not None:
        if done.status == 303:
            raise Failed(f"the hold was set, and was to be refused saying {refusal!r}")
        if refusal not in html.unescape(sentence(done)):
            raise Failed(f"the hold was refused, not saying {refusal!r}: {sentence(done)}")
        print(f"hold: refused: {html.unescape(sentence(done))}")
        return
    if done.status != 303:
        raise Failed(f"the hold was not set: {done.status} {sentence(done)}")
    print(f"hold: {args[0]} days over {covers}")


def archive(args):
    if not args or args[0] not in ("allow", "withdraw"):
        raise Failed("archive takes allow or withdraw")
    verb = args.pop(0)
    bound = option(args, "--bound-gib", "")
    prepare = option(args, "--prepare", None)
    if args:
        raise Failed(f"archive {verb} takes no {args[0]!r}")
    admin = signed_in()
    summary_at = f"/plugins/{INSTANCE}?level=admin&tab=summary"
    page = admin.get(summary_at)
    if page.status != 200:
        raise Failed(f"its Summary: {page.status} {sentence(page)}")
    fields = {"form_token": form_token(page)}
    path = f"/plugins/{INSTANCE}/archive"
    if verb == "allow":
        fields["most_gib"] = bound
    else:
        path += "/withdraw"
    if prepare is not None:
        fields.update({"prepare": "1", "note": prepare})
        prepared_by_dialog(admin.post(path, fields), f"archive {verb}")
        return
    done = admin.post(path, fields)
    if done.status != 303:
        raise Failed(f"the archive was not {verb}ed: {done.status} {sentence(done)}")
    shown = admin.get(summary_at)
    said = re.search(r'class="archive-state"[^>]*>([^<]*)<', shown.body)
    print(f"archive {verb}: {html.unescape(said.group(1)) if said else 'done'}")


def summary(args):
    seconds = number(args, "--seconds", 60)
    wanted = option(args, "--until", None)
    admin = signed_in()

    def answered():
        page = admin.get(f"/plugins/{INSTANCE}?level=admin&tab=summary")
        if page.status != 200:
            raise Failed(f"its Summary: {page.status} {sentence(page)}")
        if wanted is not None and wanted not in html.unescape(page.body):
            raise Failed(f"its Summary does not say {wanted!r} yet")
        return page

    page = until(seconds, answered, f"{INSTANCE}'s Summary")
    print(page.body)


def view(args):
    seconds = number(args, "--seconds", 60)
    tab = option(args, "--tab", None)
    wanted = option(args, "--until", None)
    if args:
        raise Failed(f"view takes no {args[0]!r}")
    admin = signed_in()
    path = VIEW + (f"?tab={urllib.parse.quote(tab)}" if tab else "")

    def answered():
        page = admin.get(path)
        if page.status != 200:
            raise Failed(f"its view: {page.status} {sentence(page)}")
        if wanted is not None and wanted not in html.unescape(page.body):
            raise Failed(f"its view does not say {wanted!r} yet")
        return page

    print(until(seconds, answered, f"{INSTANCE}'s view").body)


def account(args):
    seconds = number(args, "--seconds", 60)
    # Granted the plugin the command acts on, which it may then link to
    # (W6.30, contract v20), unless --no-grant: nothing is by default.
    granting = "--no-grant" not in args
    if not granting:
        args.remove("--no-grant")
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

    found = until(seconds, listed, f"{name} was never listed")
    if granting:
        accounts = admin.get(f"{VIEW}?tab=accounts")
        granted = admin.post(f"{VIEW}/accounts", {"form_token": form_token(accounts),
                                                  "grant": found, "withdraw": "",
                                                  "note": "granted by the harness"})
        if granted.status != 303:
            raise Failed(f"{name} was not granted {INSTANCE}: {granted.status} {sentence(granted)}")
    print(found)


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


def core_page(args):
    """`core-page PATH [--expect TEXT]...`: one of core's own pages, as the
    person signed in; its status and body printed, failing unless it says
    each TEXT."""
    expected = options(args, "--expect")
    if len(args) != 1:
        raise Failed("core-page takes one path")
    person = signed_in()
    reply = person.get(args[0])
    for wanted in expected:
        if wanted not in reply.body:
            raise Failed(f"{args[0]} answered {reply.status} without {wanted!r}")
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
    role = option(args, "--role", None)
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
        # Granted again with its accounts changed: the group is edited and
        # the permission stands as it was.
        if path == "/admin/permissions" and "already granted" in sentence(done):
            return
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
    # A row of the Access editor per role (contract v15), or as the form
    # was before, the plugin ticked at its level.
    access_group = defined("/admin/access-groups", "access_group_id",
                           f"Harness {level} on {INSTANCE}" + (f" {role}" if role else ""),
                           [(f"level.{INSTANCE}:{role}", level)] if role is not None
                           else [("plugin", INSTANCE), (f"level.{INSTANCE}", level)])
    # An admin reaches no account's data, so its permission names none.
    account_group = "" if level == "admin" else ALL_ACCOUNTS
    if accounts and level != "admin":
        account_group = defined("/admin/account-groups", "account_group_id",
                                f"Harness accounts of {holder}",
                                [("account_ids", account) for account in accounts])
    post("/admin/permissions", [("user_group_id", user_group),
                                ("account_group_id", account_group),
                                ("access_group_id", access_group)])
    print(f"grant: {holder} holds {level} on {INSTANCE}" + (f" {role}" if role else "") + ", on "
          + ("no account" if level == "admin" else ", ".join(accounts) or "All accounts"))


def instrument_rows(admin):
    """The Instruments page's records: each ID, its identifiers as listed, and
    whether the book can use it."""
    page = admin.get("/admin/instruments")
    if page.status != 200:
        raise Failed(f"/admin/instruments: {page.status} {sentence(page)}")
    table = re.search(r'<table class="list[^"]*" id="instruments-table">(.*?)</table>', page.body, re.S)
    rows = []
    for row in re.findall(r"<tr[^>]*>(.*?)</tr>", table.group(1) if table else "", re.S):
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
    ask_platform = "--ask-platform" in args
    if ask_platform:
        args.remove("--ask-platform")
    asked = dict(pairs(args))
    named = args.pop(0) if args else None
    if args or (named is None) == (by_identifier is None):
        raise Failed("instrument takes an ID or --identifier TEXT, and NAME=VALUE")
    source = asked.get("source", "")
    # `--ask-platform` alone asks and completes nothing (contract v24: a
    # provisional record the platform holds is resolved by the ask, W3.8).
    ask_only = ask_platform and not asked
    if not ask_only and (asked.get("asset_class") not in CLASSES or not asked.get("currency")
                         or not source):
        raise Failed(f"instrument takes asset_class={'|'.join(CLASSES)}, currency=CODE and source=TEXT")
    admin = signed_in()

    def found():
        for instrument_id, identifiers, _ in instrument_rows(admin):
            if instrument_id == named or (by_identifier and by_identifier in identifiers):
                return instrument_id
        # A complete record is not listed to complete (contract v24: the
        # platform's record, kept complete): its own page answers it.
        if named and admin.get(f"/admin/instruments/{urllib.parse.quote(named)}").status == 200:
            return named
        raise Failed(f"no record listed is {named or by_identifier!r}")

    instrument_id = until(seconds, found, "the record was never listed")
    if ask_platform:
        shown = admin.get(f"/admin/instruments/{urllib.parse.quote(instrument_id)}")
        if shown.status != 200:
            raise Failed(f"{instrument_id}'s page: {shown.status} {sentence(shown)}")
        answered = admin.post(f"/admin/instruments/{urllib.parse.quote(instrument_id)}/ask",
                              {"form_token": form_token(shown)})
        if answered.status != 200:
            raise Failed(f"{instrument_id}: the platform was not asked: "
                         f"{answered.status} {sentence(answered)}")
        if ask_only:
            print(f"instrument: the platform asked about {instrument_id}")
            return
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
         "lake", "instrument", "conductor", "dashboard", "runner", "store", "storage",
         "runtime", "first-run", "dashboard-1", "analytics"}
RUNTIME_IMAGE = "${MERIDIAN_RUNTIME_IMAGE:?set MERIDIAN_RUNTIME_IMAGE to the runtime image of the harness's commit}"
# The roles whose plugins own storage for their raw external records
# (decisions/028), as the chart's `meridian-runtime.edgeRoles` says them; a
# plugin holding any is given a volume of its own, where the chart gives it a
# claim of its own, at the same path.
EDGE_ROLES = ("confirmation", "connectivity", "custody", "data", "reporting", "servicing", "settlement")
STORAGE_DIR = "/var/lib/meridian/storage"
# Where an edge plugin allowed an archive finds it (contract v16), as the
# chart's archive shape mounts an allowed instance's directory.
ARCHIVE_DIR = "/var/lib/meridian/archive"


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
        if not isinstance(plugin, dict) or not {"instance", "image", "roles"} <= set(plugin) \
                or set(plugin) - {"instance", "image", "roles", "archive", "declaration"}:
            raise Failed(f"{where} is an object of instance, image and roles, archive where it "
                         "is allowed one and declaration where it is held to one, and nothing else")
        if "archive" in plugin and plugin["archive"] is not True:
            raise Failed(f"{where}'s archive is true where it is allowed one, and absent otherwise")
        if "declaration" in plugin and (not isinstance(plugin["declaration"], str)
                                        or not plugin["declaration"]
                                        or re.search(r"[\s$:]", plugin["declaration"])):
            raise Failed(f"{where}'s declaration is the path of the SDK's declaration JSON, "
                         "with no space, colon or $")
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
        if plugin.get("archive") and not any(role in EDGE_ROLES for role in roles):
            raise Failed(f"{where} is allowed an archive and holds no edge role; only a plugin at the "
                         "edge keeps raw records (decisions/028)")
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
    archived = [plugin["instance"] for plugin in listed if plugin.get("archive")]
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
            "volumes": [f"storage-{name}:/storage/{name}" for name in stored]
                       + [f"archive-{name}:/storage/archive-{name}" for name in archived],
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
        # Its registration held to the declaration an upload carries (W4.1,
        # contract v25), where it is given one.
        if plugin.get("declaration"):
            # A bind mount: compose reads a bare name as a named volume.
            source = plugin["declaration"]
            if not source.startswith(("/", "./", "../")):
                source = "./" + source
            services[f"sidecar-{name}"]["volumes"].append(
                f"{source}:/declaration/declaration.json:ro")
            services[f"sidecar-{name}"]["environment"]["MERIDIAN_PLUGIN_DECLARATION"] = \
                "/declaration/declaration.json"
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
        # Its archive, where it is allowed one (contract v16): a volume of
        # its own beside its storage, which `down -v` removes.
        if name in archived:
            services[name]["environment"]["MERIDIAN_ARCHIVE_DIR"] = ARCHIVE_DIR
            services[name]["volumes"].append(f"archive-{name}:{ARCHIVE_DIR}")
    written = {
        "x-harness": "written by harness.py compose for " + ", ".join(names)
                     + "; written again, never edited, for another list",
        "services": services,
    }
    if stored:
        written["volumes"] = {f"storage-{name}": {} for name in stored}
        written["volumes"].update({f"archive-{name}": {} for name in archived})
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


LEVELS_DOWN = ["admin", "write", "read"]


def consent_rows(page):
    """Each row's tools as the consent page lists them (contract v17): its
    `data-reach` (deployment_admin, or INSTANCE:ROLE:LEVEL as the form posts
    it) to the names in its `data-tools`, which are what tools/list answers
    through a delegation covering that row alone."""
    return {html.unescape(reach): set(html.unescape(names).split())
            for reach, names in re.findall(r'<p data-reach="([^"]*)" data-tools="([^"]*)"', page)}


def consent_said(rows, covers):
    """What the consent page says a delegation covering `covers` grants: the
    union of its rows' tools, a level picked including those held below it
    (as the page's select says, "Manage, with Open and View")."""
    if covers == ["everything"]:
        return set().union(*rows.values()) if rows else set()
    said = set()
    for covered in covers:
        if covered == "deployment_admin":
            said |= rows.get("deployment_admin", set())
            continue
        parts = covered.split(":")
        instance, role, level = (parts[0], "", parts[1]) if len(parts) == 2 else parts
        for below in LEVELS_DOWN[LEVELS_DOWN.index(level):]:
            said |= rows.get(f"{instance}:{role}:{below}", set())
    return said


def mcp_connect(args):
    covers = options(args, "--covers")
    client_name = option(args, "--client", "harness agent")
    check_consent = "--check-consent" in args
    if check_consent:
        args.remove("--check-consent")
    consent_says = options(args, "--consent-says")
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
    for wanted in consent_says:
        if wanted not in consent.body:
            raise Failed(f"the consent page does not say {wanted!r}")
    everything = "everything" in covers
    if everything and len(covers) > 1:
        raise Failed("--covers everything is the whole of what she holds, alone")
    fields = [("request", request.group(1)), ("confirm", confirm.group(1)), ("decision", "allow"),
              ("covers", "everything" if everything else "some"), ("days", "30")]
    # Every account she may tick: All accounts once she holds a grant on it.
    for offered in re.findall(r'name="account_group" value="([^"]+)"', consent.body):
        fields.append(("account_group", html.unescape(offered)))
    for covered in [] if everything else covers:
        if covered == "deployment_admin":
            fields.append(("deployment_admin", "1"))
        elif re.fullmatch(r"[a-z0-9-]+(:[a-z]*)?:(admin|write|read)", covered):
            fields.append(("level", covered))
        else:
            raise Failed(f"--covers is deployment_admin or INSTANCE[:ROLE]:LEVEL, not {covered!r}")
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
    if check_consent:
        said = consent_said(consent_rows(consent.body), covers)
        listed = {tool["name"] for tool in tools}
        if said != listed:
            raise Failed(f"the consent page said {sorted(said - listed)} that tools/list does not "
                         f"list, and tools/list lists {sorted(listed - said)} the page did not say")
        print(f"mcp: the consent page listed the {len(said)} tools tools/list lists")
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
    describe = option(args, "--describe", None)
    if args:
        raise Failed(f"mcp list takes no {args[0]!r}")
    listed = rpc("tools/list")["tools"]
    if describe is not None:
        found = [tool for tool in listed if tool["name"] == describe]
        if not found:
            raise Failed(f"the delegation is not listed {describe}")
        print(found[0].get("description", ""))
        return
    names = [tool["name"] for tool in listed]
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


# ── The Data sources page (W10.1, W10.2, contract v18) ──────────────────────

DATA_SOURCES = "/admin/data-sources"
BROKER = os.environ.get("MERIDIAN_HARNESS_BROKER", "/broker")


def truth(args, name):
    """A NAME=true|false pair taken out of `args`, false when left out."""
    for at, arg in enumerate(args):
        if arg.startswith(name + "="):
            del args[at]
            said = arg.split("=", 1)[1]
            if said not in ("true", "false"):
                raise Failed(f"{name} is true or false, not {said!r}")
            return said == "true"
    return False


def said_pair(args, name, default=""):
    for at, arg in enumerate(args):
        if arg.startswith(name + "="):
            del args[at]
            return arg.split("=", 1)[1]
    return default


def refused_or_done(done, refusal, what):
    """A change posted: done, or refused saying `refusal` when one is
    expected. A page that answers by going back with what became of it
    (contract v25's Plugins page: `?failed=` or `?done=`) is read there."""
    said = urllib.parse.parse_qs(urllib.parse.urlsplit(done.location or "").query)
    if done.status == 303 and "failed" in said:
        failed = said["failed"][0]
        if refusal is None:
            raise Failed(f"{what} was refused: {failed}")
        if refusal not in failed:
            raise Failed(f"{what} was refused, not saying {refusal!r}: {failed}")
        return f"refused: {failed}"
    if done.status == 303 and refusal is None and "done" in said:
        return said["done"][0]
    if refusal is not None:
        if done.status == 303:
            raise Failed(f"{what} was done, and was to be refused saying {refusal!r}")
        if refusal not in html.unescape(sentence(done)):
            raise Failed(f"{what} was refused, not saying {refusal!r}: {sentence(done)}")
        return f"refused: {html.unescape(sentence(done))}"
    if done.status != 303:
        raise Failed(f"{what} was not done: {done.status} {sentence(done)}")
    return None


def licence(args):
    refusal = option(args, "--expect-refused", None)
    prepare = "--prepare" in args
    if prepare:
        args.remove("--prepare")
    fields = {"retention_days": said_pair(args, "retention_days", "0"),
              "default_fields": said_pair(args, "default_fields"),
              "note": said_pair(args, "note")}
    for name in ("kept", "derived_use", "display", "personal_use"):
        if truth(args, name):
            fields[name] = "1"
    if len(args) != 1:
        raise Failed("licence takes a dataset, then its terms as NAME=VALUE")
    fields["dataset"] = args[0]
    admin = signed_in()
    fields["form_token"] = form_token(admin.get(DATA_SOURCES))
    if prepare:
        fields["prepare"] = "1"
        prepared_by_dialog(admin.post(DATA_SOURCES + "/licence", fields), f"{args[0]}'s licence")
        return
    said = refused_or_done(admin.post(DATA_SOURCES + "/licence", fields), refusal,
                           f"{args[0]}'s licence")
    print(f"licence: {said}" if said else
          f"licence: {args[0]} {'kept' if 'kept' in fields else 'served, not kept'}"
          f", {fields['retention_days']} days")


def entitlements(admin):
    """Every dataset each plugin is entitled to, as the Data sources page
    lists them."""
    page = admin.get(DATA_SOURCES)
    if page.status != 200:
        raise Failed(f"the Data sources page did not open: {page.status} {sentence(page)}")
    table = re.search(r'<table class="list one-line entitlements"[^>]*>(.*?)</table>', page.body, re.S)
    held = {}
    for dataset, instance in re.findall(r'<tr data-id="([^" ]+) ([^"]+)">',
                                        table.group(1) if table else ""):
        held.setdefault(html.unescape(instance), []).append(html.unescape(dataset))
    return held


def broker_reloaded(held, seconds):
    """The broker's configuration written again with `held`, each plugin's
    datasets, and the broker reloaded with it."""
    token = secrets.token_hex(8)
    with open(os.path.join(BROKER, "datasets.json.next"), "w", encoding="utf-8") as kept:
        json.dump({instance: sorted(set(datasets)) for instance, datasets in held.items()}, kept)
    os.replace(os.path.join(BROKER, "datasets.json.next"), os.path.join(BROKER, "datasets.json"))
    with open(os.path.join(BROKER, "asked"), "w", encoding="utf-8") as kept:
        kept.write(token)

    def reloaded():
        try:
            with open(os.path.join(BROKER, "reloaded"), encoding="utf-8") as said:
                if said.read().strip() == token:
                    return True
        except OSError:
            pass
        raise Failed("the broker has not reloaded with the plugins' datasets")
    until(seconds, reloaded, "the broker did not reload")


def entitle(args):
    seconds = number(args, "--seconds", 60)
    named = option(args, "--fields", "")
    withdraw = "--withdraw" in args
    if withdraw:
        args.remove("--withdraw")
    note = said_pair(args, "note")
    if len(args) != 1:
        raise Failed("entitle takes a dataset")
    dataset = args[0]
    admin = signed_in()
    held = entitlements(admin)
    if not withdraw:
        held.setdefault(INSTANCE, []).append(dataset)
        broker_reloaded(held, seconds)
    fields = {"form_token": form_token(admin.get(DATA_SOURCES)), "dataset": dataset,
              "instance": INSTANCE, "fields": named, "note": note}
    if not withdraw:
        fields["allowed"] = "1"
    refused_or_done(admin.post(DATA_SOURCES + "/entitlement", fields), None,
                    f"{INSTANCE}'s entitlement to {dataset}")
    if withdraw:
        held[INSTANCE] = [d for d in held.get(INSTANCE, []) if d != dataset]
        broker_reloaded(held, seconds)
    print(f"entitle: {INSTANCE} {'withdrawn from' if withdraw else 'entitled to'} {dataset}"
          + (f" ({named})" if named and not withdraw else ""))


def priority(args):
    refusal = option(args, "--expect-refused", None)
    kind = option(args, "--kind", "")
    against = option(args, "--against", None)
    note = said_pair(args, "note")
    if len(args) < 2:
        raise Failed("priority takes a data type, then its datasets first to last")
    data_type, datasets = args[0], args[1:]
    admin = signed_in()
    page = admin.get(DATA_SOURCES)
    if against is None:
        against = "0"
        for fill in re.findall(r'data-dialog-open="priority-dialog" data-title="Change the priority" '
                               r'data-fill="([^"]+)"', page.body):
            said = json.loads(html.unescape(fill))["fields"]
            if said["data_type"] == data_type and said["kind"] == kind:
                against = said["against_updated_at_ns"]
                break
    fields = {"form_token": form_token(page), "data_type": data_type, "kind": kind,
              "datasets": "\n".join(datasets), "against_updated_at_ns": against, "note": note}
    said = refused_or_done(admin.post(DATA_SOURCES + "/priority", fields), refusal,
                           f"the priority for {data_type} {kind}".strip())
    print(f"priority: {said}" if said else
          f"priority: {data_type}{' ' + kind if kind else ''}: {' then '.join(datasets)}")


# ── Contract v20: prepared changes, the admins' levels, account grants ──────


def form_fields(body, action_has):
    """Every field a page's form posts, the one whose action names
    `action_has`: inputs by value (a checkbox only when checked), a select by
    its selected option, a textarea by its text."""
    forms = re.findall(r'<form[^>]*action="([^"]*)"[^>]*>(.*?)</form>', body, re.S)
    chosen = [inner for action, inner in forms if action_has in html.unescape(action)]
    if not chosen:
        raise Failed(f"no form posts to {action_has}")
    inner = chosen[0]
    fields = []
    for tag in re.findall(r"<input[^>]*>", inner):
        name = re.search(r'name="([^"]+)"', tag)
        if not name:
            continue
        kind = re.search(r'type="([^"]+)"', tag)
        kind = kind.group(1) if kind else "text"
        if kind in ("checkbox", "radio") and "checked" not in tag:
            continue
        if kind in ("submit", "button"):
            continue
        value = re.search(r'value="([^"]*)"', tag)
        fields.append((html.unescape(name.group(1)), html.unescape(value.group(1)) if value else ""))
    for name, options in re.findall(r'<select[^>]*name="([^"]+)"[^>]*>(.*?)</select>', inner, re.S):
        picked = re.search(r'<option value="([^"]*)"[^>]*selected', options)
        fields.append((html.unescape(name), html.unescape(picked.group(1)) if picked else ""))
    return fields


def prepared(args):
    """`prepared [--expect N] [--until ID]`: the prepared changes waiting for
    whoever signs in, from the Inbox, each its ID on a line."""
    seconds = number(args, "--seconds", 30)
    expect = option(args, "--expect", None)
    wanted = option(args, "--until", None)
    if args:
        raise Failed(f"prepared takes no {args[0]!r}")
    person = signed_in()

    def listed():
        page = person.get("/inbox/prepared")
        if page.status != 200:
            raise Failed(f"the Inbox: {page.status} {sentence(page)}")
        ids = re.findall(r'<tr data-prepared="(PC-[^"]+)"', page.body)
        if expect is not None and len(ids) != int(expect):
            raise Failed(f"{len(ids)} waiting, not {expect}: {ids}")
        if wanted is not None and wanted not in ids:
            raise Failed(f"{wanted} is not waiting: {ids}")
        return ids

    for found in until(seconds, listed, "the prepared changes"):
        print(found)


def prepared_act(args):
    """`prepared-act make|discard|share|withdraw ID [--note TEXT]
    [--expect-refused WORDS]`: the button on the prepared change's page."""
    if len(args) < 2 or args[0] not in ("make", "discard", "share", "withdraw"):
        raise Failed("prepared-act takes make, discard, share or withdraw and an ID")
    verb, ident = args.pop(0), args.pop(0)
    note = option(args, "--note", "")
    refusal = option(args, "--expect-refused", None)
    if args:
        raise Failed(f"prepared-act takes no {args[0]!r}")
    person = signed_in()
    page = person.get(f"/prepared/{ident}")
    if page.status != 200:
        raise Failed(f"{ident}: {page.status} {sentence(page)}")
    done = person.post(f"/prepared/{ident}/{verb}", {"form_token": form_token(page), "note": note})
    if refusal is not None:
        if done.status == 303:
            raise Failed(f"{ident} was {verb}d, and was to be refused saying {refusal!r}")
        if refusal not in html.unescape(sentence(done)):
            raise Failed(f"{verb} was refused, not saying {refusal!r}: {sentence(done)}")
        print(f"prepared-act: refused: {html.unescape(sentence(done))}")
        return
    if done.status != 303:
        raise Failed(f"{ident} was not {verb}d: {done.status} {sentence(done)}")
    shown = person.get(f"/prepared/{ident}")
    state = re.search(r"<code>" + re.escape(ident) + r"</code> &middot; ([A-Za-z, ]+)", shown.body)
    print(f"prepared-act: {verb}: {state.group(1) if state else 'done'}")


def prepared_form(args):
    """`prepared-form ID [--expect WORDS]`: the plugin's Settings form opened
    from a prepared change -- "Prepared by" and its reason shown -- saved as
    it was started, by whoever signs in."""
    if not args:
        raise Failed("prepared-form takes an ID")
    ident = args.pop(0)
    wanted = option(args, "--expect", None)
    refusal = option(args, "--expect-refused", None)
    person = signed_in()
    page = person.get(f"{VIEW}?tab=settings&prepared={urllib.parse.quote(ident)}")
    if page.status != 200:
        raise Failed(f"the form: {page.status} {sentence(page)}")
    if f'data-prepared="{ident}"' not in page.body:
        raise Failed(f"the form was not started from {ident}")
    shown = html.unescape(page.body)
    if wanted is not None and wanted not in shown:
        raise Failed(f"the form opened from {ident} does not say {wanted!r}")
    fields = form_fields(page.body, f"{VIEW}/settings")
    done = person.post(f"{VIEW}/settings", dict(fields))
    if refusal is not None:
        if done.status == 303:
            raise Failed(f"saved, and was to be refused saying {refusal!r}")
        if refusal not in html.unescape(sentence(done)):
            raise Failed(f"refused, not saying {refusal!r}: {sentence(done)}")
        print(f"prepared-form: refused: {html.unescape(sentence(done))}")
        return
    if done.status != 303:
        raise Failed(f"not saved: {done.status} {sentence(done)}")
    print(f"prepared-form: {ident} saved from the form")


def prepare_settings(args):
    """`prepare-settings NAME=VALUE ... --note TEXT`: the Settings form's
    Prepare, a person preparing a change for someone else to make; prints
    the prepared change's ID."""
    note = option(args, "--note", "")
    asked = pairs(args)
    if args or not asked:
        raise Failed("prepare-settings takes NAME=VALUE, at least one")
    person = signed_in()
    page = person.get(f"{VIEW}?tab=settings")
    if page.status != 200:
        raise Failed(f"the form: {page.status} {sentence(page)}")
    fields = dict(form_fields(page.body, f"{VIEW}/settings"))
    for name, value in asked:
        fields[f"value.{name}"] = value
    fields["prepare"] = "1"
    fields["note"] = note
    done = person.post(f"{VIEW}/settings", fields)
    if done.status != 303 or not (done.location or "").startswith("/prepared/PC-"):
        raise Failed(f"not prepared: {done.status} {done.location} {sentence(done)}")
    print(done.location.rsplit("/", 1)[1])


def agents(args):
    """`agents ACT --level may_make|prepare_only|may_not [--scope act|ceiling|
    group:ID] [--eyes] [--note TEXT] [--plugin]`: what agents may do with an
    act, at the deployment's Agents tab, or with --plugin at the plugin's
    Access tab, as its admin."""
    if not args:
        raise Failed("agents takes an act")
    act = args.pop(0)
    level = option(args, "--level", "may_make")
    scope = option(args, "--scope", "act")
    note = option(args, "--note", "")
    eyes = "--eyes" in args
    if eyes:
        args.remove("--eyes")
    on_plugin = "--plugin" in args
    if on_plugin:
        args.remove("--plugin")
    refusal = option(args, "--expect-refused", None)
    if args:
        raise Failed(f"agents takes no {args[0]!r}")
    person = signed_in()
    page = person.get(VIEW + "?tab=access" if on_plugin else "/admin")
    fields = {"form_token": form_token(page), "act": act, "scope": scope, "level": level, "note": note}
    if eyes:
        fields["second_pair_of_eyes"] = "1"
    done = person.post(f"{VIEW}/agents" if on_plugin else "/admin/agents", fields)
    if refusal is not None:
        if done.status == 303:
            raise Failed(f"set, and was to be refused saying {refusal!r}")
        if refusal not in html.unescape(sentence(done)):
            raise Failed(f"refused, not saying {refusal!r}: {sentence(done)}")
        print(f"agents: refused: {html.unescape(sentence(done))}")
        return
    if done.status != 303:
        raise Failed(f"not set: {done.status} {sentence(done)}")
    print(f"agents: {act} at {scope}: {level}{', second pair of eyes' if eyes else ''}")


def linkable(args):
    """`linkable --grant ACCOUNT | --withdraw ACCOUNT [--note TEXT]`: the
    plugin's Accounts tab, a deployment admin's grant."""
    grant = option(args, "--grant", "")
    withdraw = option(args, "--withdraw", "")
    note = option(args, "--note", "")
    if args or not (grant or withdraw):
        raise Failed("linkable takes --grant or --withdraw and an account")
    person = signed_in()
    page = person.get(VIEW + "?tab=accounts")
    if page.status != 200:
        raise Failed(f"its Accounts tab: {page.status} {sentence(page)}")
    done = person.post(f"{VIEW}/accounts", {"form_token": form_token(page), "grant": grant,
                                            "withdraw": withdraw, "note": note})
    if done.status != 303:
        raise Failed(f"not changed: {done.status} {sentence(done)}")
    shown = person.get(VIEW + "?tab=accounts")
    print(f"linkable: {len(re.findall(r'<tr data-id=', shown.body))} granted")


def service_account(args):
    """`service-account ...`: the Service accounts tab and a role's runs-as
    (contract v21; see the module's comment)."""
    if not args:
        raise Failed("service-account takes create, set, disable, enable, remove, grant, runs-as or list")
    act = args.pop(0)
    refusal = option(args, "--expect-refused", None)
    person = signed_in()

    def login_of(name):
        return LOGIN if name in (None, ADMIN) else f"local|{name}"

    def rows():
        page = person.get("/admin")
        if page.status != 200:
            raise Failed(f"the Settings page: {page.status} {sentence(page)}")
        return page, re.findall(
            r'<tr data-id="(SA-[0-9A-Z]+)" data-name="([^"]*)">(.*?)</tr>', page.body, re.S)

    def filled(row_html):
        found = re.search(r'data-fill="([^"]+)"', row_html)
        return json.loads(html.unescape(found.group(1)))["fields"] if found else {}

    def done(reply, what):
        if refusal is not None:
            if reply.status == 303:
                raise Failed(f"{what}, and was to be refused saying {refusal!r}")
            said = html.unescape(sentence(reply))
            if refusal not in said:
                raise Failed(f"refused, not saying {refusal!r}: {said}")
            print(f"service-account: refused: {said}")
            return False
        if reply.status != 303:
            raise Failed(f"{what}: {reply.status} {html.unescape(sentence(reply))}")
        return True

    if act == "list":
        _, found = rows()
        for sa, name, row_html in found:
            setting = re.search(r'data-setting="([a-z_]+)"', row_html)
            state = re.search(r'data-state="([a-z]+)"', row_html)
            print(f"{sa}\t{html.unescape(name)}\t{setting.group(1) if setting else ''}\t"
                  f"{state.group(1) if state else ''}")
        return
    if act == "create":
        if not args:
            raise Failed("service-account create takes a name")
        name = args.pop(0)
        owner = option(args, "--owner", None)
        setting = option(args, "--setting", "prepare_only")
        purpose = option(args, "--purpose", "")
        if args:
            raise Failed(f"service-account create takes no {args[0]!r}")
        page, _ = rows()
        reply = person.post("/admin/service-accounts", {
            "form_token": form_token(page), "service_account_id": "", "name": name,
            "purpose": purpose, "owner_subject": login_of(owner), "setting": setting,
            "note": "created by the harness"})
        if not done(reply, "not created"):
            return
        _, found = rows()
        minted = [sa for sa, listed, _ in found if html.unescape(listed) == name]
        if not minted:
            raise Failed(f"{name!r} is not listed once created")
        print(minted[-1])
        return
    if not args:
        raise Failed(f"service-account {act} takes a service account's SA- ID")
    sa = args.pop(0)
    if act == "set":
        setting = option(args, "--setting", None)
        name = option(args, "--name", None)
        owner = option(args, "--owner", None)
        if args:
            raise Failed(f"service-account set takes no {args[0]!r}")
        page, found = rows()
        row = [row_html for listed, _, row_html in found if listed == sa]
        if not row:
            raise Failed(f"{sa} is not listed")
        fields = filled(row[0])
        reply = person.post("/admin/service-accounts", {
            "form_token": form_token(page), "service_account_id": sa,
            "name": name or fields.get("name", ""), "purpose": fields.get("purpose", ""),
            "owner_subject": login_of(owner) if owner else fields.get("owner_subject", ""),
            "setting": setting or fields.get("setting", ""), "note": "changed by the harness"})
        if done(reply, "not changed"):
            print(f"service-account: {sa} changed")
        return
    if act in ("disable", "enable", "remove"):
        if args:
            raise Failed(f"service-account {act} takes no {args[0]!r}")
        page = person.get("/service-accounts")
        to = {"disable": "disabled", "enable": "active", "remove": "removed"}[act]
        reply = person.post("/admin/service-accounts/state", {
            "form_token": form_token(page), "service_account_id": sa, "state": to,
            "note": f"{act}d by the harness"})
        if done(reply, f"not {to}"):
            print(f"service-account: {sa} {to}")
        return
    if act == "grant":
        role = option(args, "--role", None)
        level = option(args, "--level", "write")
        if args or level not in ("read", "write"):
            raise Failed("service-account grant takes --role ROLE --level read|write")

        def post(path, fields):
            token = form_token(person.get("/admin"))
            reply = person.send("POST", path, fields + [("form_token", token)])
            if reply.status != 303:
                raise Failed(f"{path}: {reply.status} {sentence(reply)}")

        def defined(path, field, name, fields):
            def minted():
                found = re.search(r'<tr data-id="([^"]+)" data-name="'
                                  + re.escape(html.escape(name)) + '"', person.get("/admin").body)
                return found.group(1) if found else ""
            held = minted()
            post(path, ([(field, held)] if held else []) + [("name", name)] + fields)
            held = minted()
            if not held:
                raise Failed(f"{path}: {name!r} is not listed once defined")
            return held

        user_group = defined("/admin/user-groups", "user_group_id", f"Harness {sa}", [("login", sa)])
        access_group = defined("/admin/access-groups", "access_group_id",
                               f"Harness {level} on {INSTANCE}" + (f" {role}" if role else ""),
                               [(f"level.{INSTANCE}:{role}", level)] if role is not None
                               else [("plugin", INSTANCE), (f"level.{INSTANCE}", level)])
        post("/admin/permissions", [("user_group_id", user_group), ("account_group_id", ALL_ACCOUNTS),
                                    ("access_group_id", access_group)])
        print(f"service-account: {sa} holds {level} on {INSTANCE}" + (f" {role}" if role else ""))
        return
    if act == "runs-as":
        role = option(args, "--role", "")
        level = option(args, "--level", "write")
        if args:
            raise Failed(f"service-account runs-as takes no {args[0]!r}")
        page = person.get(VIEW + "?tab=access")
        if page.status != 200:
            raise Failed(f"its Access tab: {page.status} {sentence(page)}")
        reply = person.post(f"{VIEW}/runs-as", {
            "form_token": form_token(page), "role": role,
            "service_account_id": "" if sa == "none" else sa, "level": level,
            "note": "chosen by the harness"})
        if done(reply, "not chosen"):
            print(f"service-account: {INSTANCE} {role or 'as a whole'} runs as "
                  + ("nobody" if sa == "none" else f"{sa} at {level}"))
        return
    raise Failed(f"service-account takes no {act!r}")


def connectivity(args):
    """`connectivity available|link ...`: the connectivity plugin's ems, on
    its view's Overview (W11.21, contract v22)."""
    if not args or args[0] not in ("available", "link"):
        raise Failed("connectivity takes available or link")
    what = args.pop(0)
    note = option(args, "--note", "set by the harness")
    person = signed_in()
    page = person.get(VIEW)
    if page.status != 200:
        raise Failed(f"its view: {page.status} {sentence(page)}")
    token = form_token(page)
    if what == "available":
        withdraw = "--withdraw" in args
        if withdraw:
            args.remove("--withdraw")
        if args:
            raise Failed(f"connectivity available takes no {args[0]!r}")
        done = person.send("POST", f"{VIEW}/connectivity/available",
                           [("form_token", token), ("available", "false" if withdraw else "true"),
                            ("note", note)])
    else:
        ems = option(args, "--ems", "")
        users = options(args, "--user")
        if args:
            raise Failed(f"connectivity link takes no {args[0]!r}")
        done = person.send("POST", f"{VIEW}/connectivity/link",
                           [("form_token", token), ("ems_instance_id", ems), ("note", note)]
                           + [("user", user) for user in users])
    if done.status != 303:
        raise Failed(f"connectivity {what}: {done.status} {sentence(done)}")
    shown = person.get(VIEW)
    found = re.search(r'<table class="list one-line connectivity"[^>]*>(.*?)</table>', shown.body, re.S)
    cells = re.findall(r"<td[^>]*>(.*?)</td>", found.group(1), re.S) if found else []
    print(f"connectivity: {INSTANCE}: " + " | ".join(
        " ".join(re.sub(r"<[^>]+>", "", cell).split()) for cell in cells))


def instruction(args):
    """`instruction VALUE (--all|--account|--plugin|--person|--service-account
    ID) [--within PLUGIN] note=TEXT`: the Trading instructions page (W11.15,
    contract v22)."""
    refusal = option(args, "--expect-refused", None)
    within = option(args, "--within", "")
    targets = [("--account", "account"), ("--plugin", "plugin"), ("--person", "person"),
               ("--service-account", "service_account")]
    kind, named = None, ""
    if "--all" in args:
        args.remove("--all")
        kind = "all"
    for flag, said in targets:
        given = option(args, flag, None)
        if given is not None:
            if kind is not None:
                raise Failed("instruction names one target")
            kind, named = said, given
    note = dict(pairs(args)).get("note", "")
    values = [a for a in args if "=" not in a]
    if kind is None or len(values) != 1 or values[0] not in ("active", "pause", "stop"):
        raise Failed("instruction takes active, pause or stop and one target")
    instance = named if kind == "plugin" else within
    person = signed_in()
    page = person.get("/admin/trading-instructions")
    if page.status != 200:
        raise Failed(f"the Trading instructions page: {page.status} {sentence(page)}")
    done = person.post("/admin/trading-instructions", {
        "form_token": form_token(page), "kind": kind,
        "id": "" if kind in ("all", "plugin") else named, "instance": instance,
        "value": values[0], "note": note})
    said = refused_or_done(done, refusal, f"the {values[0]} instruction")
    if said:
        print(f"instruction: {said}")
        return
    shown = person.get("/admin/trading-instructions")
    lines = re.findall(r'<tr data-id="([^"]+)" data-name="([^"]+)" data-value="([^"]+)">', shown.body)
    print("instruction: " + "; ".join(f"{html.unescape(name)} {value}" for _, name, value in lines))


def attribute(args):
    """`attribute ACCOUNT waits=true|false note=TEXT`: the Accounts tab's book
    part (W9.13, contract v22)."""
    refusal = option(args, "--expect-refused", None)
    given = dict(pairs(args))
    rest = [a for a in args if "=" not in a]
    if len(rest) != 1 or given.get("waits") not in ("true", "false"):
        raise Failed("attribute takes an account and waits=true|false")
    person = signed_in()
    page = person.get("/admin")
    done = person.post("/admin/accounts/book", {
        "form_token": form_token(page), "account_id": rest[0], "base_currency_code": "",
        "lot_relief_default": "0",
        "waits_for_compliance": "wait" if given["waits"] == "true" else "no-wait",
        "note": given.get("note", "")})
    said = refused_or_done(done, refusal, "the attribute")
    print(f"attribute: {said or rest[0] + ' waits ' + given['waits']}")


def model_routing(args):
    """`model-routing MODEL_ID IMPLEMENTATION note=TEXT`: the Settings' Model
    routing tab (W12.6), as a deployment admin routes a model id."""
    refusal = option(args, "--expect-refused", None)
    given = dict(pairs(args))
    rest = [a for a in args if "=" not in a]
    if len(rest) != 2:
        raise Failed("model-routing takes a model id and an implementation")
    person = signed_in()
    page = person.get("/admin")
    done = person.post("/admin/model-routing", {
        "form_token": form_token(page), "model_id": rest[0], "implementation": rest[1],
        "note": given.get("note", "")})
    said = refused_or_done(done, refusal, "the routing")
    print(f"model-routing: {said or rest[0] + ' routed to ' + rest[1]}")


def core_form(args):
    """`core-form PAGE POST FIELD=VALUE... [--expect-refused TEXT]`: one of
    core's own forms, as the deployment admin signed in: PAGE read for its
    form token, POST sent with the fields given (contract v24: the escape
    hatch's setting, the usage report's, an account's Close; the migration's
    lines, a FIELD given more than once sent once for each, as a form's
    repeated field is)."""
    refusal = option(args, "--expect-refused", None)
    fields = pairs(args)
    rest = [a for a in args if "=" not in a]
    if len(rest) != 2:
        raise Failed("core-form takes the page it is on and the path it posts to")
    person = signed_in()
    page = person.get(rest[0])
    fields.append(("form_token", form_token(page)))
    done = person.post(rest[1], fields)
    said = refused_or_done(done, refusal, rest[1])
    print(f"core-form: {said or 'done'}")


COMMANDS = {"ready": ready, "settings": settings, "hold": hold, "archive": archive,
            "summary": summary, "view": view, "account": account, "page": page,
            "form": form, "unlinked": unlinked, "grant": grant, "compose": compose,
            "instruments": instruments, "instrument": instrument, "mcp": mcp,
            "ticket": ticket, "inbox": inbox, "licence": licence, "entitle": entitle,
            "priority": priority, "prepared": prepared, "prepared-act": prepared_act,
            "prepared-form": prepared_form, "prepare-settings": prepare_settings,
            "agents": agents, "linkable": linkable, "core-page": core_page,
            "service-account": service_account, "connectivity": connectivity,
            "instruction": instruction, "attribute": attribute, "core-form": core_form,
            "model-routing": model_routing}


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
