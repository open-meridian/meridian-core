# The plugin harness

The smallest deployment a plugin can prove itself against: a released runtime
image's own components, a broker and a database, any number of plugins each
beside its own sidecar, a runner that does what a person does in the
dashboard, and a `store` service that prints what the stores kept. A plugin's
own e2e runs it against the runtime it pins; core's `make harness-check` runs
it at every commit with three of core's stand-in plugins, so a change here
that would break a plugin's e2e breaks core's gate first.

**It is a development deployment, for a test, and never a way to run one.** It
has no platform, so no instrument resolves and every row a plugin records names
the deployment's placeholder; its dashboard runs with `MERIDIAN_DEVELOPMENT`,
so developer settings are shown and accepted; and nothing outlives `down -v`.
It publishes no port. To run a deployment, install the chart.

It names no plugin, and adds nothing to the contract: a plugin reaches it only
through its sidecar, as it reaches any deployment.

## What is in it

`ghcr.io/open-meridian/meridian-harness`, an image of files only, published at
each commit of meridian-core with the runtime image of the same commit and the
same tag. The runtime image carries none of it.

| File | What it is |
|---|---|
| `compose.yaml` | The deployment: `keys`, which draws the run's keys and passwords; Postgres; NATS configured for the plugins and their roles; the stores migrated; street, the book (`bor`), instrument, conductor and a development dashboard; the `runner`; and `store` |
| `harness.py` | The runner, standard library only: `ready`, `settings`, `account`, `page`, `form`, `unlinked`, `grant`; and `compose`, which writes the plugins' half of the deployment |
| `street.sql` | The street store as stable, sorted lines, which `store street` prints |
| `book.sql` | The book of record as stable, sorted lines (contract v8), which `store book` prints |

## Taking it out of the image

Pin the runtime image by tag and digest, and the harness image at the same
commit's tag beside it, and copy the directory out of the harness image. Any
command will do for `create`; it is never run.

    RUNTIME_IMAGE=ghcr.io/open-meridian/meridian-runtime:<commit>@sha256:<digest>
    HARNESS_IMAGE=ghcr.io/open-meridian/meridian-harness:<commit>@sha256:<digest>
    id=$(docker create "$HARNESS_IMAGE" none)
    docker cp "$id:/harness" .e2e/harness
    docker rm "$id"

A harness at one commit and a runtime at another may disagree about the
dashboard's pages the runner reads; keep the two tags the same.

## The plugins

`compose.yaml` is the deployment without its plugins. The plugins are a second
compose file, written by the harness itself from a JSON list of any number of
them, in the runner's image, before anything starts:

    [
      {"instance": "snaptrade", "image": "ghcr.io/open-meridian/meridian-snaptrade:<version>", "roles": ["custody"]},
      {"instance": "operations", "image": "meridian-plugin/sample-operations:dev", "roles": ["operations"]}
    ]

    docker run --rm -i -v "$PWD/.e2e/harness":/harness:ro python:3.12-alpine \
        python /harness/harness.py compose <e2e/plugins.json >.e2e/harness/plugins.yaml

For each plugin it writes a sidecar, as instance `instance` holding its
`roles`, and the plugin itself as the compose service named `instance`, in its
sidecar's network namespace, reaching it at `127.0.0.1:9191`
(`MERIDIAN_SIDECAR_ADDRESS`). Each plugin is started again whenever it exits
failing, as a pod's container is: a plugin whose first call reaches its
sidecar before the deployment serves is refused, exits, and finds it serving
on its next start. It also configures the broker for every instance and its
roles; a name that is not a role stops the run at `broker-config`, naming it.

- `instance` is lower case letters, digits and inner hyphens, at most 32,
  starting with a letter, and none of the names the harness already holds
  (its services, `runtime`, `first-run`, `dashboard-1`, or `sidecar-...`).
- `roles` is a list, empty for a plugin holding none.
- The first plugin listed is the runner's, when a command names no
  `--instance`.

Write the file again for another list; never edit it. Every compose command
then names both files, and the one variable the deployment needs, `down` and
`run` included, since compose reads every file each time:

    export MERIDIAN_RUNTIME_IMAGE=$RUNTIME_IMAGE
    H="docker compose -p <plugin>-e2e -f .e2e/harness/compose.yaml -f .e2e/harness/plugins.yaml"

Name the project after the plugin (`-p`), so two harnesses never share one.

To give a plugin environment, a command or a volume, add a compose file of your
own overriding its service by its instance name (`-f e2e/plugin.yaml`, a
`services: {snaptrade: {environment: ...}}`); give a path in it absolutely,
since a later file's relative paths resolve against the first file's
directory. A plugin that restarts on failure needs no override for it.

## Keys and passwords

