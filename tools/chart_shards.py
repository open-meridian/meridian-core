#!/usr/bin/env python3
"""Hold the chart's renders to a plugin instance's shards and standby pods
(step 3d; the resilience spec's section 7; the review of 95d68904, S6).

chart-check greps a render for words. That cannot say which container a
mount sits in, or what a setting's value is, so these checks read the shape:

  default   the default profile: no members' ServiceAccount, Role or
            RoleBinding, and no shape for shards (standbys are
            configuration, ruling 1052);
  ha        the launcher's shards shape (plugin-shards.json): under the
            members' account, automountServiceAccountToken false by value,
            the token mounted in the sidecar's container alone and a plugin
            container beside it, the shard count told, and a rollout a
            shard at a time;
  sharded   a sidecars[] entry given shards and a standby (INSTANCE, PODS,
            SHARDS): the same, read from its own Deployment;
  trio      a sidecars[] entry's own account (INSTANCE, SHARDS; checkpoint
            5, item 31): a ServiceAccount, Role and RoleBinding named
            `<fullname>-shards-<instance>`, the Role's one rule `get` and
            `update` on exactly its shards' Leases, bound to that account
            alone, the account carrying no secret; each Lease rendered and
            kept; and the members' account, Role and binding -- the
            stores' alone -- never a plugin's.

  downward  every plugin container, the launcher's shapes and each
            sidecars[] Deployment's alike, told its namespace and account by
            the downward API: MERIDIAN_POD_NAMESPACE (metadata.namespace)
            and MERIDIAN_POD_SERVICE_ACCOUNT (spec.serviceAccountName)
            (PO item 64).

Usage:  helm template ... | python3 tools/chart_shards.py default
        helm template ... | python3 tools/chart_shards.py ha
        helm template ... | python3 tools/chart_shards.py sharded feed-1 3 2
        helm template ... | python3 tools/chart_shards.py trio feed-1 2
        helm template ... | python3 tools/chart_shards.py downward
Exit:   0 as it should be, 1 naming what is not
"""

from __future__ import annotations

import json
import re
import sys

TOKEN_PATH = "/var/run/secrets/kubernetes.io/serviceaccount"
MEMBER = re.compile(r"^  name: \S+-member$", re.M)
LAUNCHER = "check-meridian-runtime-launcher"
PODS = re.compile(r'^    resources: \["pods"\]$', re.M)
# The SDK's command an instance's own storage is copied with, in its own
# plugin image as its own user (PO items 86 and 88).
STORAGE_COPY = ["meridian-storage", "copy", "check-meridian-runtime-storage-__INSTANCE__"]
PODS_LISTED = re.compile(r'^    resources: \["pods"\]\n    verbs: \["list"\]$', re.M)
# Where the chart renders the copy pod (item 30), and only there, the one
# pod the launcher makes: made, read and removed (review c5c L8).
PODS_COPIED = re.compile(r'^    resources: \["pods"\]\n    verbs: \["list", "create", "get", "delete"\]$', re.M)


def documents(render: str) -> list[str]:
    return [doc for doc in re.split(r"^---\s*$", render, flags=re.M) if doc.strip()]


def kind(doc: str) -> str:
    found = re.search(r"^kind: (\S+)$", doc, re.M)
    return found.group(1) if found else ""


def default(render: str) -> list[str]:
    wrong = []
    for doc in documents(render):
        if kind(doc) in ("ServiceAccount", "Role", "RoleBinding") and MEMBER.search(doc):
            wrong.append(
                f"the default profile renders the members' {kind(doc)}: with no standby "
                "anywhere there is no lease right (ruling 1052)"
            )
    for word in ("plugin-shards.json", "MERIDIAN_LAUNCHER_SHARDS_TEMPLATE", "MERIDIAN_PLUGIN_SHARDS"):
        if word in render:
            wrong.append(f"the default profile renders {word}: standbys are configuration")
    if "plugin.json:" not in render:
        wrong.append("the default profile renders no launcher template to hold this check to")
    if any(PODS.search(doc) for doc in named(render, "Role", LAUNCHER)):
        wrong.append("the default profile gives the launcher a right on pods: it has no shards shape to stop")
    return wrong


