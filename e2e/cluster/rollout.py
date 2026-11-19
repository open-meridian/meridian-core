"""Whether a release's workloads have finished rolling out, judged from their
objects as `meridian upgrade-deployment` judges it before it says an upgrade
is done (meridian-cli, upgrade_deployment.rs): each template seen by its
controller, every replica on it and ready, and nothing old left -- a
StatefulSet's current revision its update's.

`kubectl wait --for=condition=Available` is not that. A Deployment is
Available while no more of its pods are unavailable than its strategy
allows, so one that replaces a pod at a time (the dashboard's maxSurge 0,
maxUnavailable 1, with two copies) is Available all through its rollout,
an old pod still to be replaced: PR CI 44604574060 (failover, ha) port-
forwarded to the dashboard's last old pod there, and the rollout ended it
under the Move dialog's post ("Remote end closed connection without
response").

Shared by e2e/cluster/run.py and e2e/failover/run.py, which ask the API for
the objects. `rollout.py --self-test` holds the judgment to cases, no
cluster.
"""

import sys


def unfinished(deployments, statefulsets):
    """Each of the Deployments and StatefulSets given (as the API answers
    them) whose rollout is not finished, and why; none when every one is."""
    left = []
    for kind, items in (("deployments", deployments), ("statefulsets", statefulsets)):
        for item in items:
            name, spec, status = item["metadata"]["name"], item.get("spec", {}), item.get("status", {})
            wanted = spec.get("replicas", 1)
            if item["metadata"].get("generation", 0) > status.get("observedGeneration", 0):
                left.append(f"{name}: its new template not yet seen")
            elif status.get("updatedReplicas", 0) < wanted:
                left.append(f"{name}: {status.get('updatedReplicas', 0)} of {wanted} on the new template")
            elif status.get("readyReplicas", 0) < wanted:
                left.append(f"{name}: {status.get('readyReplicas', 0)} of {wanted} ready")
            elif kind == "deployments" and (status.get("replicas", 0) > wanted
                                            or status.get("availableReplicas", 0) < wanted):
                left.append(f"{name}: old pods still being replaced")
            elif kind == "statefulsets" and spec.get("updateStrategy", {}).get("type") != "OnDelete" \
                    and status.get("currentRevision") != status.get("updateRevision"):
                left.append(f"{name}: its update not finished")
    return left


def deployment(name, replicas, updated, ready, available=None, total=None, generation=1, observed=1):
    """A Deployment as the API answers one, its Available condition True:
    for the self-tests here and the harness's (e2e/failover/self_test.py)."""
    available = ready if available is None else available
    return {
        "metadata": {"name": name, "generation": generation},
        "spec": {"replicas": replicas},
        "status": {"observedGeneration": observed, "replicas": replicas if total is None else total,
                   "updatedReplicas": updated, "readyReplicas": ready, "availableReplicas": available,
                   "conditions": [{"type": "Available", "status": "True"}]},
    }


def self_test():
    failed = []

    def case(said, want, what):
        if bool(said) != want:
            failed.append(f"{what}: {'unfinished' if want else 'finished'} was wanted, it said {said or 'finished'}")

    # The dashboard as PR CI 44604574060 met it: two copies, one replaced
    # and ready, one old still serving -- Available, and not rolled out.
    case(unfinished([deployment("dashboard", 2, 1, 1)], []), True,
         "two copies, one on the new template")
    case(unfinished([deployment("dashboard", 2, 2, 1)], []), True,
         "both on the new template, one not yet ready")
    case(unfinished([deployment("dashboard", 2, 2, 2, total=3)], []), True,
         "an old pod left beside two new ones")
    case(unfinished([deployment("dashboard", 2, 2, 2, generation=2, observed=1)], []), True,
         "a new template its controller has not seen")
    case(unfinished([deployment("dashboard", 2, 2, 2)], []), False,
         "both copies on the new template and ready")
    set_rolling = {"metadata": {"name": "database", "generation": 3},
                   "spec": {"replicas": 3, "updateStrategy": {"type": "RollingUpdate"}},
                   "status": {"observedGeneration": 3, "replicas": 3, "updatedReplicas": 3, "readyReplicas": 3,
                              "currentRevision": "a", "updateRevision": "b"}}
    case(unfinished([], [set_rolling]), True, "a StatefulSet whose update is not finished")
    on_delete = {**set_rolling, "spec": {"replicas": 3, "updateStrategy": {"type": "OnDelete"}}}
    case(unfinished([], [on_delete]), False, "an OnDelete StatefulSet, which rolls only as its pods are deleted")
    case(unfinished([], [{**set_rolling, "status": {**set_rolling["status"], "currentRevision": "b"}}]), False,
         "a StatefulSet rolled out")
    for line in failed:
        print(f"rollout self-test: {line}", file=sys.stderr)
    if not failed:
        print("rollout: self-test OK")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(self_test() if sys.argv[1:] == ["--self-test"] else "usage: rollout.py --self-test")
