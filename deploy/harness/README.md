# The plugin harness

The smallest deployment a plugin can prove itself against: a released runtime
image's own components, a broker and a database, any number of plugins each
beside its own sidecar, a runner that does what a person does in the
dashboard, and a `store` service that prints what the stores kept. A plugin's
own e2e runs it against the runtime it pins; core's `make harness-check` runs
it at every commit with three of core's stand-in plugins, so a change here
that would break a plugin's e2e breaks core's gate first.

**It is a development deployment, for a test, and never a way to run one.** It
has no platform, and a deployment's records are its own (contract v10), so every
row a plugin records names a record the deployment minted, completed by the
runner as a deployment admin does at the dashboard's Instruments page; its dashboard runs with `MERIDIAN_DEVELOPMENT`,
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
| `harness.py` | The runner, standard library only: `ready`, `settings`, `hold`, `archive`, `summary`, `account`, `page`, `form`, `unlinked`, `grant`, `instruments`, `instrument`, `mcp`, `ticket`, `inbox`; and `compose`, which writes the plugins' half of the deployment |
| `street.sql` | The street store as stable, sorted lines, which `store street` prints |
| `book.sql` | The book of record as stable, sorted lines (contract v8), which `store book` prints |
| `tickets.sql` | The dashboard's tickets, the records they name and their notes, as stable, sorted lines (contract v13), which `store tickets` prints; never a text |
| `activity.sql` | The custodian's activity and each connection's latest sync status, as the street keeps them, as stable, sorted lines (contract v14), which `store activity` prints |
| `moves.sql` | An edge plugin's moves of its raw records, and the holds and archives a deployment admin set, each its own record, as stable, sorted lines (contract v16), which `store moves` prints; never a record's content |

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

A plugin holding an edge role (`ccm`, `custody`, `dgm`, `match`, `reporting`,
`servicing` or `settlement`) is also given its storage, as a deployment gives
it (decisions/028): a volume of its own, `storage-<instance>`, at the path
`MERIDIAN_STORAGE_DIR` names (`/var/lib/meridian/storage`), writable by
whichever user its image runs as. It outlives the plugin's restarts and a
container made again (`up -d --force-recreate <instance>`), which is how a
plugin's e2e proves it rebuilds from what it kept, and goes with `down -v`.
No other plugin mounts it, and a plugin holding no edge role has none.

From contract v16, an edge plugin listed with `"archive": true` is also given
the harness's archive (spec/an-edge-plugins-older-records-move-to-the-archive),
as the launcher gives an instance a deployment admin allowed one: a volume of
its own, `archive-<instance>`, at the path `MERIDIAN_ARCHIVE_DIR` names
(`/var/lib/meridian/archive`), beside its storage and going with `down -v`. The
harness has no launcher, so the volume is there from the start; `archive
allow` then records on the plugin's Manage page what a deployment admin
allowed, which past-the-window `archived` needs. The harness's archive is a
local one, which cannot lock: a write-once hold is refused here, as on any
local deployment.

    [{"instance": "snaptrade", "image": "...", "roles": ["custody"], "archive": true}]

- `instance` is lower case letters, digits and inner hyphens, at most 32,
  starting with a letter, and none of the names the harness already holds
  (its services, `storage` among them, `runtime`, `first-run`, `dashboard-1`,
  or `sidecar-...`).
- `roles` is a list, empty for a plugin holding none.
- `archive`, where given, is `true`, and only for a plugin holding an edge role.
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

A run that needs more than one person names them in `MERIDIAN_HARNESS_PEOPLE`
-- `ada=Ada Park,ben=Ben Ito`, a name and how the deployment shows it, comma
separated -- passed to every compose command, as `MERIDIAN_RUNTIME_IMAGE` is.
`keys` draws a password and its hash for each, and the `people` service makes
an account the dashboard holds for each from the hash, before the dashboard
starts. Any runner command then acts as one of them with `--as NAME`; they
hold nothing until `grant --to NAME` grants it. A run naming nobody has the
admin alone, as before.