def shape_wrong(pod: dict, strategy: dict, standby: bool, shards: str | None, instance: str) -> list[str]:
    """What is wrong with a several-pod instance's pod spec, read as JSON."""
    wrong = []
    account = pod.get("serviceAccountName", "")
    if not account.endswith(f"-shards-{instance}"):
        wrong.append(f"its account is {account!r}, not its instance's own, <fullname>-shards-{instance} (item 31)")
    if pod.get("automountServiceAccountToken") is not False:
        wrong.append(
            "automountServiceAccountToken is "
            f"{pod.get('automountServiceAccountToken')!r}, not false"
        )
    tokens = {
        volume["name"]
        for volume in pod.get("volumes", [])
        if any("serviceAccountToken" in source for source in volume.get("projected", {}).get("sources", []))
    }
    if not tokens:
        wrong.append("no volume carries the members' token")
    containers = pod.get("containers", []) + pod.get("initContainers", [])
    names = [container["name"] for container in containers]
    if "sidecar" not in names or "plugin" not in names:
        wrong.append(f"its containers are {names}; a sidecar and a plugin are held to this")
    for container in containers:
        mounted = [
            mount
            for mount in container.get("volumeMounts", [])
            if mount["name"] in tokens or mount.get("mountPath", "").startswith(TOKEN_PATH)
        ]
        if container["name"] == "sidecar" and len(mounted) != 1:
            wrong.append(f"the sidecar mounts the token {len(mounted)} times, not once")
        if container["name"] != "sidecar" and mounted:
            wrong.append(f"the {container['name']} container mounts the members' token")
    told = [
        env.get("value")
        for container in containers
        if container["name"] == "sidecar"
        for env in container.get("env", [])
        if env["name"] == "MERIDIAN_PLUGIN_SHARDS"
    ]
    if len(told) != 1 or (shards is not None and told[0] != shards):
        wrong.append(f"its sidecar is told the shard count {told}, not {shards or 'once'}")
    rolling = strategy.get("rollingUpdate", {})
    allowed = 2 if standby else 1
    if strategy.get("type") != "RollingUpdate" or rolling.get("maxSurge") != 1 or rolling.get("maxUnavailable") != allowed:
        wrong.append(
            f"its rollout is {strategy}: a shard at a time is RollingUpdate, maxSurge 1, "
            f"maxUnavailable {allowed} (one more for a standby pod, never ready)"
        )
    return wrong


def ha(render: str) -> list[str]:
    line = re.search(r"^  plugin-shards\.json: (\".*\")$", render, re.M)
    if not line or "MERIDIAN_LAUNCHER_SHARDS_TEMPLATE" not in render:
        return ["the ha profile gives the launcher no shape for shards and standby pods"]
    shape = json.loads(json.loads(line.group(1)))
    wrong = [
        f"the launcher's shards shape: {what}"
        for what in shape_wrong(shape["spec"]["template"]["spec"], shape["spec"].get("strategy", {}), True, None,
                                "__INSTANCE__")
    ]
    # Read alone: a stopped instance's pods, ended before its account is
    # removed (review c5b L5, c5b2 H1).
    roles = named(render, "Role", LAUNCHER)
    copies = template(render, "storage-copy.json") is not None
    pods = PODS_COPIED if copies else PODS_LISTED
    if not roles or not any(pods.search(doc) for doc in roles):
        wrong.append("the launcher with a shards shape may not list pods, to see a stopped instance's end"
                     if not copies else
                     "the launcher with a copy pod may not list pods and make, read and remove its copy pod")
    elif any(len(PODS.findall(doc)) != 1 for doc in roles):
        wrong.append("the launcher's Role names pods more than once")
    return wrong


def scalar(text: str):
    text = text.strip()
    if text in ("true", "false"):
        return text == "true"
    if re.fullmatch(r"-?\d+", text):
        return int(text)
    return text.strip('"')


