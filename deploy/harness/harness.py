"""The plugin harness's runner: what a person does in the dashboard, done as
the harness's deployment admin, through the dashboard's own pages and forms.

Standard library only. Run as the compose file's `runner` service:

    docker compose ... run --rm runner <command> [arguments]

Each command signs in afresh, does one thing, prints what it found, and exits
0; or exits non-zero saying why. Every wait has a bound (`--seconds`).

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

  form --level L --page PATH --post PATH [--csrf-field NAME] [--expect TEXT] FIELD=VALUE ...
      The same session: reads PATH, takes its CSRF field (`csrf`, as the SDK
      names it; a FIELD given by that name wins) when it has one, and posts
      the fields urlencoded to --post, a field repeated as often as given.
      Prints the status, then the body. With --expect, fails unless the body
      says TEXT. This is how a plugin's own link page is driven: a link is
      the plugin's to send, for an admin (W6.4), never the harness's.

  unlinked [--expect N] [--seconds N]
      Prints how many external accounts the plugin reported that nothing
      links, as the dashboard counts them on its line (W6.4, W6.10). With
      --expect, waits until it is N.

Reading core's HTML is this runner's alone, and only because it ships in the
same image as the dashboard it reads, and core's gate runs it against that
dashboard at every commit. A plugin never parses core's pages itself.
"""
import html
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
PASSWORD = os.environ.get("MERIDIAN_HARNESS_ADMIN_PASSWORD", "")
# The plugin's own host, below the dashboard's (decisions/021). A browser
# resolves it by name; this sends to the dashboard naming the host, which is
# the same request.
PLUGIN_HOST = f"{INSTANCE}.plugins.{urllib.parse.urlparse(DASHBOARD).netloc}"
VIEW = f"/admin/plugins/{INSTANCE}"
LEVELS = ("admin", "write", "read")


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


def signed_in(seconds=120):
    """The admin, signed in to the dashboard, once the dashboard answers and
    her account is the deployment's admin: the conductor names her at its
    start, and the dashboard reads that a moment later."""
    def attempt():
        admin = Browser()
        if admin.get("/healthz").status != 200:
            raise Failed("the dashboard is not serving yet")
        admin.get("/sign-in")
        signed = admin.post("/sign-in", {"name": ADMIN, "password": PASSWORD})
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
    if not any(name == csrf for name, _ in fields):
        token = csrf_of(read.body, csrf)
        if token is not None:
            fields.append((csrf, token))
    reply = followed(host, host.post(target, fields))
    print(reply.status)
    print(reply.body)
    if reply.status >= 400:
        raise Failed(f"{target} refused the form: {reply.status}")
    if text is not None and text not in reply.body:
        raise Failed(f"{target} did not say {text!r}")


def csrf_of(body, name):
    """The value of the form field `name` on a page, or None."""
    for field in re.findall(r"<input\b[^>]*>", body):
        if re.search(r'\bname="' + re.escape(name) + '"', field):
            found = re.search(r'\bvalue="([^"]*)"', field)
            return found.group(1) if found else ""
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


COMMANDS = {"ready": ready, "settings": settings, "account": account, "page": page,
            "form": form, "unlinked": unlinked}


def main(argv):
    if not argv or argv[0] not in COMMANDS:
        print(__doc__, file=sys.stderr)
        return 2
    command, args = argv[0], list(argv[1:])
    try:
        COMMANDS[command](args)
    except Failed as failed:
        print(f"harness {command} FAILED: {failed}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