## The runner

    $H run --rm -T runner <command>

Each command signs in, does one thing, prints what it found and exits 0, or
exits non-zero saying why. Every wait is bounded by `--seconds`.

| Command | What it does |
|---|---|
| `ready [--seconds N]` | Waits until the plugin has registered and the dashboard lists it, healthy or not. The first command of a run. |
| `settings NAME=VALUE ... [--seconds N] [--expect TEXT]` | Sets the plugin's settings in its settings form, once it has declared each; a secret in its secret field. A developer setting is accepted here. A table setting's cell is `NAME[ROW].COLUMN=VALUE`, and the rows given are its whole (contract v14). `--expect` reads the Settings tab after saving and fails unless it says TEXT, printing who last changed them |
| `hold DAYS [--role ROLE] [--write-once] [--expect-refused TEXT]` | From contract v16: sets the hold on raw records on the deployment's Settings, as a deployment admin does, for one edge role or every one; 0 days clears it. With `--expect-refused`, fails unless it is refused saying `TEXT`. `settings ... --expect-refused TEXT` likewise expects a window below the hold refused, naming the setting. |
| `archive allow [--bound-gib N]` / `archive withdraw` | From contract v16: allows the plugin an archive, with a bound in GiB or none, or withdraws it, on its Manage page, and prints what its Summary then says. |
| `summary [--until TEXT] [--seconds N]` | Prints the plugin's Summary under Manage, where from contract v16 its raw records are drawn: per kind what its storage and its archive hold, and its moves; with `--until`, again until it says `TEXT`. |
| `account NAME [--seconds N]` | Defines an account and prints its ID, for a page that links only to an account that exists. |
| `page --level admin\|write\|read PATH [--until TEXT] [--seconds N]` | Opens a session on the plugin's own host at that level (Manage, Open, View), GETs `PATH`, prints the status and then the body; with `--until`, again until the body says `TEXT`. |
| `form --level L --page PATH --post PATH [--csrf-field NAME] [--from-page NAME ...] [--expect TEXT] FIELD=VALUE ...` | In such a session, reads `--page`, takes its CSRF field (`csrf`, the SDK's name) and each field `--from-page` names (repeated, or comma separated) with the value the page gives it -- a proposal's digest, say -- and posts them with the fields you give, urlencoded, to `--post`; prints the status and the body. A field you give wins over one taken from the page; a field the page does not have fails. With `--expect`, fails unless the body says `TEXT`. A 4xx or 5xx fails. |
| `unlinked [--expect N] [--seconds N]` | Prints how many external accounts the plugin reported that nothing links, as the dashboard counts them; with `--expect`, waits for `N`. |
| `instruments [--expect N] [--seconds N]` | Prints each record the dashboard's Instruments page lists as one the book cannot use, its ID and identifiers; with `--expect`, waits until it lists `N`. |
| `instrument ID\|--identifier TEXT asset_class=CLASS currency=CODE source=TEXT [description=TEXT] [instrument_type=money_market_fund fund_category=government\|prime\|tax_exempt fund_investors=retail\|institutional fund_nav=stable\|floating fund_liquidity_fee=mandatory\|discretionary] [note=TEXT]` | Completes a record at the Instruments page as the deployment admin does, each value with its source: its asset class and currency, and its description; and from contract v11 its instrument type and, for a money market fund, its four attributes, stated together. |
| `mcp connect [--covers everything \| --covers deployment_admin] [--covers INSTANCE[:ROLE]:LEVEL ...] [--client NAME]` | From contract v12: connects an MCP client as the admin, as an agent's client does: registers it, takes her through the authorisation for the `/mcp` resource with a PKCE challenge, signs her in afresh, consents to only what `--covers` names on All accounts -- from contract v15 a row per plugin role -- or with `--covers everything` to all she holds now and is granted later, and keeps the token pair in the runner's state for the run. |
| `mcp list [--expect NAME ...]` | Prints the tools the delegation reaches, one per line. |
| `mcp call NAME [JSON] [--from SAVED[.PATH]] [--set PATH=VALUE ...] [--save SAVED] [--expect-outcome OUTCOME] [--expect TEXT]` | Calls a tool and prints its typed answer as JSON; `--from` starts from what an earlier call saved, and each `--set` changes one field by the dictionary's path grammar, `[key=value]` picking a row by a field of its own. Refreshes the pair when the access token has lapsed. |
| `mcp complete --identifier TEXT asset_class=CLASS currency=CODE source=TEXT note=TEXT [instrument_type=... fund_...=...]` | Completes the record listed with that identifier through core's tools, against the version listed. |
| `mcp calls [--expect N]` | Prints how many calls Connected clients lists for the admin; with `--expect`, fails below `N`. |
| `grant --level read\|write\|admin [--role ROLE] [--to NAME] [--accounts ID,...]` | Grants the harness's admin that level on the plugin -- from contract v15 on one of its roles, `--role` naming it, else the plugin's one role or the plugin as a whole -- on All accounts, as a deployment admin does: a user group holding the admin, an access group, and the permission joining them. The admin holds nothing on a plugin until granted; a session at `write` (Open) needs write. With `--to`, one of the run's people instead; with `--accounts`, on an account group of those accounts alone. |
| `ticket file [--concerns PART] title=TEXT [seen=TEXT] [kind=KIND] [--expect-refused PATH]` | From contract v13: files a ticket as a person presses "Report a problem": about the plugin, or a part of core with `--concerns`. Prints its ID; with `--expect-refused`, fails unless refused naming `PATH`. |
| `ticket list [--concerns X] [--state S] [--expect N] [--seconds N]` | Prints the tickets the person may see: ID, state and title, a held title withheld. |
| `ticket read ID [--page] [--expect TEXT ...] [--expect-status N]` | Prints a ticket as its row answers it, a held text withheld; with `--page`, its page, where a person reads it. |
| `ticket note ID TEXT` | Adds a note on the ticket's page. |
| `ticket work ID act=ACT [owner=NAME] [due=DATE] [resolution=R] [cites=C] [release=ticket\|N] [--expect-status N]` | Takes one act on the ticket's page: assign, due, resolve, close, reopen or release. |
| `inbox [--expect N] [--expect-kind KIND ...]` | Prints the person's notices new since their pages last read them: the ticket and the change's kind. |

Every command acts on the first plugin listed, or on another with `--instance
<instance>`; as the admin, or with `--as NAME` as one of the run's people.

A link is the plugin's to send, acting for an admin, so the harness never sends
one: `form` drives the plugin's own link page under Manage, which is also the
proof that the page links.

## The stores

    $H run --rm -T store street
    $H run --rm -T store book
    $H run --rm -T store tickets
    $H run --rm -T store activity
    $H run --rm -T store moves

`store moves` (contract v16) prints each move of raw records the conductor
recorded, and each hold and archive change, one line each, sorted, never a
record's content:

    move|<instance>|<kind>|<unit>|<archived|restored|returned|deleted>|<records>|<first received ns>|<last received ns>|<rule>|<person>
    hold|<role, empty for every one>|<days>|<write-once>|<set by>
    archive|<instance>|<allowed>|<most bytes>|<set by>

`store tickets` (contract v13) prints the dashboard's tickets, one line each,
and the records each names and its notes, sorted, and never a text:

    ticket|<ID>|<concerns kind>|<concerns instance>|<filed provenance>|<filed person>|<filed instance>|<state>|<owner>|<due>|<seen count>|<suspect>|<matched rules>
    reference|<ID>|<position>|<kind>|<value>|<account>|<found in its text>
    note|<ID>|<number>|<kind>|<author provenance>|<author person>|<author client>|<suspect>


prints that store as stable, sorted lines, so a file from one run compares
with the next. Whoever reads a store needs no database user, password, file or
query of their own.

`store` prints the kernel's stores and never a plugin's storage: what an edge
plugin keeps there is its own, read by no one else (decisions/028), so a
plugin's e2e reads its raw records through the plugin, on its own page.

### The street store

`store street` prints every account's rows, ordered bytewise:

    assumed|<account>|<instrument>|<side>
    position|<account>|<instrument>|<side>|<quantity>|<settle-date quantity>|<market value> <currency>|<in-cash>
    statement|<account>|<source>|<expected rows>|<complete or open>|<buying power>|<margin requirement>|<maintenance excess>|<currency assumed>
    pending|<account>|<instrument>|<side>|<value date>|<quantity>
    closed|<account>|<instrument>|<side>|<field>|<kind>
    amended|<account>|<instrument>|<side>|<contract version>|<field>|<raw record's key>

- `<account>` is the account's name, since its ID is minted per run.
- `<instrument>` is an `INS-` ID, or for a record the deployment minted (its
  `LCL-` ID is drawn per run) its identifiers in force, sorted:
  `local(figi:BBG000B9XRY4, symbol:AAPL@snaptrade)`. In the harness every record
  is one the deployment minted, so an expected file names each row by the
  identifiers the plugin sent. (`placeholder(...)` until contract v10.)
- A number is printed at the scale it was stated with; what was not reported is
  empty, never zero. `<in-cash>` is `in-cash` for a position the venue also
  counts in cash.
- `assumed` is a row of a latest statement whose currency the connector assumed
  (a plugin before contract v11; from v11 the currency's provenance says it).
- From contract v11: `pending` is a position's quantity not yet settled, by its
  value date; `closed` a value of a position the custody plugin closed rather
  than read, by its path in the row (`quantity`, `settle_date_quantity`,
  `market_value.currency_code`), with its provenance's kind (`derived`,
  `supplied`, `second-source`, `reported`); each from the row that last stated
  the position, with what a backfill added to it. `amended` is a backfill
  journaled beside a row of a latest statement: the version and field that are
  its cause, and the key of the raw record it was re-converted from. A raw
  record's key otherwise stays off these lines: a plugin's own keys name its
  reads, which change from run to run.
- `statement` is the latest statement of each source for each account, by its
  rows' account. A statement none of whose rows was recorded (its account
  unlinked) belongs to no account and is not printed.

