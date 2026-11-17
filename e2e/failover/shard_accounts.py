"""An instance of several pods' own account, asked of the API server (step
3d, checkpoint 5, item 31): `<fullname>-shards-<instance>`, a ServiceAccount
and a Role and RoleBinding of its name, the Role granting `get` and `update`
on exactly that instance's shards' Leases.

Each check answers (held, what), for run.py's `s.check`, and makes nothing
it does not remove: what the account may do is asked with `kubectl auth
can-i`, and what the launcher's admission policy admits with server-side
dry runs, apart from one Role the binding checks need to exist, made as the
launcher and removed after.

`kubectl` is a callable as run.py's: kubectl(*args, input=None, check=True,
quiet=False) -> CompletedProcess, run in the release's namespace.
"""

from __future__ import annotations

import json

# The launcher's admission policy's words (PO item 56, 2026-11-16).
ROLE = "the launcher's Role grants get and update on its instance's shard Leases alone"
BINDING = "the launcher's RoleBinding binds its instance's shards Role to the same-named account alone"
ACCOUNT = "the launcher's ServiceAccount carries no secrets"
DEPLOYMENT = "a launched plugin runs under no account, its instance's shards account or the archive's"
TOKEN = "under its instance's shards account the token is mounted in the sidecar's container alone"

# An instance nothing runs as, for what the policy is asked to make.
PROOF = "account-proof"


def account_of(full: str, instance: str) -> str:
    return f"{full}-shards-{instance}"


def can_i(kubectl, namespace: str, account: str, verb: str, resource: str) -> bool:
    done = kubectl("auth", "can-i", verb, resource, f"--as=system:serviceaccount:{namespace}:{account}",
                   check=False, quiet=True)
    return done.stdout.decode(errors="replace").strip() == "yes"


def rights(kubectl, namespace: str, full: str, instance: str, shards: int, other: str, store: str):
    """What the instance's own account may do: get and update each of its
    shards' Leases, and nothing else -- not another instance's, not a
    store's, not one past its count, no create and no delete."""
    account = account_of(full, instance)
    held = []
    for shard in range(shards):
        lease = f"leases/{instance}-shard-{shard}"
        allowed = [verb for verb in ("get", "update") if can_i(kubectl, namespace, account, verb, lease)]
        held.append((allowed == ["get", "update"],
                     f"accounts: {account} may get and update {lease} (it may {allowed})"))
    for verb, resource, what in (
            ("update", f"leases/{other}-shard-0", "another instance's Lease"),
            ("get", f"leases/{other}-shard-0", "another instance's Lease"),
            ("update", f"leases/{store}", "a store's Lease"),
            ("update", f"leases/{instance}-shard-{shards}", "a Lease past its count"),
            ("create", "leases", "a Lease of any name"),
            ("delete", f"leases/{instance}-shard-0", "its own Lease"),
            ("list", "leases", "the namespace's Leases"),
            ("get", "secrets", "a Secret")):
        held.append((not can_i(kubectl, namespace, account, verb, resource),
                     f"accounts: {account} may not {verb} {what} ({resource})"))
    return held


def role_names(kubectl, full: str, instance: str) -> list[str]:
    done = kubectl("get", "role", account_of(full, instance), "-o", "json", check=False, quiet=True)
    if done.returncode != 0:
        return []
    rules = json.loads(done.stdout).get("rules", [])
    return sorted(name for rule in rules for name in rule.get("resourceNames", []))


