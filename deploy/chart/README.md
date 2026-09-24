# meridian-runtime

A Meridian deployment, for a Kubernetes
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

```bash
helm install meridian oci://ghcr.io/open-meridian/charts/meridian-runtime \
  --set deployment.id=DEP-... \
  --set deployment.enrolmentCode=ENROL-...
```

You do not tell it where the platform is. `platform.address` is empty by
default and the chart resolves it, so nothing ordinary carries that address
around. Set it only to reach a different one -- a staging platform, while
somebody is testing a change to the platform itself.

Every published chart pins the image built from the same commit, so a chart and
the runtime it runs are one build. Installing from a checkout instead
(`./deploy/chart`) uses `image.tag: latest`, which moves -- pin it to a commit
in anything you care about.

Then open the wizard. It is not given a public address and should not have one,
so reach it through a forward:

```bash
kubectl port-forward svc/meridian-meridian-runtime-dashboard 8443:80
```

and go to `http://127.0.0.1:8443/first-run`. It asks for a first-run code,
which a deployment administrator issues on the platform, and then for the
database, the directory and the addresses. Nothing is written until you apply.

Where you have a terminal,
[meridian-cli](https://github.com/open-meridian/meridian-cli) does that order
for you -- `meridian up --id DEP-...` -- and can answer the wizard from a file
so a teardown and relaunch is scriptable. No install depends on it.

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
