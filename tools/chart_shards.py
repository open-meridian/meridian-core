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
            SHARDS): the same, read from its own Deployment.

Usage:  helm template ... | python3 tools/chart_shards.py default
        helm template ... | python3 tools/chart_shards.py ha
        helm template ... | python3 tools/chart_shards.py sharded feed-1 3 2
Exit:   0 as it should be, 1 naming what is not
"""

from __future__ import annotations

import json
import re
import sys

TOKEN_PATH = "/var/run/secrets/kubernetes.io/serviceaccount"
MEMBER = re.compile(r"^  name: \S+-member$", re.M)


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
    return wrong


def shape_wrong(pod: dict, strategy: dict, standby: bool, shards: str | None) -> list[str]:
    """What is wrong with a several-pod instance's pod spec, read as JSON."""
    wrong = []
    account = pod.get("serviceAccountName", "")
    if not account.endswith("-member"):
        wrong.append(f"its account is {account!r}, not the members'")
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
    return [
        f"the launcher's shards shape: {what}"
        for what in shape_wrong(shape["spec"]["template"]["spec"], shape["spec"].get("strategy", {}), True, None)
    ]


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
    return wrong + [f"{instance}: {what}" for what in shape_wrong(pod, strategy, standby, shards)]


def main() -> int:
    render = sys.stdin.read()
    mode, rest = sys.argv[1], sys.argv[2:]
    wrong = {"default": default, "ha": ha, "sharded": sharded}[mode](render, *rest)
    for what in wrong:
        print(f"chart-check FAILED (step 3d, shards): {what}", file=sys.stderr)
    return 1 if wrong else 0


if __name__ == "__main__":
    sys.exit(main())
