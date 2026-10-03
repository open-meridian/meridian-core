# meridian-runtime

An Open Meridian deployment, for a Kubernetes
cluster. On a laptop or at a provider; the chart does not care which.

If you do not have a cluster, use the compose file in the repository root
instead. It runs the same image.

This is reference, organised by topic. To install one for the first time,
follow [INSTALL.md](../../INSTALL.md) instead: same ground, in the order you
do it.

## Before installing

Two things, both from the platform: **this deployment's identifier** and a
**one-time enrolment code**. Register the deployment through the site or with
whoever administers your organisation, and you are given both.

Nothing else is prepared by hand. There is no key to generate and no secret to
create:

- **The key is the deployment's own.** The conductor generates it inside the
  cluster and registers the public half with the enrolment code, which is then
  spent. Nobody handles either half, and no private key passes through a laptop,
  a shell history or a backup on its way in.
- **The database is the wizard's.** A fresh install serves a set-up wizard, and
  what you tell it there is written into the cluster by a Job that gives its
  rights up afterwards. It tests both database roles before it writes anything
  and names the statement that fixes what it finds.

The wizard asks about the database once, for the whole deployment, and takes
one of two answers:

- **A database you already run** -- your own Postgres, one in Docker, a
  managed one from your cloud. It needs two roles: the migrating role may
  create a table and the serving role must not. Both are tested before
  anything is written. This is what anything you depend on should use.
- **One started inside this cluster.** Nothing is asked of you: the chart
  ships a Postgres at zero replicas, and choosing this starts it and makes
  the databases, the roles and the grants. For trying the product and for
  development. It keeps its data if Meridian is removed and installed again,
  it loses everything if the cluster is deleted, and nobody backs it up.

## Installing

Install it with the command line; [INSTALL.md](../../INSTALL.md) walks the
whole path:

```bash
export MERIDIAN_ENROLMENT_CODE=ENR-...
meridian up --id DEP-...
```

`meridian up` checks the machine and the cluster first (`meridian doctor`),
installs this chart, waits for the dashboard and prints the wizard's address.
`-f <file>` passes any of the values below; `--params <file>` answers the
wizard from a file, so a teardown and relaunch is scriptable.

You do not tell it where the platform is. `platform.address` is empty by
default and the chart resolves it, so nothing ordinary carries that address
around. Set it only to reach a different one -- a staging platform, while
somebody is testing a change to the platform itself.

Every published chart pins the image built from the same commit, so a chart and
the runtime it runs are one build. `meridian up --chart ./deploy/chart`
installs from a checkout instead, with `image.tag: latest`, which moves -- pin
it with `--image` in anything you care about.

The wizard asks for a first-run code, which a deployment administrator issues
on the platform, and then for the database, how people sign in, and the
addresses. Nothing is written until you apply.

## What it is configured with, and what it is not

One platform address, this deployment's identifier, its key and a database. That
is the whole list, and the omissions are deliberate: no region, no instance
list, no failover order. The platform is allowed to grow, move and be redirected
without anything here changing or restarting, which is only true while nothing
here describes its shape.

## The dashboard, and how people sign in

The dashboard is on, because a deployment with no dashboard has no way to be
set up: the wizard is what a fresh install serves. The address your staff
reach it at is one of the things the wizard asks for, so it is not a value you
set.

This deployment runs no identity server (decision 018). It signs people in one
of three ways, chosen in the wizard rather than here:

- **Your OpenID Connect provider.** The dashboard federates to it, and nothing
  of ours authenticates anybody. Your provider **must return `auth_time`** --
  Entra ID sends it only if the app registration asks for it, and a provider
  that cannot is not usable. See the install guide.
- **Your LDAP.** The dashboard binds to it directly: it forwards a password
  once and stores none, and reads the person's groups from the directory.
- **Neither.** The deployment holds the accounts itself, with hashed passwords
  and a lockout, for a firm with no directory of its own.

Nothing is configured here for any of them. `dashboard.oidc` exists for an
administrator who would rather set a provider in values than in the wizard;
everything else the wizard writes into Secrets this chart names.

## Storage for plugins at the edge

A plugin holding an edge role -- `ccm`, `custody`, `dgm`, `match`,
`reporting`, `servicing` or `settlement` -- keeps an outside party's raw
records and its working state, and rebuilds from them after a restart
(decisions/028). Each such instance is given a volume of its own, a
PersistentVolumeClaim named `<release>-meridian-runtime-storage-<instance>`,
`pluginStorage.size` each from the default storage class or
`pluginStorage.storageClassName`, mounted in that plugin's container alone at
the path `MERIDIAN_STORAGE_DIR` names. No other plugin can mount it; where the
cluster has ValidatingAdmissionPolicy (Kubernetes 1.30 and later), a plugin's
pod asking for any storage but its own instance's is refused.

Stopping a plugin keeps its storage, and launching the same plugin as the same
instance mounts it again; launching a different plugin as that instance is
refused while it is kept. Nothing in the deployment deletes one -- not
stopping, not removing the plugin from `sidecars`, not uninstalling -- since a
firm may have to keep those records for years. Removing one is an
administrator's act, with `kubectl delete pvc`, until the dashboard has a
place for it. Encryption at rest, snapshots and backups are the storage
class's and the cluster's, as they are for the database; the claims carry
`meridian.dev/component: plugin-storage` for a backup policy to select.
`pluginStorage.enabled: false` gives no plugin storage.
