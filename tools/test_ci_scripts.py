"""The cluster jobs' own scripts, tested with stand-ins and no network: part
of `make e2e-harness-check`.

- tools/ci-k3s-images.sh: k3d's own images are fetched, saved and loaded
  beside the node image, and the airgap tarball's fetch backs off between
  tries, a bounded number of them, within the 300 s that bounded it before
  (queue run 45307348738: a 403 on all seven tries inside one minute).
- tools/ci-watchdog.sh proof pulls: the host pulled nothing while the
  cluster was made, or the job fails (run 45300919431 died on k3d's pull);
  a pull by kube-system is a warning naming the image, as on main, where it
  was said and never failed a job (a known gap: CoreDNS, 4 of 52
  three-agent jobs).
- tools/ci-watchdog.sh verdict: a failed job's last line names the failed
  check, or the step that died where no check ran (v31a item 11).
- e2e/cluster/nodes-steady.sh: the cluster is ready only when every node is
  Ready and no node container is restarting, its restart count steady
  (v31a item 11; queue run 45395079443's agent-2 restart-looped while every
  ready wait passed).
- e2e/cluster/image-load.sh: an image that never reached a node says that
  node's container state (restarts, exit code, OOMKilled) and its log's last
  200 lines (e2e/cluster/node-state.sh), as the watchdog's evidence does for
  every node (v31a item 11).
- tools/ci-watchdog.sh kernel-again: the kernel's log read again at the end
  for stall lines, which a hung task writes about two minutes after it
  blocks, after the proof's read; said, and never failing (v31a item 11).
- tools/ci-sdk.sh: interop reads the SDK release tools/ci-sdk.txt pins, and
  another ref only where a run names it, said as a warning (v31a item 11).
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

    def test_a_pull_by_kube_system_is_a_warning_naming_the_image(self):
        done = self.run_watchdog("proof", "pulls", log=self.READY.format(
            system='"rancher/mirrored-coredns-coredns:1.14.7"; ', host="none"))
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn("::warning ", done.stdout)
        self.assertIn("mirrored-coredns-coredns:1.14.7", done.stdout)
        self.assertNotIn("::error", done.stdout)

    def test_a_pull_by_both_fails_on_the_host_s_and_names_each(self):
        done = self.run_watchdog("proof", "pulls", log=self.READY.format(
            system='"rancher/mirrored-coredns-coredns:1.14.7"; ', host="ghcr.io/k3d-io/k3d-tools:5.9.0"))
        self.assertEqual(done.returncode, 1)
        self.assertIn("::error", done.stdout)
        self.assertIn("k3d-tools", done.stdout)
        self.assertIn("mirrored-coredns-coredns", done.stdout)

    def test_a_host_line_that_is_missing_fails(self):
        done = self.run_watchdog("proof", "pulls", log=(
            "e2e-cluster-k3d: images pulled while it was made: none\n"))
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


class KernelAgain(unittest.TestCase):
    """ci-watchdog.sh kernel-again, with sudo, dmesg and journalctl stood
    in for: the kernel's log is a file the test writes."""

    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="c4b2-kernel-")
        self.bin = os.path.join(self.dir, "bin")
        os.makedirs(self.bin)
        self.make = os.path.join(self.dir, "make.log")
        self.dmesg = os.path.join(self.dir, "dmesg")
        stub(self.bin, "sudo", "exit 1\n")
        stub(self.bin, "dmesg", f"cat {self.dmesg}\n")
        stub(self.bin, "journalctl", "exit 1\n")

    def tearDown(self):
        shutil.rmtree(self.dir)

    def again(self, kernel):
        with open(self.dmesg, "w") as f:
            f.write(kernel)
        env = {k: v for k, v in os.environ.items() if k not in ("GITHUB_STEP_SUMMARY", "GITHUB_OUTPUT")}
        env["PATH"] = f"{self.bin}:{env['PATH']}"
        return subprocess.run(["bash", os.path.join(TOOLS, "ci-watchdog.sh"), "kernel-again", self.make],
                              capture_output=True, text=True, cwd=self.dir, env=env)

    HUNG = ("[  1.0] NET: Registered PF_NETLINK/PF_ROUTE protocol family\n"
            "[ 245.1] INFO: task containerd:4242 blocked for more than 120 seconds.\n"
            "[ 245.2] hung_task: blocked tasks\n")

    def test_a_hung_task_logged_after_the_proof_is_said_and_fails_nothing(self):
        done = self.again(self.HUNG)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("kernel, read again: matched, 2 blocked-task", done.stdout)
        self.assertIn("::warning title=kernel, read again: matched", done.stdout)
        with open(os.path.join(self.dir, "proof.log")) as f:
            self.assertIn("INFO: task containerd:4242 blocked", f.read())

    def test_a_clean_kernel_is_said_clean(self):
        done = self.again("[  1.0] NET: Registered PF_NETLINK/PF_ROUTE protocol family\n")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("kernel, read again: clean, 1 lines read by dmesg", done.stdout)
        self.assertNotIn("::warning", done.stdout)

    def test_an_unreadable_kernel_is_said_unreadable(self):
        done = self.again("")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("kernel, read again: unreadable", done.stdout)

    def test_the_evidence_and_its_beacon_read_it_again(self):
        with open(os.path.join(TOOLS, "ci-watchdog.sh")) as f:
            watchdog = f.read()
        evidence = watchdog[watchdog.index("\n  evidence)"):watchdog.index("\n  beacon)")]
        beacon = watchdog[watchdog.index("\n  beacon)"):watchdog.index("\n  kernel-again)")]
        self.assertIn('kernel-again "$file"', evidence)
        self.assertIn("kernel_scan", beacon)


