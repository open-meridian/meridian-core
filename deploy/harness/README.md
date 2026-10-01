# The plugin harness

The smallest deployment a plugin can prove itself against: a released runtime
image's own components, a broker and a database, the plugin beside its
sidecar, a runner that does what a person does in the dashboard, and the SQL
that prints what the street store kept. A plugin's own `make e2e` runs it
against the image it pins; core's `make harness-check` runs it at every commit
with core's stand-in plugin, so a change here that would break a plugin's e2e
breaks core's gate first.

**It is a development deployment, for a test, and never a way to run one.** It
has no platform, so no instrument resolves and every row a plugin records names
the deployment's placeholder; its dashboard runs with `MERIDIAN_DEVELOPMENT`,
so developer settings are shown and accepted; its account, its passwords and
its keys are fixed test values; and nothing outlives `down -v`. It publishes no
port. To run a deployment, install the chart.

It names no plugin, and adds nothing to the contract: a plugin reaches it only
through its sidecar, as it reaches any deployment.

## What is in it

`/usr/share/meridian/harness/` in `ghcr.io/open-meridian/meridian-runtime`,
versioned with the binaries beside it:

| File | What it is |
|---|---|
| `compose.yaml` | The deployment: Postgres, NATS with a configuration generated for one plugin instance and its roles, the stores migrated, street, instrument, conductor, a development dashboard, the plugin's sidecar, the `plugin` service, and the `runner` |
| `harness.py` | The runner, standard library only: `ready`, `settings`, `account`, `page`, `form`, `unlinked` |
| `street.sql` | The street store as stable, sorted lines |

## Taking it out of the image

Pin the image by tag and digest, and copy the directory out of that image. Any
command will do for `create`; it is never run.

    RUNTIME_IMAGE=ghcr.io/open-meridian/meridian-runtime:<commit>@sha256:<digest>
    id=$(docker create "$RUNTIME_IMAGE" none)
    docker cp "$id:/usr/share/meridian/harness" .e2e/harness
    docker rm "$id"

## Variables

Every compose command needs all three, `down` and `run` included, since compose
reads the whole file each time:

| Variable | Value |
|---|---|
| `MERIDIAN_RUNTIME_IMAGE` | the image the harness was copied from, so the file never guesses its own tag |
| `MERIDIAN_HARNESS_PLUGIN_IMAGE` | the plugin's image |
| `MERIDIAN_HARNESS_PLUGIN_ROLES` | its roles, comma separated, as it declares them; empty for a plugin holding none. A name that is not a role stops the run at `broker-config`, naming it |

Name the project after the plugin (`-p`), so two harnesses never share one.

The plugin is instance `plugin-1`, in its sidecar's network namespace, and
reaches the sidecar at `127.0.0.1:9191` (`MERIDIAN_SIDECAR_ADDRESS`). To give it
environment, a command or a volume, add a compose file of your own overriding
the `plugin` service (`-f .e2e/harness/compose.yaml -f e2e/plugin.yaml`); give a
path in it absolutely, since a second file's relative paths resolve against the
first file's directory.

The deployment's admin is the dashboard's local account `harness`, named the
deployment's admin by the conductor at its start, as first run names one. The
runner signs in as it; nothing else needs to.

## The runner

    docker compose ... run --rm -T runner <command>

Each command signs in, does one thing, prints what it found and exits 0, or
exits non-zero saying why. Every wait is bounded by `--seconds`.

| Command | What it does |
|---|---|
| `ready [--seconds N]` | Waits until the plugin has registered and the dashboard lists it, healthy or not. The first command of a run. |
| `settings NAME=VALUE ... [--seconds N]` | Sets the plugin's settings in its settings form, once it has declared each; a secret in its secret field. A developer setting is accepted here. |
| `account NAME [--seconds N]` | Defines an account and prints its ID, for a page that links only to an account that exists. |
| `page --level admin\|write\|read PATH [--until TEXT] [--seconds N]` | Opens a session on the plugin's own host at that level (Manage, Open, View), GETs `PATH`, prints the status and then the body; with `--until`, again until the body says `TEXT`. |
| `form --level L --page PATH --post PATH [--csrf-field NAME] [--expect TEXT] FIELD=VALUE ...` | In such a session, reads `--page`, takes its CSRF field (`csrf`, the SDK's name; a field you give by that name wins), posts the fields urlencoded to `--post`, prints the status and the body. With `--expect`, fails unless the body says `TEXT`. A 4xx or 5xx fails. |
| `unlinked [--expect N] [--seconds N]` | Prints how many external accounts the plugin reported that nothing links, as the dashboard counts them; with `--expect`, waits for `N`. |

A link is the plugin's to send, acting for an admin, so the harness never sends
one: `form` drives the plugin's own link page under Manage, which is also the
proof that the page links.

## The street store

    docker compose ... exec -T postgres psql -U meridian -d meridian -At -v ON_ERROR_STOP=1 -f /harness/street.sql

prints every account's rows, ordered bytewise:

    assumed|<account>|<instrument>|<side>
    position|<account>|<instrument>|<side>|<quantity>|<settle-date quantity>|<market value> <currency>|<in-cash>
    statement|<account>|<source>|<expected rows>|<complete or open>|<buying power>|<margin requirement>|<maintenance excess>|<currency assumed>

- `<account>` is the account's name, since its ID is minted per run.
- `<instrument>` is an `INS-` ID, or for a placeholder the identifiers it stands
  for, sorted: `placeholder(figi:BBG000B9XRY4, symbol:AAPL@snaptrade)`. In the
  harness every instrument is a placeholder, so an expected file names each row
  by the identifiers the plugin sent.
- A number is printed at the scale it was stated with; what was not reported is
  empty, never zero. `<in-cash>` is `in-cash` for a position the venue also
  counts in cash.
- `assumed` is a row of a latest statement whose currency the connector assumed.
- `statement` is the latest statement of each source for each account, by its
  rows' account. A statement none of whose rows was recorded (its account
  unlinked) belongs to no account and is not printed.

An account nothing links has no rows, so a file listing every account proves
both what a linked account holds and that an unlinked one holds nothing. Pair
it with `unlinked`, which proves the plugin reported the unlinked ones.

## A plugin's e2e, in outline

    H="docker compose -p <plugin>-e2e -f .e2e/harness/compose.yaml"
    export MERIDIAN_RUNTIME_IMAGE=$RUNTIME_IMAGE MERIDIAN_HARNESS_PLUGIN_IMAGE=<plugin image> MERIDIAN_HARNESS_PLUGIN_ROLES=custody
    $H down -v --remove-orphans
    $H up -d
    $H run --rm -T runner ready
    $H run --rm -T runner settings <name>=<value>
    $H run --rm -T runner form --level admin --page <its link page> --post <its link action> <fields> --expect "<what it says>"
    # poll street.sql until the statement you expect is `complete`, not merely present
    $H exec -T postgres psql -U meridian -d meridian -At -v ON_ERROR_STOP=1 -f /harness/street.sql > .e2e/street
    diff -u e2e/expected.street .e2e/street
    $H run --rm -T runner unlinked --expect <n>
    $H logs --no-color > .e2e/components.log   # on a failure, before tearing down
    $H down -v --remove-orphans

A setting changed and a link made each wake a plugin that reads on them, so wait
for a complete statement rather than for any row, or the diff can catch one
half recorded. core's `harness-check` target in its `Makefile` is a working
example.