None is written in the harness. When the run starts, `keys` draws at random
the dashboard's signing key, the key secret settings are sealed with, the
database's password, a password for each broker user, and the admin's
password with its Argon2id hash. They live in the run's volumes until `down
-v`. The dashboard makes its one local account, `harness`, from the hash at its
start, and the conductor names that account the deployment's admin, as first
run names one. The runner signs in with the password, read from the run's
`secrets` volume; nothing else needs to, and it is never printed.

## The runner

    $H run --rm -T runner <command>

Each command signs in, does one thing, prints what it found and exits 0, or
exits non-zero saying why. Every wait is bounded by `--seconds`.

| Command | What it does |
|---|---|
| `ready [--seconds N]` | Waits until the plugin has registered and the dashboard lists it, healthy or not. The first command of a run. |
| `settings NAME=VALUE ... [--seconds N]` | Sets the plugin's settings in its settings form, once it has declared each; a secret in its secret field. A developer setting is accepted here. |
| `account NAME [--seconds N]` | Defines an account and prints its ID, for a page that links only to an account that exists. |
| `page --level admin\|write\|read PATH [--until TEXT] [--seconds N]` | Opens a session on the plugin's own host at that level (Manage, Open, View), GETs `PATH`, prints the status and then the body; with `--until`, again until the body says `TEXT`. |
| `form --level L --page PATH --post PATH [--csrf-field NAME] [--from-page NAME ...] [--expect TEXT] FIELD=VALUE ...` | In such a session, reads `--page`, takes its CSRF field (`csrf`, the SDK's name) and each field `--from-page` names (repeated, or comma separated) with the value the page gives it -- a proposal's digest, say -- and posts them with the fields you give, urlencoded, to `--post`; prints the status and the body. A field you give wins over one taken from the page; a field the page does not have fails. With `--expect`, fails unless the body says `TEXT`. A 4xx or 5xx fails. |
| `unlinked [--expect N] [--seconds N]` | Prints how many external accounts the plugin reported that nothing links, as the dashboard counts them; with `--expect`, waits for `N`. |
| `grant --level read\|write\|admin` | Grants the harness's admin that level on the plugin, on All accounts, as a deployment admin does: a user group holding the admin, an access group, and the permission joining them. The admin holds nothing on a plugin until granted; a session at `write` (Open) needs write. |

Every command acts on the first plugin listed, or on another with `--instance
<instance>`.

A link is the plugin's to send, acting for an admin, so the harness never sends
one: `form` drives the plugin's own link page under Manage, which is also the
proof that the page links.

## The stores

    $H run --rm -T store street
    $H run --rm -T store book

prints that store as stable, sorted lines, so a file from one run compares
with the next. Whoever reads a store needs no database user, password, file or
query of their own.

### The street store

`store street` prints every account's rows, ordered bytewise:

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

### The book of record

`store book` prints every account's book, ordered bytewise within each kind of
line:

    attributes|<account>|<base currency>|<lot relief>|<opening balance as of>
    position|<account>|<instrument>|<side>|<trade date>|<settled>|<not stated>|<effective date>
    pending|<account>|<instrument>|<side>|<value date>|<quantity>|<failing>
    lot|<account>|<instrument>|<side>|<order opened>|<open>|<original>|<cost> <currency>|<acquired>|<source>
    break|<account>|<subject>|<category>|<state>|<first seen>|<last seen>|<recorded by>|<confirmed cause>|<resolved by>
    figures|<account>|<external account>/<segment>|<business date>
    entry|<account>|<kind>|<effective date>|<actor>

- `<account>` and `<instrument>` as the street store prints them. No identifier
  the book mints -- an entry's, a lot's, a break's -- and no number in a
  partition or time is printed, so a file from one run compares with the next.
- `<settled>` is empty while any of the quantity is not stated: unknown, never
  zero. A `pending` line with no value date is "date not stated", from an
  opening balance whose source gave a settled quantity.
- An enum is its number, 0 for none: `<lot relief>` (LotReliefMethod),
  `<category>` (BreakCategory), `<state>` (BreakState: 1 open, 2 resolved, 3
  closed), `<confirmed cause>` (BreakCauseCategory), `<source>` (LotSource).
  `<resolved by>` is `entries`, `explanation` or `cleared`, empty while open.
- `<recorded by>` and `<actor>` are a person's subject, `instance <id>` for a
  finding a plugin sent as itself, or `book` for the book's own act.
- Entries are listed in the order each account's were made.

## A plugin's e2e, in outline

    id=$(docker create "$HARNESS_IMAGE" none); docker cp "$id:/harness" .e2e/harness; docker rm "$id"
    docker run --rm -i -v "$PWD/.e2e/harness":/harness:ro python:3.12-alpine \
        python /harness/harness.py compose <e2e/plugins.json >.e2e/harness/plugins.yaml
    export MERIDIAN_RUNTIME_IMAGE=$RUNTIME_IMAGE
    H="docker compose -p <plugin>-e2e -f .e2e/harness/compose.yaml -f .e2e/harness/plugins.yaml"
    $H down -v --remove-orphans
    $H up -d
    $H run --rm -T runner ready
    $H run --rm -T runner settings <name>=<value>
    $H run --rm -T runner form --level admin --page <its link page> --post <its link action> <fields> --expect "<what it says>"
    # poll `store street` until the statement you expect is `complete`, not merely present
    $H run --rm -T store street > .e2e/street
    diff -u e2e/expected.street .e2e/street
    $H run --rm -T runner unlinked --expect <n>
    $H logs --no-color > .e2e/components.log   # on a failure, before tearing down
    $H down -v --remove-orphans

A setting changed and a link made each wake a plugin that reads on them, so wait
for a complete statement rather than for any row, or the diff can catch one
half recorded. core's `harness-check` target in its `Makefile` is a working
example, with three plugins.