def sharded(render: str, instance: str, pods: str, shards: str) -> list[str]:
    """The entry's own Deployment, read by its indentation: helm renders it
    block style, and this reads only what the checks name."""
    mine = [
        doc
        for doc in documents(render)
        if kind(doc) == "Deployment" and f'meridian.dev/instance: "{instance}"' in doc
    ]
    if len(mine) != 1:
        return [f"{len(mine)} Deployments are {instance}'s, not one"]
    doc = mine[0]
    wrong = []
    replicas = re.search(r"^  replicas: (\d+)$", doc, re.M)
    if not replicas or replicas.group(1) != pods:
        wrong.append(f"its replicas are {replicas.group(1) if replicas else None}, not {pods}")
    pod: dict = {"containers": [], "volumes": []}
    for key in ("serviceAccountName", "automountServiceAccountToken"):
        found = re.search(rf"^      {key}: (.+)$", doc, re.M)
        if found:
            pod[key] = scalar(found.group(1))
    # Each container, and each volume, is a `- name:` item at eight spaces
    # under its six-space key.
    section, item = None, None
    for line in doc.splitlines():
        if re.match(r"^      \S", line):
            section = line.strip().rstrip(":") if line.strip() in ("containers:", "initContainers:", "volumes:") else None
            item = None
            continue
        if section is None:
            continue
        start = re.match(r"^        - name: (\S+)$", line)
        if start:
            item = {"name": start.group(1).strip('"'), "volumeMounts": [], "env": [], "text": ""}
            pod["volumes" if section == "volumes" else "containers"].append(item)
            continue
        if item is not None:
            item["text"] += line + "\n"
    for volume in pod["volumes"]:
        if "serviceAccountToken:" in volume["text"]:
            volume["projected"] = {"sources": [{"serviceAccountToken": {}}]}
    for container in pod["containers"]:
        text = container["text"]
        for name, path in re.findall(r"^            - name: (\S+)\n              mountPath: (\S+)$", text, re.M):
            container["volumeMounts"].append({"name": name, "mountPath": path})
        if TOKEN_PATH in text and not any(m["mountPath"].startswith(TOKEN_PATH) for m in container["volumeMounts"]):
            container["volumeMounts"].append({"name": "?", "mountPath": TOKEN_PATH})
        for name, value in re.findall(r"^            - name: (\S+)\n              value: (.+)$", text, re.M):
            container["env"].append({"name": name, "value": str(scalar(value))})
    strategy = {}
    found = re.search(r"^  strategy:\n    type: (\S+)\n(?:    rollingUpdate:\n      maxSurge: (\d+)\n      maxUnavailable: (\d+)\n)?", doc, re.M)
    if found:
        strategy = {"type": found.group(1)}
        if found.group(2):
            strategy["rollingUpdate"] = {"maxSurge": int(found.group(2)), "maxUnavailable": int(found.group(3))}
    standby = int(pods) > int(shards)
    return wrong + [f"{instance}: {what}" for what in shape_wrong(pod, strategy, standby, shards, instance)]


def named(render: str, which: str, name: str) -> list[str]:
    return [doc for doc in documents(render) if kind(doc) == which and re.search(rf"^  name: {re.escape(name)}$", doc, re.M)]


