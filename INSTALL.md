# Installing a deployment

Follow this from top to bottom. It ends with a running Meridian deployment
that your firm's people sign in to, and it assumes you have not installed one
before.

If you have to stop and ask somebody a question, that is a defect in this page
rather than in you. Say which step, and what you had to ask.

**You will move between two places.** The **platform** is the site at
<https://open-meridian.com>, where deployments are registered and codes are
issued. The **deployment** is what you install into your own cluster. They are
deliberately separate: the platform never holds your positions, and it never
holds a private key.

Budget half an hour.

## Before you start

- A Kubernetes cluster, and `kubectl` already pointing at it. A laptop cluster
  (k3s, Rancher Desktop, kind, Docker Desktop) is fine for trying this.
- `helm`, version 3.8 or newer. Older ones cannot install from a registry.
- An account on the platform, in an organisation, holding **owner** or
  **admin**. If you have neither, whoever owns the organisation can give you
  one; nothing else on this page will work without it.

You do **not** need to prepare a key, create a Secret, obtain a certificate, or
give the deployment a public address. The deployment makes its own key, and the
set-up wizard writes its own Secrets. If an older instruction told you to
generate a key, ignore it.

### The two decisions, made now

Both are easier to make before you install than after.

**Where the database lives.**

| | Use this when | What you are agreeing to |
|---|---|---|
| **One this deployment brings** | Trying the product, or developing against it | It is started in your cluster, and its roles and passwords are made for you. It survives Meridian being removed and installed again. It is lost with the cluster, and **nobody backs it up** |
| **One you already run** | Anything you depend on | Your own Postgres, one in Docker, or a managed one from your cloud. You create two roles first — see below — and both are tested before anything is written |

If you are pointing at a database you run, make it two roles before you go on.
The **migrating** role may create tables; the **serving** role must not. Two
roles rather than one because the deployment runs day to day as something that
could not alter its own schema if it were compromised:

```sql
CREATE DATABASE meridian;
CREATE ROLE meridian_migrate LOGIN PASSWORD 'a-password-you-choose';
CREATE ROLE meridian_app LOGIN PASSWORD 'another-password-you-choose';
GRANT ALL ON DATABASE meridian TO meridian_migrate;
```

The wizard tests both and names the statement that fixes what it finds, so you
do not have to get the grants exactly right here.

**How people sign in.** One of three, and unlike the database this one you
can leave until the wizard — nothing about it is passed at install.

| | |
|---|---|
| **Your OpenID Connect provider** | The deployment federates to it. Nothing of ours authenticates anybody. **It must return `auth_time`** — see step 7 |
| **Your LDAP** | The deployment binds to it directly: it forwards a password once, stores none, and reads each person's groups from your directory |
| **Neither** | The deployment holds the accounts itself, with hashed passwords and a lockout. For a firm with no directory of its own |

Nothing of Meridian's signs people in as a separate service, so there is no
identity server to size, version, back up or upgrade.

## 1. Register the deployment

On the platform: sign in, open your organisation, then **Deployments**, then
**Register deployment**. Give it a name you will recognise a year from now —
`production`, `uat` — and register it.

It now has an identifier, `DEP-` and some letters. **Copy it.** Every step
below wants it, and every assertion this deployment ever signs is about it.

## 2. Issue an enrolment code

On the deployment you just registered, open its menu and choose **Issue
enrolment code**.

The code is shown once, on that page, and lasts a day. Copy it now.

This is what the install carries instead of a key. The deployment generates its
own keypair inside your cluster, registers the public half using this code, and
the code is spent. Nobody handles a private key, and none passes through a
laptop, a shell history, or a backup on its way in.

Two things that will save you a confusing minute later: issuing a second code
expires the first, and a deployment that has already enrolled a key is refused
one altogether. A **retired** deployment is refused one too — return it to
service first.

## 3. Install the chart

Make a namespace and install. Substitute your identifier and your code:

```bash
kubectl create namespace meridian
```

```bash
helm install meridian oci://ghcr.io/open-meridian/charts/meridian-runtime \
  --namespace meridian \
  --set deployment.id=DEP-XXXX-XXXX \
  --set deployment.enrolmentCode=ENR-XXXX-XXXX-XXXX
```

That is the whole command, on every branch. How people sign in is answered in
the wizard, not here.

You do not tell it where the platform is. Leave `platform.address` alone unless
somebody has asked you to test against a staging platform.

Now wait for the pods:

```bash
kubectl --namespace meridian get pods --watch
```

**Some of them will sit in an error, and that is what a correct install looks
like at this point.** Expect this:

| | |
|---|---|
| `broker`, `conductor`, `dashboard`, `first-run` | reach **1/1**. These are what serve you the wizard |
| `key` | **Completed** — the deployment has made its own keypair |
| `street`, `instrument`, `migrate` | **`CreateContainerConfigError`**, and they stay there |

That last row is not a failure. The chart creates the database Secret empty,
for the wizard to fill, so those three report `couldn't find key url in Secret
…-database`. They cannot start without a database and there is not one yet.
They retry by themselves and come up a minute or two after you apply the
wizard; nothing needs restarting and nothing needs deleting.

The conductor and the dashboard treat that same key as optional, which is
exactly what lets a fresh install serve a wizard at all.

If you are watching this in a graphical cluster tool, it will report an error
count for those three. Ignore it until after step 7.


## 4. Check the deployment enrolled, and that the key is yours

The conductor enrols by itself while the pods come up. Confirm it:

```bash
kubectl --namespace meridian logs -l meridian.dev/component=conductor --tail=40
```

Back on the platform, the deployment now lists a key with a **fingerprint**.
You will compare it against the deployment's own in the next step. Do not skip
that comparison: it is the one check that tells you the key registered against
your deployment is the one your cluster made, rather than one somebody else
enrolled with a code that reached them.

## 5. Issue a first-run code

On the platform, on the same deployment: **Issue first-run code**.

Single use, one day. It is what opens the wizard. It is separate from the
enrolment code because enrolling is a machine proving what it is, and this is a
person proving they are allowed to set it up.

## 6. Open the wizard

The wizard is not given a public address and should not have one. Reach it
through a forward:

```bash
kubectl --namespace meridian port-forward svc/meridian-meridian-runtime-dashboard 8443:80
```

Leave that running, and open <http://127.0.0.1:8443/first-run>.

**Compare the fingerprints now.** The page shows this deployment's identifier
and its key's fingerprint before it asks for anything. It must match what the
platform showed in step 4. If it does not, somebody else spent your enrolment
code: revoke that key on the platform, issue a fresh code, and start again at
step 2.

Then enter the first-run code and continue.

## 7. Answer the wizard

Four sections. Nothing is written until you press **Apply**, and **Test** may
be pressed as often as you like.

**Database.** Choose the route you decided on. *Start one inside this cluster*
asks you for nothing further. *Use a database you already run* wants the host,
port, database name, TLS mode, and the two roles with their passwords.

**Signing in.** Choose one of the three, and answer only that part.

- *Our own OpenID Connect provider* asks for the issuer, the client id, the
  client secret (none for a public client) and the claim carrying groups. The
  issuer must be **exactly** what your provider calls itself, trailing slash
  and all: Test reads your provider's discovery document and says so if one
  character is off. Leave *other audiences* empty unless your provider puts
  something besides the client id in a token's audience — some add the
  project the client belongs to, and then every sign-in is refused until
  that is listed here.
- *Our LDAP or Active Directory* asks for the servers, where people are, the
  account this deployment searches as, and how a person is found (`{}` is the
  name they type; empty is `(uid={})`, and Active Directory usually wants
  `(sAMAccountName={})`). Tick **StartTLS** for an `ldap://` address, or every
  password crosses your network as it was typed; an `ldaps://` address is
  already encrypted. Test binds with that account and reads where people are.
- *We have no directory: make me an account* asks for the login, email, name
  and password of the one account this deployment then holds.

**If you are using your own provider, it must return `auth_time`.** This is
the one thing about your provider that Meridian requires and that not every
provider does by default. It is what makes withdrawn access actually go away:
a provider with its own session can hand out a token without asking the person
anything, carrying groups your directory has since removed, and `auth_time` is
the only thing that says when they really authenticated. Meridian asks for it
correctly, with `max_age=0` and `prompt=login`, which obliges a provider to
answer under the OpenID Connect specification.

Check yours before you go on, because the failure comes at the very end and
reads like a problem with your directory:

| Provider | What you need to do |
|---|---|
| **Microsoft Entra ID** | **Add `auth_time` as an optional claim** on the app registration — Token configuration, add optional claim, ID token, `auth_time`. Entra does not send it otherwise |
| **Okta** | Nothing. It is sent automatically when `prompt=login` or `max_age=0` is asked for, which Meridian does |
| **Ping** | Check. Their documentation does not say either way; sign in once and look at the token |
| **dex** | Not usable. It does not implement `auth_time`, and the request to add it has been open since 2017 |

If your provider cannot return it, stop here and talk to us rather than
working around it: the alternative is a deployment where somebody removed from
a group keeps their access until their session happens to end.