An account nothing links has no rows, so a file listing every account proves
both what a linked account holds and that an unlinked one holds nothing. Pair
it with `unlinked`, which proves the plugin reported the unlinked ones.

### The custodian's activity

`store activity` (contract v14) prints the activity a custody plugin
recorded and each connection's latest sync status, ordered bytewise:

    activity|<account>|<source>|<external activity id>|<kind>|<instrument>|<trade date>|<units>
    sync|<account>|<source>|<external account>|<state>|<history from>
    re-resolution|<account>|<source>|<external activity id>|<n>|<instrument>

- `<account>` and `<instrument>` as the street store prints them; a sync
  status of an external account nothing links has an empty `<account>`, and
  an activity whose instrument did not resolve an empty `<instrument>`.
- `<kind>` (ActivityKind) and `<state>` (SyncState) are their numbers, 0 for
  not known. `<units>` is printed at the scale it was stated with, empty
  where none was stated.
- An activity is one line however often the plugin sent it; a `sync` line is
  the latest the street heard for that connection, and `<history from>` the
  first date the source said it can read history from, empty where it said
  none.
- A `re-resolution` line (contract v15) is one of an activity's later
  resolutions, kept beside it while its `activity` line stays as first
  recorded: `<n>` its place among the activity's re-resolutions, 1 the
  first, and `<instrument>` empty where it is unresolved again.

Its own store rather than lines of `store street`, so a plugin's expected
street file is not changed by a revision it has not taken up.

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