E2E = os.path.join(TOOLS, "..", "e2e", "cluster")


class NodesSteady(unittest.TestCase):
    """nodes-steady.sh with docker, kubectl and date stood in for, each
    answering from a file the test writes."""

    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="c4b2-steady-")
        self.bin = os.path.join(self.root, "bin")
        os.makedirs(self.bin)
        self.state = os.path.join(self.root, "state")
        # docker: `ps` lists the node files' names; `inspect` answers a node's
        # file: running restarting restarts exit-code oomkilled.
        stub(self.bin, "docker", f"""
case "$1" in
  ps) ls {self.root}/node | sed 's/^/x/;s/^x//'; echo k3d-c-serverlb ;;
  inspect) eval last=\\${{$#}}; cat {self.root}/node/$last ;;
  logs) echo "the log of $3" ;;
esac
""")
        stub(self.bin, "kubectl", f"cat {self.root}/ready\n")
        stub(self.bin, "date", f"cat {self.root}/now\n")
        os.makedirs(os.path.join(self.root, "node"))

    def tearDown(self):
        shutil.rmtree(self.root)

    def nodes(self, now, ready=True, **nodes):
        """Each node keyword: (running, restarting, restarts)."""
        for name in os.listdir(os.path.join(self.root, "node")):
            os.remove(os.path.join(self.root, "node", name))
        lines = []
        for name, (running, restarting, restarts) in nodes.items():
            node = "k3d-c-" + name.replace("_", "-")
            with open(os.path.join(self.root, "node", node), "w") as f:
                f.write(f"{str(running).lower()} {str(restarting).lower()} {restarts} 0 false\n")
            lines.append(f"{node} {'True' if ready is True or (ready and name not in ready) else 'False'}")
        with open(os.path.join(self.root, "ready"), "w") as f:
            f.write("\n".join(lines) + "\n")
        with open(os.path.join(self.root, "now"), "w") as f:
            f.write(f"{now}\n")

    def ask(self):
        env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}", E2E_K3D_STEADY_SECONDS="30")
        return subprocess.run(["sh", os.path.join(E2E, "nodes-steady.sh"), "c", self.state],
                              capture_output=True, text=True, env=env)

    def test_nodes_ready_and_never_restarted_pass_once_steady_for_long_enough(self):
        self.nodes(1000, server_0=(True, False, 0), agent_0=(True, False, 0))
        self.assertEqual(self.ask().returncode, 1)
        self.nodes(1020, server_0=(True, False, 0), agent_0=(True, False, 0))
        early = self.ask()
        self.assertEqual(early.returncode, 1)
        self.assertIn("have held for 20s of 30s", early.stdout)
        self.nodes(1031, server_0=(True, False, 0), agent_0=(True, False, 0))
        done = self.ask()
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertIn("has not restarted for 31s", done.stdout)

    def test_a_node_restart_looping_never_passes(self):
        # Agent-2's loop of queue run 45395079443: running at every look,
        # its count higher at each.
        for turn in range(12):
            self.nodes(1000 + 18 * turn, server_0=(True, False, 0), agent_2=(True, False, turn))
            done = self.ask()
            self.assertEqual(done.returncode, 1, f"turn {turn}: {done.stdout}")
        self.assertIn("a node container restarted", done.stdout)
        self.assertIn("were k3d-c-agent-2=10 k3d-c-server-0=0, now k3d-c-agent-2=11 k3d-c-server-0=0", done.stdout)

    def test_a_node_container_restarting_or_stopped_fails_and_starts_the_clock_again(self):
        self.nodes(1000, server_0=(True, False, 0), agent_0=(True, False, 1))
        self.ask()
        self.nodes(1040, server_0=(True, False, 0), agent_0=(False, True, 1))
        down = self.ask()
        self.assertEqual(down.returncode, 1)
        self.assertIn("k3d-c-agent-0 (running false, restarting true, restarts 1, last exit 0, OOMKilled false)", down.stdout)
        # Up again with its count unchanged: the clock started again.
        self.nodes(1050, server_0=(True, False, 0), agent_0=(True, False, 1))
        self.assertEqual(self.ask().returncode, 1)
        self.nodes(1081, server_0=(True, False, 0), agent_0=(True, False, 1))
        self.assertEqual(self.ask().returncode, 0)

    def test_a_node_not_ready_fails_though_its_container_is_steady(self):
        self.nodes(1000, ready={"agent_1"}, server_0=(True, False, 0), agent_1=(True, False, 0))
        self.ask()
        self.nodes(1100, ready={"agent_1"}, server_0=(True, False, 0), agent_1=(True, False, 0))
        done = self.ask()
        self.assertEqual(done.returncode, 1)
        self.assertIn("a node is not Ready: k3d-c-agent-1 (Ready False)", done.stdout)
        # Ready later: steady since the first look, so it passes at once.
        self.nodes(1101, server_0=(True, False, 0), agent_1=(True, False, 0))
        self.assertEqual(self.ask().returncode, 0)

    def test_no_node_container_fails(self):
        self.nodes(1000)
        done = self.ask()
        self.assertEqual(done.returncode, 1)
        self.assertIn("no node container", done.stdout)

    def test_the_bring_up_asks_it_as_the_cluster_is_made_and_in_its_last_wait(self):
        with open(os.path.join(TOOLS, "..", "Makefile")) as f:
            make = f.read()
        recipe = make[make.index("\ne2e-cluster-k3d:"):]
        recipe = recipe[:recipe.index("the cluster is ready, ")]
        first = recipe.index("e2e/cluster/nodes-steady.sh")
        self.assertLess(first, recipe.index('ready "the API server'))
        self.assertIn('ready "every node Ready, its container running and not restarting', recipe)


