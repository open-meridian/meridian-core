"""The cluster jobs' own scripts, tested with stand-ins and no network: part
of `make e2e-harness-check`.

- tools/ci-k3s-images.sh: k3d's own images are fetched, saved and loaded
  beside the node image, and the airgap tarball's fetch backs off between
  tries, a bounded number of them, within the 300 s that bounded it before
  (queue run 45307348738: a 403 on all seven tries inside one minute).
- tools/ci-watchdog.sh proof pulls: making the cluster pulled nothing, or
  the job fails (run 45300919431 died on k3d's pull).
- tools/ci-watchdog.sh verdict: a failed job's last line names the failed
  check, or the step that died where no check ran (v31a item 11).
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

    def test_a_refused_fetch_is_tried_again_after_waits_that_grow(self):
        done = self.run_script("fetch", os.path.join(self.root, "images"), "v1.0.0-k3s1", fails=2)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(self.said("curl")), 3)
        self.assertEqual(self.said("sleep")[:3], ["0", "15", "45"])
        self.assertIn("try 2 for", done.stderr)
        self.assertIn("(curl exit 22)", done.stderr)

    def test_a_fetch_refused_every_time_stops_after_a_bounded_number_of_tries(self):
        done = self.run_script("fetch", os.path.join(self.root, "images"), "v1.0.0-k3s1", fails=99)
        self.assertEqual(done.returncode, 1)
        self.assertEqual(len(self.said("curl")), 4)
        self.assertEqual(self.said("sleep"), ["0", "15", "45", "120"])
        self.assertIn("was not fetched in 4 tries, within 300s", done.stderr)
        self.assertEqual([c for c in self.said("docker") if c.startswith("pull")], [])

    def test_no_try_begins_past_the_bound(self):
        done = self.run_script("fetch", os.path.join(self.root, "images"), "v1.0.0-k3s1", fails=99,
                               env={"FETCH_WITHIN": "50"})
        self.assertEqual(done.returncode, 1)
        self.assertEqual(self.said("sleep"), ["0", "15", "45"])

    def test_each_try_ends_a_stalled_transfer(self):
        self.run_script("fetch", os.path.join(self.root, "images"), "v1.0.0-k3s1")
        self.assertIn("--speed-time 60", self.said("curl")[0])


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

    def last_line(self, *args, **kwargs):
        done = self.run_watchdog("verdict", *args, **kwargs)
        self.assertEqual(done.returncode, 0, done.stderr)
        return done.stdout.strip().splitlines()[-1]

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

    def test_the_last_line_names_the_first_failed_check_and_how_many(self):
        line = self.last_line(log="    ok: the API answers\n"
                                  "    FAILED: broker: its servers restarted one at a time\n"
                                  "    FAILED: analytics: every copy has JetStream's work queue\n"
                                  "e2e-cluster-failover FAILED (ha): 2\n"
                                  "make[1]: *** [Makefile:1300: e2e-cluster-failover] Error 1\n",
                              steps={"make": {"outcome": "failure"}})
        self.assertEqual(line, "FAILED: broker: its servers restarted one at a time (the first of 2 failed checks)")

    def test_the_last_line_names_a_ready_wait_that_gave_up(self):
        line = self.last_line(log="e2e-cluster-k3d: waiting for kube-system's deployments, at most 300s\n"
                                  "e2e-cluster-k3d: gave up on kube-system's deployments to be available after 300s. It said:\n"
                                  "make: *** [Makefile:1080: e2e-cluster-k3d] Error 1\n",
                              steps={"make": {"outcome": "failure"}})
        self.assertEqual(line, "FAILED: making the cluster: gave up on kube-system's deployments to be available after 300s")

    def test_the_last_line_names_the_make_s_step_that_died_before_any_check(self):
        line = self.last_line(log="e2e-cluster: installing the chart\n"
                                  "Error: INSTALLATION FAILED: context deadline exceeded\n"
                                  "make[1]: *** [Makefile:1500: e2e-cluster] Error 1\n"
                                  "make: *** [Makefile:1093: e2e-cluster-k3d] Error 2\n",
                              steps={"make": {"outcome": "failure"}})
        self.assertEqual(line, "FAILED: the make's step Makefile:1500: e2e-cluster died before any check failed "
                               "(Error 1); its last line: Error: INSTALLATION FAILED: context deadline exceeded")

    def test_the_last_line_names_a_step_before_the_make(self):
        line = self.last_line(log="", steps={"images-built": {"outcome": "success"},
                                             "k3s-images": {"outcome": "failure"},
                                             "make": {"outcome": "skipped"}})
        self.assertEqual(line, "FAILED: the step k3s-images (failure), before the make ran")

    def test_the_last_line_names_a_make_cut_off_by_the_job_s_limit(self):
        line = self.last_line(log="e2e-cluster-failover: the driver writes\n",
                              steps={"make": {"outcome": "cancelled"}})
        self.assertEqual(line, "FAILED: the make was cancelled (the job's time limit) before any check failed; "
                               "its last line: e2e-cluster-failover: the driver writes")

    def test_the_last_line_names_a_step_after_the_make(self):
        line = self.last_line(log=self.READY.format(system="none", host="x:1"),
                              steps={"make": {"outcome": "success"}, "pulls": {"outcome": "failure"}})
        self.assertEqual(line, "FAILED: the step pulls (failure), after the make")


if __name__ == "__main__":
    unittest.main()