def trio(render: str, instance: str, shards: str) -> list[str]:
    """A sidecars[] entry's own account, its Role, binding and Leases (item
    31), read by their lines: helm renders each block style, the Role's
    lists as the template writes them."""
    wrong = []
    leases = [f"{instance}-shard-{n}" for n in range(int(shards))]
    accounts = [doc for doc in documents(render) if kind(doc) == "ServiceAccount"
                and re.search(rf"^  name: \S+-shards-{re.escape(instance)}$", doc, re.M)]
    if len(accounts) != 1:
        return [f"{len(accounts)} ServiceAccounts are {instance}'s own, not one"]
    own = re.search(r"^  name: (\S+)$", accounts[0], re.M).group(1)
    if re.search(r"^(image[pP]ull)?[sS]ecrets:", accounts[0], re.M):
        wrong.append(f"{own} carries a secret")
    roles = named(render, "Role", own)
    if len(roles) != 1:
        wrong.append(f"{len(roles)} Roles are named {own}, not one")
    else:
        role = roles[0]
        rules = re.findall(r"^  - ", role, re.M)
        verbs = re.findall(r"^    verbs: (\[.*\])$", role, re.M)
        groups = re.findall(r"^  - apiGroups: (\[.*\])$", role, re.M)
        resources = re.findall(r"^    resources: (\[.*\])$", role, re.M)
        names = re.findall(r"^      - \"(\S+)\"$", role, re.M)
        if len(rules) != 1:
            wrong.append(f"{own}'s Role has {len(rules)} rules, not one")
        if [json.loads(v) for v in verbs] != [["get", "update"]]:
            wrong.append(f"{own}'s Role grants {verbs}, not get and update alone")
        if [json.loads(g) for g in groups] != [["coordination.k8s.io"]] or \
                [json.loads(r) for r in resources] != [["leases"]]:
            wrong.append(f"{own}'s Role is on {groups} {resources}, not leases alone")
        if names != leases:
            wrong.append(f"{own}'s Role names {names}, not exactly {leases}")
    bindings = named(render, "RoleBinding", own)
    if len(bindings) != 1:
        wrong.append(f"{len(bindings)} RoleBindings are named {own}, not one")
    else:
        binding = bindings[0]
        subjects = re.findall(r"^  - kind: (\S+)\n    name: (\S+)$", binding, re.M)
        ref = re.search(r"^roleRef:\n  kind: (\S+)\n  name: (\S+)$", binding, re.M)
        if subjects != [("ServiceAccount", own)]:
            wrong.append(f"{own}'s binding binds {subjects}, not {own} alone")
        if not ref or ref.groups() != ("Role", own):
            wrong.append(f"{own}'s binding is to {ref.groups() if ref else None}, not its own Role")
    for lease in leases:
        made = named(render, "Lease", lease)
        if len(made) != 1 or "helm.sh/resource-policy: keep" not in made[0]:
            wrong.append(f"{lease} is not rendered once and kept ({len(made)} rendered)")
    # The members' account is the stores' alone: no plugin's Deployment runs
    # under it, and its binding binds no plugin's account.
    for doc in documents(render):
        if kind(doc) == "Deployment" and "meridian.dev/component: sidecar" in doc and \
                re.search(r"^      serviceAccountName: \S+-member$", doc, re.M):
            wrong.append("a plugin's Deployment runs under the members' account, the stores' alone")
        if kind(doc) == "RoleBinding" and re.search(r"^  name: \S+-member$", re.split(r"^roleRef:", doc, flags=re.M)[-1], re.M) \
                and re.search(r"^    name: \S+-shards-", doc, re.M):
            wrong.append("the members' Role is bound to a plugin instance's own account")
    return wrong


DOWNWARD = {"MERIDIAN_POD_NAMESPACE": "metadata.namespace",
            "MERIDIAN_POD_SERVICE_ACCOUNT": "spec.serviceAccountName"}


def downward(render: str) -> list[str]:
    wrong, shapes = [], 0
    # The launcher's shapes, each a JSON Deployment in its ConfigMap.
    for key, text in re.findall(r"^  (plugin[-a-z]*\.json): (\".*\")$", render, re.M):
        shape = json.loads(json.loads(text))
        for container in shape["spec"]["template"]["spec"]["containers"]:
            if container["name"] != "plugin":
                continue
            shapes += 1
            told = {e["name"]: e.get("valueFrom", {}).get("fieldRef", {}).get("fieldPath")
                    for e in container.get("env", [])}
            for name, path in DOWNWARD.items():
                if told.get(name) != path:
                    wrong.append(f"the launcher's {key}: its plugin container is not told {name} from {path}")
    # The chart's own sidecars[] Deployments, read as they render.
    for doc in documents(render):
        if kind(doc) != "Deployment" or not re.search(r"^        - name: plugin$", doc, re.M):
            continue
        shapes += 1
        name = re.search(r"^  name: (\S+)$", doc, re.M).group(1)
        for env, path in DOWNWARD.items():
            if not re.search(rf"^            - name: {env}\n              valueFrom:\n                fieldRef:\n"
                             rf"                  fieldPath: {re.escape(path)}$", doc, re.M):
                wrong.append(f"{name}: its plugin container is not told {env} from {path}")
    if not shapes:
        wrong.append("the render has no plugin container to hold this check to")
    return wrong


def template(render: str, name: str):
    """The launcher's template `name` from its ConfigMap, as JSON; None
    where the render has none."""
    line = re.search(r"^  " + re.escape(name) + r": (\".*\")$", render, re.M)
    return json.loads(json.loads(line.group(1))) if line else None