class ImageLoad(unittest.TestCase):
    """image-load.sh with k3d and docker stood in for: the import exits 0
    and agent-1's containerd never holds the image."""

    def setUp(self):
        self.root = tempfile.mkdtemp(prefix="c4b2-load-")
        self.bin = os.path.join(self.root, "bin")
        os.makedirs(self.bin)
        stub(self.bin, "k3d", "echo imported\n")
        stub(self.bin, "sleep", "")
        stub(self.bin, "docker", """
case "$1" in
  ps) printf 'k3d-c-server-0\nk3d-c-agent-1\nk3d-c-serverlb\n' ;;
  image) exit 0 ;;
  exec) echo "IMAGE TAG ID SIZE"; [ "$2" = k3d-c-agent-1 ] || echo "docker.io/library/app 1 x 1MB" ;;
  inspect) echo "running true, restarting false, restarts 3, last exit 137, OOMKilled true, started a, finished b" ;;
  logs) for n in $(seq 1 250); do echo "line $n of $5"; done | tail -n "$4" ;;
esac
""")

    def tearDown(self):
        shutil.rmtree(self.root)

    def test_an_image_that_never_reached_a_node_says_what_docker_says_of_that_node(self):
        env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}")
        done = subprocess.run(["sh", os.path.join(E2E, "image-load.sh"), "k3d", "c", "app:1"],
                              capture_output=True, text=True, env=env)
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("image-load: app:1 never arrived in k3d-c-agent-1", done.stderr)
        self.assertIn("node-state: k3d-c-agent-1: running true, restarting false, restarts 3, last exit 137, "
                      "OOMKilled true", done.stderr)
        self.assertIn("node-state: k3d-c-agent-1, its log's last 200 lines:", done.stderr)
        said = [line for line in done.stderr.splitlines() if line.startswith("    line ")]
        self.assertEqual((len(said), said[-1].strip()), (200, "line 250 of k3d-c-agent-1"))
        self.assertNotIn("node-state: k3d-c-server-0", done.stderr)

    def test_the_watchdog_s_evidence_says_every_node_container(self):
        with open(os.path.join(TOOLS, "ci-watchdog.sh")) as f:
            watchdog = f.read()
        evidence = watchdog[watchdog.index("\n  evidence)"):watchdog.index("\n  beacon)")]
        self.assertIn("nodes_said", evidence)
        self.assertIn("e2e/cluster/node-state.sh\" 200 $bad", watchdog)


