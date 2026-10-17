"""Run by `make e2e-plugin-area` after 1.4.0 is installed: its provenance
taken out of the referrers the deployment's own registry keeps for it (the
tag schema's index, `sha256-<hex>`), as anyone able to write that registry
could, so the launch's verification again (W8.3; the spec's requirement 24)
has to refuse it. Standard library only."""
import json
import os
import urllib.request

REGISTRY = "http://registry:5000/v2/plugins/open-meridian/stand-in"
INDEX = "application/vnd.oci.image.index.v1+json"
with open(os.path.join(os.path.dirname(__file__), "cases.json"), encoding="utf-8") as held:
    CASES = json.load(held)
tag = CASES["1.4.0"].replace(":", "-", 1)
asked = urllib.request.Request(f"{REGISTRY}/manifests/{tag}", headers={"Accept": INDEX})
index = json.load(urllib.request.urlopen(asked, timeout=30))
before = len(index["manifests"])
index["manifests"] = [m for m in index["manifests"] if m["digest"] != CASES["provenance_1.4.0"]]
if len(index["manifests"]) != before - 1:
    raise SystemExit(f"the provenance {CASES['provenance_1.4.0']} is not among {before} referrers")
put = urllib.request.Request(f"{REGISTRY}/manifests/{tag}", data=json.dumps(index).encode(),
                             method="PUT", headers={"Content-Type": INDEX})
urllib.request.urlopen(put, timeout=30)
print(f"tamper: {before} referrers, now {len(index['manifests'])}: the provenance is gone")