def _objects(namespace: str, full: str, instance: str) -> dict:
    own = account_of(full, instance)
    labels = {"meridian.dev/launched": "true", "meridian.dev/instance": instance}
    return {
        "role": {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
                 "metadata": {"name": own, "namespace": namespace, "labels": labels},
                 "rules": [{"apiGroups": ["coordination.k8s.io"], "resources": ["leases"],
                            "verbs": ["get", "update"],
                            "resourceNames": [f"{instance}-shard-0", f"{instance}-shard-1"]}]},
        "binding": {"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
                    "metadata": {"name": own, "namespace": namespace, "labels": labels},
                    "subjects": [{"kind": "ServiceAccount", "name": own, "namespace": namespace}],
                    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": own}},
        "account": {"apiVersion": "v1", "kind": "ServiceAccount",
                    "metadata": {"name": own, "namespace": namespace, "labels": labels},
                    "automountServiceAccountToken": False},
    }


def admission(kubectl, namespace: str, full: str):
    """The launcher's admission policy holds what it makes for an instance
    to exactly the shape it makes: each shape admitted as the launcher
    makes it, and refused, in the policy's own words, a Role wider than its
    one rule, a Role naming another instance's Lease, a binding to another
    account, and an account carrying a secret."""
    launcher = f"system:serviceaccount:{namespace}:{full}-launcher"

    def asked(made, dry=True):
        done = kubectl("create", *(["--dry-run=server"] if dry else []), f"--as={launcher}", "-f", "-",
                       input=json.dumps(made).encode(), check=False, quiet=True)
        return done.returncode == 0, (done.stderr or b"").decode(errors="replace").strip()[-400:]

    shapes = _objects(namespace, full, PROOF)
    held = []
    # The Role made, as the launcher makes it: the binding's checks need it
    # to exist (RBAC asks that its binder hold what it grants).
    made, why = asked(shapes["role"], dry=False)
    held.append((made, f"accounts: the launcher may make an instance's own Role as it shapes it ({why})"))
    try:
        for what, shape in (("binding", shapes["binding"]), ("account", shapes["account"])):
            admitted, why = asked(shape)
            held.append((admitted, f"accounts: the launcher may make an instance's own {what} as it shapes it ({why})"))

        def changed(key, change):
            shape = json.loads(json.dumps(shapes[key]))
            change(shape)
            return shape

        for what, shape, says in (
                ("a Role wider than get and update",
                 changed("role", lambda r: r["rules"][0]["verbs"].append("create")), ROLE),
                ("a Role of two rules",
                 changed("role", lambda r: r["rules"].append({"apiGroups": ["coordination.k8s.io"],
                                                              "resources": ["leases"], "verbs": ["get"]})),
                 ROLE),
                ("a Role naming another instance's Lease",
                 changed("role", lambda r: r["rules"][0]["resourceNames"].append("another-shard-0")), ROLE),
                ("a binding to another account",
                 changed("binding", lambda b: b["subjects"][0].__setitem__("name", f"{full}-conductor")), BINDING),
                ("an account carrying a secret",
                 changed("account", lambda a: a.__setitem__("secrets", [{"name": f"{full}-broker"}])), ACCOUNT)):
            # A Role of the same name exists: asked under a fresh instance's
            # name where the shape is a Role, so the answer is the policy's,
            # not the store's.
            if shape["kind"] == "Role":
                fresh = PROOF + "-b"
                text = json.dumps(shape).replace(account_of(full, PROOF), account_of(full, fresh))
                text = text.replace(f'"{PROOF}-shard-', f'"{fresh}-shard-').replace(
                    f'"meridian.dev/instance": "{PROOF}"', f'"meridian.dev/instance": "{fresh}"')
                shape = json.loads(text)
            admitted, why = asked(shape)
            held.append((not admitted and says in why,
                         f"accounts: the launcher may not make {what}, in the policy's words ({why})"))
    finally:
        kubectl("delete", "role", account_of(full, PROOF), "--ignore-not-found", check=False, quiet=True)
    return held


def deployments(kubectl, namespace: str, full: str, other: str):
    """The launcher's admission policy holds a launched Deployment of
    several pods to its own instance's account: admitted under it, with the
    token in the sidecar's container alone; refused, in the policy's words,
    under another instance's own account or the members' (the stores'
    alone), and with the token in the plugin's container."""
    launcher = f"system:serviceaccount:{namespace}:{full}-launcher"
    labels = {"meridian.dev/launched": "true", "meridian.dev/instance": PROOF}
    token = {"name": "member-token", "projected": {"sources": [{"serviceAccountToken": {"path": "token"}}]}}
    base = {"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": f"{full}-plugin-{PROOF}", "namespace": namespace, "labels": labels},
            "spec": {"selector": {"matchLabels": labels},
                     "template": {"metadata": {"labels": labels}, "spec": {
                         "serviceAccountName": account_of(full, PROOF),
                         "automountServiceAccountToken": False,
                         "containers": [
                             {"name": "sidecar", "image": "x", "volumeMounts": [
                                 {"name": "member-token", "mountPath": "/var/run/secrets/kubernetes.io/serviceaccount"}]},
                             {"name": "plugin", "image": "x"}],
                         "volumes": [token]}}}}

    def asked(change):
        made = json.loads(json.dumps(base))
        change(made["spec"]["template"]["spec"])
        done = kubectl("create", "--dry-run=server", f"--as={launcher}", "-f", "-",
                       input=json.dumps(made).encode(), check=False, quiet=True)
        return done.returncode == 0, (done.stderr or b"").decode(errors="replace").strip()[-400:]

    held = []
    admitted, why = asked(lambda pod: None)
    held.append((admitted, f"accounts: the launcher may run an instance of several pods under its own account ({why})"))
    for what, change, says in (
            ("under another instance's own account",
             lambda pod: pod.__setitem__("serviceAccountName", account_of(full, other)), DEPLOYMENT),
            ("under the members' account, the stores' alone",
             lambda pod: pod.__setitem__("serviceAccountName", f"{full}-member"), DEPLOYMENT),
            ("with the token in its plugin's container",
             lambda pod: pod["containers"][1].__setitem__("volumeMounts", [
                 {"name": "member-token", "mountPath": "/tmp/token"}]), TOKEN)):
        admitted, why = asked(change)
        held.append((not admitted and says in why,
                     f"accounts: the launcher may not run one {what}, in the policy's words ({why})"))
    return held