class SdkRef(unittest.TestCase):
    """ci-sdk.sh: which meridian-python ref interop reads."""

    def choose(self, *args, pin=None):
        root = tempfile.mkdtemp(prefix="c4b2-sdk-")
        self.addCleanup(shutil.rmtree, root)
        shutil.copy(os.path.join(TOOLS, "ci-sdk.sh"), root)
        if pin is None:
            shutil.copy(os.path.join(TOOLS, "ci-sdk.txt"), root)
        else:
            with open(os.path.join(root, "ci-sdk.txt"), "w") as f:
                f.write("# a comment naming main\n\n" + pin + "\n")
        out = os.path.join(root, "out")
        env = dict(os.environ, GITHUB_OUTPUT=out)
        done = subprocess.run(["sh", os.path.join(root, "ci-sdk.sh"), *args], capture_output=True, text=True, env=env)
        ref = None
        if os.path.exists(out):
            with open(out) as f:
                ref = f.read().strip()
        return done, ref

    def test_the_pin_is_a_release_and_is_read_by_default(self):
        done, ref = self.choose()
        self.assertEqual(done.returncode, 0, done.stdout)
        self.assertRegex(ref, r"^ref=v[0-9]+\.[0-9]+\.[0-9]+$")
        self.assertNotIn("::warning", done.stdout)
        self.assertIn("the release tools/ci-sdk.txt names", done.stdout)

    def test_an_empty_input_reads_the_pin(self):
        done, ref = self.choose("", pin="v1.2.3")
        self.assertEqual((done.returncode, ref), (0, "ref=v1.2.3"), done.stdout)

    def test_a_run_s_input_is_read_and_said_where_it_is_no_release(self):
        done, ref = self.choose("main", pin="v1.2.3")
        self.assertEqual((done.returncode, ref), (0, "ref=main"), done.stdout)
        self.assertIn("::warning title=interop reads no release::", done.stdout)
        self.assertIn("sdk_ref input", done.stdout)

    def test_a_branch_pinned_is_said_on_every_run(self):
        done, ref = self.choose(pin="contract-31")
        self.assertEqual((done.returncode, ref), (0, "ref=contract-31"), done.stdout)
        self.assertIn("::warning", done.stdout)
        self.assertIn("tools/ci-sdk.txt", done.stdout)

    def test_a_ref_with_other_characters_fails(self):
        done, ref = self.choose("main;id")
        self.assertEqual((done.returncode, ref), (1, None), done.stdout)
        self.assertIn("::error::", done.stdout)

    def test_a_pin_file_naming_nothing_fails(self):
        done, ref = self.choose(pin="")
        self.assertEqual((done.returncode, ref), (1, None), done.stdout)

    def test_ci_reads_no_sdk_branch_by_the_pull_request_s_name(self):
        for workflow in ("ci.yaml", "failover-repeat.yaml"):
            with open(os.path.join(TOOLS, "..", ".github", "workflows", workflow)) as f:
                text = f.read()
            self.assertIn("tools/ci-sdk.sh", text, workflow)
            self.assertNotIn("git ls-remote --exit-code --heads \"$SDK\"", text, workflow)


