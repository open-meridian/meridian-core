"""The cluster jobs' own scripts, tested with stand-ins and no network: part
of `make e2e-harness-check`.

- tools/ci-k3s-images.sh: k3d's own images are fetched, saved and loaded
  beside the node image.
- tools/ci-watchdog.sh proof pulls: making the cluster pulled nothing, or
  the job fails (run 45300919431 died on k3d's pull).
"""

import hashlib
import json
import os
import shutil
import stat
import subprocess
import tempfile
import unittest

TOOLS = os.path.dirname(os.path.abspath(__file__))


def stub(directory, name, body):
    path = os.path.join(directory, name)
    with open(path, "w") as f:
        f.write("#!/bin/sh\n" + body)
    os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR)


class K3sImages(unittest.TestCase):
    """ci-k3s-images.sh in a copy of tools/, with curl, docker and sleep
    stood in for: each records its calls."""

    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="c4a-k3s-")
        self.tools = os.path.join(self.root, "tools")
        self.bin = os.path.join(self.root, "bin")
        self.calls = os.path.join(self.root, "calls")
        os.makedirs(self.tools)
        os.makedirs(self.bin)
        shutil.copy(os.path.join(TOOLS, "ci-k3s-images.sh"), self.tools)
        shutil.copy(os.path.join(TOOLS, "ci-tools.txt"), self.tools)
        self.payload = b"the airgap tarball"
        digest = hashlib.sha256(self.payload).hexdigest()
        with open(os.path.join(self.tools, "ci-k3s-images.txt"), "w") as f:
            for arch in ("x86_64", "aarch64"):
                f.write(f"v1.0.0-k3s1 {arch} {digest}\n")
        with open(os.path.join(self.root, "payload"), "wb") as f:
            f.write(self.payload)
        # curl fails (exit 22) while $CURL_FAILS counts down, then writes the
        # payload to its -o file.
        stub(self.bin, "curl", f"""
echo "curl $*" >>{self.calls}
n=$(cat {self.root}/fails 2>/dev/null || echo 0)
if [ "$n" -gt 0 ]; then echo $((n - 1)) >{self.root}/fails; exit 22; fi
while [ $# -gt 0 ]; do [ "$1" = -o ] && cp {self.root}/payload "$2"; shift; done
""")
        stub(self.bin, "sleep", f'echo "sleep $1" >>{self.calls}\n')
        stub(self.bin, "docker", f"""
echo "docker $*" >>{self.calls}
case "$1" in
  save) while [ $# -gt 0 ]; do [ "$1" = -o ] && echo image >"$2"; shift; done ;;
esac
""")

    def tearDown(self):
        shutil.rmtree(self.root)

    def run_script(self, *args, fails=0, env=None):
        with open(os.path.join(self.root, "fails"), "w") as f:
            f.write(str(fails))
        environ = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}", **(env or {}))
        return subprocess.run(["sh", os.path.join(self.tools, "ci-k3s-images.sh"), *args],
                              capture_output=True, text=True, env=environ)

    def said(self, prefix):
        with open(self.calls) as f:
            return [line.split(" ", 1)[1].strip() for line in f if line.startswith(prefix + " ")]

    def k3d(self):
        with open(os.path.join(TOOLS, "ci-tools.txt")) as f:
            for line in f:
                if "/k3d-io/k3d/releases/download/v" in line:
                    return line.split("/download/v")[1].split("/")[0]
        raise AssertionError("tools/ci-tools.txt pins no k3d")

    def test_k3d_s_own_images_are_fetched_saved_and_loaded_beside_the_node_image(self):
        images = os.path.join(self.root, "images")
        done = self.run_script("fetch", images, "v1.0.0-k3s1")
        self.assertEqual(done.returncode, 0, done.stderr)
        k3d = self.k3d()
        want = ["rancher/k3s:v1.0.0-k3s1", f"ghcr.io/k3d-io/k3d-proxy:{k3d}", f"ghcr.io/k3d-io/k3d-tools:{k3d}"]
        self.assertEqual([c.split()[-1] for c in self.said("docker") if c.startswith("pull")], want)
        loaded = self.run_script("load", images, "v1.0.0-k3s1")
        self.assertEqual(loaded.returncode, 0, loaded.stderr)
        self.assertEqual(len([c for c in self.said("docker") if c.startswith("load")]), 3)
        self.assertEqual([c.split()[-1] for c in self.said("docker") if c.startswith("image inspect")], want)
        # A second fetch finds all three kept, and pulls nothing more.
        again = self.run_script("fetch", images, "v1.0.0-k3s1")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(len([c for c in self.said("docker") if c.startswith("pull")]), 3)

    def test_the_cache_key_names_k3d_s_version(self):
        done = self.run_script("key", "v1.0.0-k3s1")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(done.stdout.strip().endswith(f"-k3d-{self.k3d()}"), done.stdout)


class Watchdog(unittest.TestCase):
    """ci-watchdog.sh's file-only facts, in a directory of their own, so no
    .e2e-*.log of this checkout is read."""

    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="c4a-watchdog-")
        self.make = os.path.join(self.dir, "make.log")

    def tearDown(self):
        shutil.rmtree(self.dir)

    READY = ("e2e-cluster-k3d: the cluster is ready, 41s after it was made\n"
             "e2e-cluster-k3d: images pulled while it was made: {system}\n"
             "e2e-cluster-k3d: images the host pulled while it was made: {host}\n")

    def run_watchdog(self, *args, log=None, steps=None):
        if log is not None:
            with open(self.make, "w") as f:
                f.write(log)
        env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_STEP_SUMMARY", "GITHUB_OUTPUT")}
        if steps is not None:
            env["STEPS"] = json.dumps(steps)
        return subprocess.run(["bash", os.path.join(TOOLS, "ci-watchdog.sh"), *args, self.make],
                              capture_output=True, text=True, cwd=self.dir, env=env)

    def test_pulls_none_from_either_passes(self):
        done = self.run_watchdog("proof", "pulls", log=self.READY.format(system="none", host="none"))
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn("pulled nothing", done.stdout)

    def test_a_pull_by_the_host_fails(self):
        done = self.run_watchdog("proof", "pulls", log=self.READY.format(
            system="none", host="ghcr.io/k3d-io/k3d-proxy:5.9.0"))
        self.assertEqual(done.returncode, 1)
        self.assertIn("k3d-proxy", done.stdout)

    def test_a_pull_by_kube_system_fails(self):
        done = self.run_watchdog("proof", "pulls", log=self.READY.format(
            system='"rancher/mirrored-pause:3.6"; ', host="none"))
        self.assertEqual(done.returncode, 1)

    def test_a_cluster_never_ready_has_no_pulls_to_judge(self):
        done = self.run_watchdog("proof", "pulls", log="e2e-cluster-k3d: gave up on x after 300s. It said:\n")
        self.assertEqual(done.returncode, 0)
        self.assertIn("nothing to judge", done.stdout)


if __name__ == "__main__":
    unittest.main()
