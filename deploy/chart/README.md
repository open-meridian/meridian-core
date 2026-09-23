# meridian-runtime

A Meridian deployment, for a Kubernetes
cluster. On a laptop or at a provider; the chart does not care which.

If you do not have a cluster, use the compose file in the repository root
instead. It runs the same image.

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

The database itself is still yours to run: the chart connects to one and never
provisions one, because a database a chart owns is a database a
`helm uninstall` can take with it. If you are bringing up a development cluster
and want one to point at, `deploy/local/postgres.yaml` is a single Postgres on
a claim -- read the file before applying it, it says what it is and what it is
not for.

## Installing

```bash
helm install meridian oci://ghcr.io/open-meridian/charts/meridian-runtime \
  --set deployment.id=DEP-... \
  --set deployment.enrolmentCode=ENROL-...
```

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

## The dashboard, and a bundled Zitadel

The dashboard is on, because a deployment with no dashboard has no way to be
set up: the wizard is what a fresh install serves. The address your staff reach
it at is one of the things the wizard asks for, so it is not a value you set.
It signs people in through a directory over OpenID Connect: point
`dashboard.oidc` at the one your firm already runs.

A firm with no directory, or one whose directory speaks only SAML or LDAP, can
have the chart run Zitadel beside the deployment instead, with
`identity.bundled.enabled=true`. It is Zitadel's own chart, vendored under
`charts/` at 10.0.6, plus a group hook that carries directory groups into the
dashboard's token and a setup Job that configures Zitadel for the dashboard.
It needs four things from you:

1. **Its version**, in both `zitadel.image.tag` and `zitadel.login.image.tag`.
   The chart has no default and never changes it: a Meridian upgrade that needs
   a newer Zitadel refuses to render and names the version, rather than
   upgrading it.
2. **Its public host**, `zitadel.zitadel.configmapConfig.ExternalDomain`. The
   dashboard's issuer is `https://` that host.
3. **Its own database and login role**, which you make. The connection string
   goes into `meridian-zitadel-database` under `dsn`, and on a fresh install
   the wizard asks for it and writes it there, so it is not a secret you create
   by hand either. Zitadel's init job makes only its schema, as that role, so
   it never holds your Postgres superuser.
4. **Where it may connect**, `identity.bundled.egress.allowCidrs`: its database,
   and your LDAP directory if any. A NetworkPolicy allows those, the group hook
   and DNS, and nothing else -- which is what makes it safe that Zitadel's
   webhook deny list is narrowed so it can reach the hook.

The chart makes Zitadel's master key once, in `meridian-zitadel-masterkey`, and
never regenerates it; it survives `helm uninstall`. **Back it up apart from the
database**: without it, the directory credentials and signing keys in Zitadel's
database cannot be read.

The setup Job runs after every install and upgrade. It alone holds Zitadel's
admin token, and it may read and update two Secrets -- the dashboard's client,
and the hook's signing keys -- and nothing else. It is idempotent: a second run
changes nothing.

### Upgrading Zitadel

Only when you choose to, by changing both tags:

1. Back up Zitadel's database. Its schema step is one-way; going back means
   restoring this backup.
2. Set `zitadel.image.tag` and `zitadel.login.image.tag` to the new version,
   and `helm upgrade`.

Its database, master key, people, groups and directory connections carry over,
and nothing else restarts. People already signed in to the dashboard stay signed
in, because sessions live in the dashboard; only new sign-ins wait while
Zitadel rolls.

Zitadel runs with its own chart's security context, which pins uid 1000: its
image names its user rather than numbering it, so that is how Kubernetes knows
it is not root. On OpenShift, set `runAsUser` and `fsGroup` to null under
`zitadel.podSecurityContext` and `zitadel.securityContext`; the platform assigns
the uid instead. Meridian's own templates pin none.

Zitadel's chart also uses `alpine/k8s` (to write its admin token into a Secret)
and `wait4x` (to wait for the database), at the versions its own chart pins.