class NightRun(unittest.TestCase):
    """v31a item 6a, the first NIGHT job: failover-repeat runs, by hand,
    the NIGHT classes no profile runs by default (broker-crash-loop), and
    each run that fails ends, as ci's cluster jobs do, with the verdict's
    line naming the failed check. Not every night yet (the 4b-restarts
    review, finding 6): the kubelet's restart backoff puts the fifth crash
    at least 80 s out, past the class's 60 s, so a schedule would be red
    every night until the broker's restart backoff is bounded, which is a
    proposal and not yet a ruled design."""

    def workflow(self):
        with open(os.path.join(TOOLS, "..", ".github", "workflows", "failover-repeat.yaml")) as f:
            return f.read()

    def test_it_runs_the_night_classes_by_hand_and_on_no_schedule_yet(self):
        text = self.workflow()
        triggers = text.split("\non:\n", 1)[1].split("\npermissions:\n", 1)[0]
        self.assertNotRegex(triggers, r"(?m)^  schedule:", "the night run fails every night by construction")
        self.assertRegex(triggers, r"(?m)^  workflow_dispatch:$")
        self.assertIn("\n      failure:\n", triggers)
        self.assertIn("broker-crash-loop", text.split("\non:\n", 1)[0])
        self.assertIn("FAILURE: ${{ needs.failover-images.outputs.failure }}", text)

    def test_a_failed_run_ends_with_the_failed_assertion(self):
        text = self.workflow()
        run_job = text.split("\n  failover-repeat:\n", 1)[1].split("\n  failover-verdict:\n", 1)[0]
        steps = [line.strip()[len("- name: "):] for line in run_job.splitlines()
                 if line.startswith("      - name: ")]
        self.assertEqual(steps[-1], "The failed assertion", steps)
        self.assertIn('tools/ci-watchdog.sh verdict "$RUNNER_TEMP/make.log"', run_job)
        self.assertIn('| tee "$RUNNER_TEMP/make.log"', run_job)
        self.assertIn("id: make", run_job)

    def test_the_broker_s_disk_full_runs_on_a_bounded_filesystem_made_before_the_cluster(self):
        """Fault 1 (ruled 2026-11-27): in ha, when broker-disk-full is asked
        for, a loop-mounted ext4 of 1 GiB, made before e2e-cluster-k3d, which
        mounts it on every node; never otherwise."""
        run_job = self.workflow().split("\n  failover-repeat:\n", 1)[1].split("\n  failover-verdict:\n", 1)[0]
        steps = run_job.split("\n      - ")
        disk = next(step for step in steps if step.startswith("name: The broker's bounded disk"))
        self.assertLess(run_job.index("name: The broker's bounded disk"), run_job.index("name: make e2e-cluster-k3d"))
        self.assertIn("matrix.profile == 'ha'", disk)
        self.assertIn("',broker-disk-full,'", disk)
        for said in ("fallocate -l 1G", "mkfs.ext4 -q -F -m 0", "mount -o loop", 'E2E_K3D_BROKER_DISK=$disk'):
            self.assertIn(said, disk)
        with open(os.path.join(TOOLS, "..", "Makefile")) as f:
            makefile = f.read()
        self.assertIn('--volume $(E2E_K3D_BROKER_DISK):$(E2E_K3D_BROKER_DISK)@server:*;agent:*', makefile)
        self.assertIn("E2E_BROKER_DISK=$(E2E_K3D_BROKER_DISK)", makefile)
        self.assertRegex(makefile, r"(?m)^E2E_K3D_BROKER_DISK \?=$")


if __name__ == "__main__":
    unittest.main()