def shared(render: str, rwx_class: str = "") -> list[str]:
    """Item 30: where shards and standby pods render and edge plugins keep
    storage, the launcher holds a shape sharing an instance's storage, the
    shared claim -- ReadWriteMany in the named class alone, else
    ReadWriteOnce -- and the copy pod, each of its exact shape; its policy
    holds the copy pod, and the plugins' storage policy admits the shared
    claim of a pod's own instance."""
    wrong = []
    claim = template(render, "shared-claim.json")
    shape = template(render, "plugin-shards-storage.json")
    copy = template(render, "storage-copy.json")
    if claim is None or shape is None or copy is None:
        return ["the launcher has no shared claim, shards storage shape or copy pod (item 30)"]
    if claim["metadata"]["name"] != "check-meridian-runtime-shared-storage-__INSTANCE__":
        wrong.append(f"the shared claim is named {claim['metadata']['name']}")
    modes, held = claim["spec"].get("accessModes"), claim["spec"].get("storageClassName", "")
    if rwx_class:
        if modes != ["ReadWriteMany"] or held != rwx_class:
            wrong.append(f"the shared claim is {modes} in {held!r}, not ReadWriteMany in {rwx_class!r}")
    elif modes != ["ReadWriteOnce"]:
        wrong.append(f"with no class named the shared claim is {modes}: ReadWriteMany renders only with the class set")
    pod = shape["spec"]["template"]["spec"]
    claims = [v["persistentVolumeClaim"]["claimName"] for v in pod.get("volumes", []) if "persistentVolumeClaim" in v]
    if claims != ["check-meridian-runtime-shared-storage-__INSTANCE__"]:
        wrong.append(f"the shards storage shape mounts {claims}, not the instance's shared storage alone")
    plugin = [c for c in pod["containers"] if c["name"] == "plugin"]
    if not plugin or not any(m["name"] == "storage" for m in plugin[0].get("volumeMounts", [])):
        wrong.append("the shards storage shape mounts no storage in the plugin's container")
    for c in pod["containers"]:
        if c["name"] != "plugin" and any(m["name"] == "storage" for m in c.get("volumeMounts", [])):
            wrong.append(f"the shards storage shape mounts storage in {c['name']}")
    if pod.get("initContainers"):
        wrong.append("the shards storage shape renders an init container; the launcher adds the copy's alone")
    spec = copy["spec"]
    volumes = {v["name"]: v.get("persistentVolumeClaim", {}) for v in spec.get("volumes", [])}
    if (copy["metadata"]["name"] != "check-meridian-runtime-storage-copy-__INSTANCE__"
            or len(spec["containers"]) != 1
            or spec["containers"][0].get("image") != "__IMAGE__"
            or spec["containers"][0].get("command") != STORAGE_COPY
            or "args" in spec["containers"][0]
            or spec.get("automountServiceAccountToken") is not False
            or volumes.get("from", {}).get("claimName") != "check-meridian-runtime-storage-__INSTANCE__"
            or volumes.get("from", {}).get("readOnly") is not True
            or volumes.get("to", {}).get("claimName") != "check-meridian-runtime-shared-storage-__INSTANCE__"
            or len(volumes) != 2):
        wrong.append("the copy pod is not the SDK's meridian-storage copy, in the instance's own plugin image, "
                     "of its own claim, read only, into its shared storage alone")
    # Review c5c M1: a copy that fails ends Failed, which the next launch
    # makes again; under OnFailure it restarts for ever, Running.
    if spec.get("restartPolicy") != "Never":
        wrong.append(f"the copy pod restarts {spec.get('restartPolicy')!r}: a failed copy must end, restartPolicy Never")
    # Item 86: the copy runs as the plugin's own user, its image's or the
    # one the cluster assigns, so it reads its private files and what it
    # writes is its own to write on; no uid pinned, which OpenShift refuses.
    held = spec.get("securityContext", {})
    pinned = [key for key in ("runAsUser", "runAsGroup")
              if key in held or key in spec["containers"][0].get("securityContext", {})]
    if pinned:
        wrong.append(f"the copy pod pins {pinned}: it runs as its plugin's own user")
    if held.get("seccompProfile", {}).get("type") != "RuntimeDefault":
        wrong.append("the copy pod's seccomp profile is not RuntimeDefault")
    box = spec["containers"][0].get("securityContext", {})
    if (box.get("capabilities") != {"drop": ["ALL"]} or box.get("readOnlyRootFilesystem") is not True
            or box.get("allowPrivilegeEscalation") is not False):
        wrong.append(f"the copy container's securityContext is {box}: drop ALL and add none, a read-only root, "
                     "no privilege escalation")
    if "the launcher's one pod copies its instance's own storage into its shared storage" not in render:
        wrong.append("the launcher's admission policy does not hold the copy pod to its shape")
    # The policy holds what the shape renders (review c5c M1, H1, L1), and
    # deletes the copy pod alone (L2).
    for held_to, what in (('object.spec.restartPolicy == "Never"', "restartPolicy Never"),
                          ('object.spec.containers[0].command == ["meridian-storage", "copy",',
                           "the SDK's meridian-storage copy"),
                          ('/plugins/[a-z]([-a-z0-9]{0,61}[a-z0-9])?@sha256:[0-9a-f]{64}$', "a plugin image by digest"),
                          ("!has(object.spec.containers[0].args)", "no args beside its command"),
                          ("!has(object.spec.containers[0].securityContext.capabilities.add)", "no capability added"),
                          ("!has(object.spec.containers[0].lifecycle)", "no lifecycle hook"),
                          ("!has(object.spec.nodeName)", "no node named"),
                          ('oldObject.metadata.labels[?"meridian.dev/component"].orValue("") == "storage-copy"',
                           "a DELETE of the copy pod alone")):
        if held_to not in render:
            wrong.append(f"the launcher's admission policy does not hold the copy pod to {what}")
    if "check-meridian-runtime-shared-storage-' +" not in render:
        wrong.append("the plugins' storage policy does not admit a pod's own instance's shared storage")
    return wrong


