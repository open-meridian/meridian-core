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
      and a password drawn for each. The first is the runner's plugin.

  ready [--seconds N]
      Until the plugin has registered with its sidecar and the dashboard lists
      it (healthy or not): the first command of a run.

  settings NAME=VALUE ... [--seconds N]
      Sets the plugin's settings in its settings form, as its admin does
      (W6.11), once the plugin has declared each. A secret goes in its secret
      field; a developer's setting is accepted because the harness's dashboard
      is a development one.

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

Reading core's HTML is this runner's alone, and only because it is published
at the same commit as the dashboard it reads, and core's gate runs it against
that dashboard at every commit. A plugin never parses core's pages itself.
"""
import html
import json
import os
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
# The built-in account group holding every account (W6.6).
ALL_ACCOUNTS = "all-accounts"


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


def drawn_password():
    """The admin's password, as `keys` drew it when the run started. Read,
    never printed: a failure names the file, not what is in it."""
    try:
        with open(PASSWORD_FILE, encoding="utf-8") as drawn:
            password = drawn.read().strip()
    except OSError as unread:
        raise Failed(f"the admin's password was not drawn: {PASSWORD_FILE}: {unread.strerror}") from None
    if not password:
        raise Failed(f"the admin's password was not drawn: {PASSWORD_FILE} is empty")
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
        signed = admin.post("/sign-in", {"name": ADMIN, "password": drawn_password()})
        if signed.status != 303:
            raise Failed(f"signing in as {ADMIN}: {signed.status} {sentence(signed)}")
        settings = admin.get("/admin")
        if settings.status != 200:
            raise Failed(f"{ADMIN} is not yet the deployment's admin: /admin {settings.status}")
        return admin
    return until(seconds, attempt, f"{ADMIN} was not signed in as the deployment's admin")


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
    asked = pairs(args)
    if args or not asked:
        raise Failed("settings takes NAME=VALUE, at least one")
    admin = signed_in()

    def declared():
        form = admin.get(f"{VIEW}?tab=settings")
        if form.status != 200:
            raise Failed(f"its settings: {form.status} {sentence(form)}")
        missing = [name for name, _ in asked if f'data-setting="{name}"' not in form.body]
        if missing:
            raise Failed(f"the plugin has not declared {', '.join(missing)}")
        return form

    form = until(seconds, declared, "the settings form never offered them")
    fields = {"form_token": form_token(form)}
    for name, value in asked:
        secret = f'name="secret.{name}"' in form.body
        fields[("secret." if secret else "value.") + name] = value
    saved = admin.post(f"{VIEW}/settings", fields)
    if saved.status != 303:
        raise Failed(f"the settings were not saved: {saved.status} {sentence(saved)}")
    print(f"settings: saved {', '.join(name for name, _ in asked)}")


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
    if level not in LEVELS:
        raise Failed(f"grant takes --level {'|'.join(LEVELS)}")
    if args:
        raise Failed(f"grant takes no {args[0]!r}")
    admin = signed_in()

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

    user_group = defined("/admin/user-groups", "user_group_id", "Harness admin",
                         [("login", LOGIN)])
    access_group = defined("/admin/access-groups", "access_group_id",
                           f"Harness {level} on {INSTANCE}",
                           [("plugin", INSTANCE), (f"level.{INSTANCE}", level)])
    post("/admin/permissions", [("user_group_id", user_group),
                                ("account_group_id", ALL_ACCOUNTS),
                                ("access_group_id", access_group)])
    print(f"grant: {ADMIN} holds {level} on {INSTANCE}, on All accounts")


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
         "instrument", "conductor", "dashboard", "runner", "store",
         "runtime", "first-run", "dashboard-1"}
RUNTIME_IMAGE = "${MERIDIAN_RUNTIME_IMAGE:?set MERIDIAN_RUNTIME_IMAGE to the runtime image of the harness's commit}"


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
    services = {
        "keys": {"environment": {"MERIDIAN_HARNESS_BROKER_USERS": " ".join(names)}},
        "broker-config": {"environment": {
            "MERIDIAN_HARNESS_INSTANCES": json.dumps({"instances": instances})}},
        "runner": {"environment": {"MERIDIAN_HARNESS_INSTANCE": names[0]}},
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
    written = {
        "x-harness": "written by harness.py compose for " + ", ".join(names)
                     + "; written again, never edited, for another list",
        "services": services,
    }
    print(json.dumps(written, indent=2))


COMMANDS = {"ready": ready, "settings": settings, "account": account, "page": page,
            "form": form, "unlinked": unlinked, "grant": grant, "compose": compose}


def main(argv):
    if not argv or argv[0] not in COMMANDS:
        print(__doc__, file=sys.stderr)
        return 2
    command, args = argv[0], list(argv[1:])
    try:
        acting_on(option(args, "--instance", INSTANCE))
        COMMANDS[command](args)
    except Failed as failed:
        print(f"harness {command} FAILED: {failed}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