**Administrators.** Who runs this deployment once it is set up. With a
directory, name a group — its members hold deployment admin, and adding
somebody later is a change in your directory rather than here. With no
directory, the account you just described is the administrator and this is left
empty.

**Spell the group exactly.** It is not checked and cannot be: a directory
states a person's groups when they sign in, and it is not asked to list them.
A group that does not exist is a deployment nobody can administer, and getting back in then
means a claim code from the platform.

**Addresses.** Where a browser reaches this deployment. One address: nothing
of ours redirects a browser anywhere else, so there is no second host to
arrange. This is the address your staff will use, not the `127.0.0.1` forward
you are reading this through.

Press **Test**. It checks every answer the way Apply will: the database roles,
your provider or directory (by connecting to it, from inside the cluster), the
addresses and the administrators. It names what it finds. Fix what it names, test again, and when it is clean
press **Apply**.

Applying writes it all at once and restarts what changed. It takes a couple of
minutes. The Job that writes gives its rights up when it finishes.

## 8. Sign in

Stop the port-forward. Open the dashboard at the address you gave it, and sign
in through the directory you configured. The administrators you named hold
deployment admin from their first sign-in; nothing else needs redeeming, and
there is no code to type.

That is the install finished.

## When something is not right

| What you see | What it means | What to do |
|---|---|---|
| The wizard asks for an enrolment code, not a first-run code | The conductor could not enrol — usually a code already spent or expired | Issue a fresh enrolment code and enter it on that page. Nothing needs reinstalling |
| The fingerprints differ | Somebody else spent your enrolment code | Revoke that key on the platform, issue a new code, re-enrol. Do not continue |
| "this deployment is retired" | It was taken out of service on the platform | Return it to service there. Codes are refused while it is retired |
| "already enrolled" when issuing an enrolment code | It holds a key already | If the cluster still has it, register the new key signed in through the deployment's keys. If you deleted the namespace, see **Starting over** below |
| "this deployment is retired" | It was taken out of service on the platform | Return it to service there |
| The wizard's database test names a missing grant | Your roles need it | Run the statement it names, then test again |
| "the directory did not say when this person authenticated" | Your provider returned no `auth_time` | See the table in step 7. On Entra, add it as an optional claim on the app registration; nothing in the deployment needs changing |
| `street`, `instrument` or `migrate` still in error **after** applying | They may not have retried yet | Give them two minutes. If they persist, read the message: a key named there that is still missing means the apply did not write the database Secret |

## Starting over

If you delete the namespace to begin again — reasonable while you are learning
the shape of this — **the deployment's private key goes with it.** It lived
only in that cluster, which is the point of it: nobody ever handled it, and
there is no copy anywhere to restore.

The platform still holds the public half, so that deployment now refuses a new
enrolment code. That refusal is deliberate rather than an oversight: a code
that still worked after a deployment had enrolled would be a second way to
register a key for a live deployment, which is the thing worth stealing.

Two ways on, and the first is usually what you want:

1. **Revoke the key, then issue a fresh enrolment code.** On the deployment's
   page the key is listed with its fingerprint and a Revoke button. Once it
   holds no key, `Issue enrolment code` works again and you start from step 2
   with the same identifier. Keeping the identifier matters: it is the subject
   of every assertion that deployment ever signed.
2. **Register a new deployment**, and start from step 1. Simplest if you are
   only experimenting and do not care about the old identifier. Retire the old
   one so it stops appearing in the list.

Deleting the namespace also removes the database if this deployment brought
one, along with everything in it. Nothing warns you and nothing backs it up.

## What to back up

If you pointed at a database you run, back it up as you back up any other.

If the deployment brought its own database, there is nothing to back up and
nothing backing it up. That is the trade you accepted before you started; it is a fine
trade for a trial and the wrong one for anything you depend on.

## Removing it

```bash
helm uninstall meridian --namespace meridian
```

This removes what Helm installed. It does **not** remove the disk of a database
the deployment brought, because Helm never created that claim — Kubernetes
did, and Kubernetes keeps it. Installing again picks the same disk back up,
with its data. To be rid of it, delete the namespace.

Retiring the deployment on the platform is a separate act, and the one that
revokes its keys and stops its codes working. Uninstalling the chart leaves the
platform still believing the deployment exists.

---

For what each chart value does, see [deploy/chart/README.md](deploy/chart/README.md).
Where you have a terminal and would rather not click,
[meridian-cli](https://github.com/open-meridian/meridian-cli) performs steps 3
to 7 in that order and can answer the wizard from a file. No install depends
on it.