def env_of(container: dict, name: str):
    for e in container.get("env", []) or []:
        if e.get("name") == name:
            return e.get("value")
    return None


def plugin_told(pod: dict, where: str) -> list[str]:
    """PO item 73: where a pod's sidecar is told MERIDIAN_PLUGIN_SHARDS, its
    plugin container is told the same value."""
    containers = {c["name"]: c for c in pod.get("containers", [])}
    said = env_of(containers.get("sidecar", {}), "MERIDIAN_PLUGIN_SHARDS")
    plugin = containers.get("plugin")
    if said is None or plugin is None:
        return []
    held = env_of(plugin, "MERIDIAN_PLUGIN_SHARDS")
    if held != said:
        return [f"{where}: the sidecar is told MERIDIAN_PLUGIN_SHARDS={said!r}, its plugin container {held!r}"]
    return []


def plugin_shards(render: str) -> list[str]:
    """PO item 73 (2026-11-17): every plugin container is told the shard
    count its sidecar is, in each shape that tells the sidecar: the
    launcher's shards shapes and a sidecars[] instance of several pods. A
    render with no such shape at all is a check that proves nothing."""
    wrong, seen = [], 0
    for name in ("plugin-shards.json", "plugin-shards-storage.json"):
        shape = template(render, name)
        if shape is not None:
            seen += 1
            wrong += plugin_told(shape["spec"]["template"]["spec"], f"the launcher's {name}")
    for doc in documents(render):
        if kind(doc) != "Deployment" or "name: MERIDIAN_PLUGIN_SHARDS" not in doc:
            continue
        seen += 1
        name = re.search(r"^  name: (\S+)$", doc, re.M)
        values = re.findall(r"- name: MERIDIAN_PLUGIN_SHARDS\n\s+value: (\S+)", doc)
        plugin_at = re.search(r"^        - name: plugin$", doc, re.M)
        after = re.findall(r"- name: MERIDIAN_PLUGIN_SHARDS\n\s+value: (\S+)", doc[plugin_at.start():]) if plugin_at else []
        if plugin_at and (len(values) != 2 or len(after) != 1 or values[0] != after[0]):
            wrong.append(f"{name.group(1) if name else 'a Deployment'}: MERIDIAN_PLUGIN_SHARDS {values}, "
                         f"its plugin container's {after}")
    if not seen:
        wrong.append("no shape tells a sidecar MERIDIAN_PLUGIN_SHARDS in this render")
    return wrong


def main() -> int:
    render = sys.stdin.read()
    mode, rest = sys.argv[1], sys.argv[2:]
    wrong = {"default": default, "ha": ha, "sharded": sharded, "trio": trio, "downward": downward,
             "shared": shared, "plugin-shards": plugin_shards}[mode](render, *rest)
    for what in wrong:
        print(f"chart-check FAILED (step 3d, shards): {what}", file=sys.stderr)
    return 1 if wrong else 0


if __name__ == "__main__":
    sys.exit(main())
