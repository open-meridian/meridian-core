"""The failover harness's own tests, no cluster: `make e2e-harness-check`.

What PR CI 44604574060 (failover, ha) met, held here where it can be made
to happen at will: the rebalance class waited for every Deployment
Available after an upgrade and port-forwarded to the dashboard, onto the
last of its old pods, which the rollout then ended under the Move dialog's
post ("Remote end closed connection without response").

- After an upgrade the run waits for every workload rolled out
  (`rolled_out`), which a Deployment one copy into its rollout is not,
  though it is Available (`all_ready`).
- The admin's client (`Dashboard`) makes its forward anew when it ends
  under a request: a GET is sent again; the Move dialog's post only once
  the store shows no move begun, from a page and token asked again.

What merge-queue run 44726786526 (failover, ha) met: a book entry is stamped
`committed_at_ns` as its command is received, and an order received 0.2 s
before a move's check began was committed 4.3 s later, after the check's
first read -- 7869 entries before the move and 7870 after it, none changed.

- What was committed before a move is cut by where its log stood
  (`committed_cut`), never by a time: a write received before the move and
  committed after the first read is not of it, and an entry changed or lost
  under the move still is.
"""

import hashlib
import os
import socket
import sqlite3
import sys
import threading
import time
import unittest
from unittest import mock

os.environ.setdefault("E2E_IMAGE", "self-test")
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# Appended: e2e/cluster holds a run.py of its own, which runs as it is imported.
sys.path.append(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "cluster"))
import rollout  # noqa: E402
import run  # noqa: E402

DASHBOARD = f"{run.FULL}-dashboard"


class Kubectl:
    """kubectl as the harness calls it, answering from the Deployments
    given: `get ... -o name|json` and `wait --for=condition=Available`,
    which holds while a Deployment's Available condition is True."""

    def __init__(self, deployments):
        self.deployments = deployments

    def __call__(self, *args, input=None, check=True, quiet=False):
        import json
        import subprocess
        said, code = "", 0
        if args[:2] == ("get", "deployments") and "name" in args:
            said = "\n".join(f"deployment.apps/{d['metadata']['name']}" for d in self.deployments)
        elif args[:2] == ("get", "deployments"):
            said = json.dumps({"items": self.deployments})
        elif args[:2] == ("get", "statefulsets"):
            said = json.dumps({"items": []})
        elif args[0] == "wait" and "--for=condition=Available" in args:
            code = 0 if all(any(c["type"] == "Available" and c["status"] == "True"
                                for c in d["status"].get("conditions", [])) for d in self.deployments) else 1
        else:
            raise AssertionError(f"kubectl {' '.join(args)} was not expected")
        return subprocess.CompletedProcess(args, code, said.encode(), b"")


class Server:
    """A dashboard on a local port: each request answered by `answer(method,
    path)`, which gives (status, location, body), or None to close the
    connection with no response, as a forward to a pod that ended does."""

    def __init__(self, answer):
        self.answer, self.asked = answer, []
        self.listening = socket.socket()
        self.listening.bind(("127.0.0.1", 0))
        self.listening.listen(8)
        self.port = self.listening.getsockname()[1]
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self):
        while True:
            try:
                connection, _ = self.listening.accept()
            except OSError:
                return
            with connection:
                raw = b""
                while b"\r\n\r\n" not in raw:
                    chunk = connection.recv(65536)
                    if not chunk:
                        break
                    raw += chunk
                head, _, body = raw.partition(b"\r\n\r\n")
                lines = head.decode().split("\r\n")
                method, path = lines[0].split(" ")[:2]
                length = next((int(line.split(":", 1)[1]) for line in lines[1:]
                               if line.lower().startswith("content-length:")), 0)
                while len(body) < length:
                    body += connection.recv(65536)
                self.asked.append((method, path))
                said = self.answer(method, path)
                if said is None:
                    continue
                status, location, text = said
                response = f"HTTP/1.1 {status} X\r\nContent-Length: {len(text.encode())}\r\nConnection: close\r\n"
                if location:
                    response += f"Location: {location}\r\n"
                connection.sendall((response + "\r\n" + text).encode())

    def close(self):
        self.listening.close()


def dashboard_at(server):
    """Her client over a forward the test stands in for: `forwarded` counts
    the forwards made anew, each answering."""
    dash = object.__new__(run.Dashboard)
    dash.port, dash.base = server.port, f"http://127.0.0.1:{server.port}"
    dash.cookies, dash.access, dash.forward = {}, None, None
    dash.forwards_made = 0

    def forwarded():
        dash.forwards_made += 1
        return True
    dash.forwarded = forwarded
    return dash


def first_closed(then):
    """An answer that closes the first connection with no response, then
    answers each as `then` does."""
    seen = []

    def answer(method, path):
        seen.append(path)
        return None if len(seen) == 1 else then(method, path)
    return answer


PAGE = ('<form method="post" action="/admin/resilience/move-partition"><input type="hidden" '
        'name="form_token" value="token-{}">')


def resilience(closed_posts):
    """The Resilience page with its Move dialog, a token each time it is
    shown; its first `closed_posts` posts closed with no response, as a
    forward to a pod that ended does, the next answered as the store does."""
    shown, posted = [], []

    def answer(method, path):
        if method == "GET":
            shown.append(path)
            return 200, None, PAGE.format(len(shown))
        posted.append(path)
        if len(posted) <= closed_posts:
            return None
        return 303, "/admin/resilience#partitions", ""
    return answer


class RolledOut(unittest.TestCase):
    def setUp(self):
        self.kubectl = run.kubectl

    def tearDown(self):
        run.kubectl = self.kubectl

    def test_a_deployment_one_copy_into_its_rollout_is_available_and_not_rolled_out(self):
        run.kubectl = Kubectl([rollout.deployment(DASHBOARD, 2, 1, 1)])
        self.assertTrue(run.all_ready(), "condition=Available holds mid-rollout: why it is not enough")
        self.assertFalse(run.rolled_out(), "one of two copies on the new template is not rolled out")
        self.assertEqual(run.rollout_left(), [f"{DASHBOARD}: 1 of 2 on the new template"])

    def test_every_copy_on_the_new_template_and_ready_is_rolled_out(self):
        run.kubectl = Kubectl([rollout.deployment(DASHBOARD, 2, 2, 2)])
        self.assertTrue(run.rolled_out())

    def test_the_sharded_instances_standby_is_never_waited_for(self):
        sharded = rollout.deployment(f"{run.FULL}-plugin-{run.SHARDED}", 3, 3, 2)
        run.kubectl = Kubectl([sharded, rollout.deployment(DASHBOARD, 2, 2, 2)])
        self.assertTrue(run.rolled_out(), "its standby pod is never ready, by design")


class ForwardEnds(unittest.TestCase):
    def test_a_get_whose_forward_ends_is_forwarded_anew_and_answered(self):
        server = Server(first_closed(lambda method, path: (200, None, "answered")))
        try:
            dash = dashboard_at(server)
            status, said, _ = dash.send("GET", "/admin/resilience")
            self.assertEqual((status, said), (200, "answered"))
            self.assertEqual(dash.forwards_made, 1)
            self.assertEqual(server.asked, [("GET", "/admin/resilience")] * 2)
        finally:
            server.close()

    def test_a_post_whose_forward_ends_is_not_sent_again_unasked(self):
        server = Server(first_closed(lambda method, path: (200, None, "{}")))
        try:
            dash = dashboard_at(server)
            with self.assertRaises(SystemExit):
                dash.send("POST", "/mcp", body={"method": "tools/call"})
            self.assertEqual(dash.forwards_made, 1, "the forward made anew for the next request")
            self.assertEqual(server.asked, [("POST", "/mcp")], "sent once: it may have acted")
        finally:
            server.close()

    def test_the_move_dialog_posted_again_once_no_move_has_begun(self):
        server = Server(resilience(closed_posts=1))
        asked = []
        try:
            dash = dashboard_at(server)
            status, location = dash.move_dialog("P0", 1, "a note", unbegun=lambda: asked.append(1) or True)
            self.assertEqual((status, location), (303, "/admin/resilience#partitions"))
            self.assertEqual(asked, [1], "the store asked once whether a move began")
            self.assertEqual(dash.forwards_made, 1)
            self.assertEqual([m for m, _ in server.asked], ["GET", "POST", "GET", "POST"],
                             "the page and its token asked again before the second post")
        finally:
            server.close()

    def test_the_move_dialog_never_posted_again_once_a_move_has_begun(self):
        server = Server(resilience(closed_posts=1))
        try:
            dash = dashboard_at(server)
            with self.assertRaises(SystemExit) as stopped:
                dash.move_dialog("P0", 1, "a note", unbegun=lambda: False)
            self.assertIn("not posted again", str(stopped.exception))
            self.assertEqual([m for m, _ in server.asked], ["GET", "POST"], "one post: the move it began stands")
        finally:
            server.close()

    def test_the_move_dialogs_posts_are_capped(self):
        server = Server(resilience(closed_posts=10))
        try:
            dash = dashboard_at(server)
            with self.assertRaises(SystemExit):
                dash.move_dialog("P0", 1, "a note", unbegun=lambda: True)
            posts = [m for m, _ in server.asked].count("POST")
            self.assertEqual(posts, 1 + run.Dashboard.RESENDS)
        finally:
            server.close()


class Store:
    """The book's entries and the lake's rows as the rebalance check reads
    them, in SQLite in memory: `psql` answers a statement as `psql -At`
    does, a row a line and its columns by `|`."""

    def __init__(self):
        self.db = sqlite3.connect(":memory:")
        self.db.create_function(
            "md5", 1, lambda text: None if text is None else hashlib.md5(text.encode()).hexdigest())
        self.db.create_function("encode", 2, lambda held, _: bytes(held).hex())
        self.db.executescript(
            "create table book_entry (entry_id text, account_id text, partition text, first_sequence integer,"
            " last_sequence integer, digest blob, committed_at_ns integer);"
            "create table lake_row (dataset text, partition text, row_key text, sequence integer,"
            " version integer, recorded_at_ns integer);")
        # Committed a minute ago: ten entries of P0, five of them the
        # account's, and ten rows of the range.
        self.then_ns = time.time_ns() - 60 * 10**9
        for n in range(1, 11):
            self.entry(n, "ACC-1" if n % 2 else "ACC-2", self.then_ns + n)
            self.row(n, self.then_ns + n)

    def entry(self, sequence, account, at_ns):
        self.db.execute("insert into book_entry values (?, ?, 'P0', ?, ?, ?, ?)",
                        (f"ENT-{sequence}", account, sequence, sequence, bytes([sequence]), at_ns))

    def row(self, sequence, at_ns):
        self.db.execute("insert into lake_row values ('drive', 'drive/8000-ffff', ?, ?, 1, ?)",
                        (f"key-{sequence}", sequence, at_ns))

    def psql(self, sql, database="meridian", quiet=False, pod=None):
        return "\n".join("|".join("" if value is None else str(value) for value in row)
                         for row in self.db.execute(sql).fetchall())


class Checks:
    """The run's summary, as a move's checks are said to it."""

    def __init__(self):
        self.said = []

    def check(self, ok, what):
        self.said.append((bool(ok), what))
        return ok

    def the_same(self):
        """Whether the move's "reads the same after it" held, and its words."""
        return next((ok, what) for ok, what in self.said if "reads the same after it" in what)


class Admin:
    """The admin's client: a move asked for is answered made, and `under`
    happens in the store while it runs."""

    def __init__(self, under):
        self.under = under

    def call(self, tool, arguments, again=None):
        self.under()
        return {"outcome": "made"}


MOVES = (("book", "partition", "P0", 1), ("book", "account", "ACC-1", "P1"),
         ("lake", "range", "drive/8000-ffff", 1))


def a_move(store, moved, under):
    """One of `MOVES` by the run's own `a_move`, the cluster stood in for:
    the store answers its reads, the move ends moved, and the driver's
    window and every wait are passed over. Says its checks."""
    said = Checks()
    with mock.patch.object(run, "psql", store.psql), \
            mock.patch.object(run, "database_pod", lambda *_: None), \
            mock.patch.object(run, "window_held", lambda *_: None), \
            mock.patch.object(run, "partition_moved", lambda *_: "moved"), \
            mock.patch.object(run.time, "sleep", lambda _: None):
        kind_store, kind, name, target = moved
        run.a_move(said, Admin(under), kind_store, kind, name, target, (0, 0))
    return said


class CommittedBefore(unittest.TestCase):
    def test_a_write_received_before_the_move_and_committed_after_its_first_read_is_not_of_it(self):
        # CI 44726786526: stamped as it was received, before the move's
        # check began, and committed under the move, after the first read.
        for moved in MOVES:
            with self.subTest(moved=moved):
                store = Store()

                def late():
                    received_ns = time.time_ns() - 5 * 10**9
                    store.entry(11, "ACC-1", received_ns)
                    store.row(11, received_ns)
                ok, what = a_move(store, moved, late).the_same()
                self.assertTrue(ok, what)

    def test_an_entry_changed_under_the_move_is_found(self):
        for moved in MOVES:
            with self.subTest(moved=moved):
                store = Store()

                def changed():
                    store.db.execute("update book_entry set digest = x'ff' where first_sequence = 3")
                    store.db.execute("update lake_row set version = 2 where sequence = 3")
                ok, what = a_move(store, moved, changed).the_same()
                self.assertFalse(ok, what)

    def test_an_entry_lost_under_the_move_is_found(self):
        for moved in MOVES:
            with self.subTest(moved=moved):
                store = Store()

                def lost():
                    store.db.execute("delete from book_entry where first_sequence = 3")
                    store.db.execute("delete from lake_row where sequence = 3")
                ok, what = a_move(store, moved, lost).the_same()
                self.assertFalse(ok, what)

    def test_a_move_of_nothing_committed_before_it_is_found(self):
        for moved in MOVES:
            with self.subTest(moved=moved):
                store = Store()
                store.db.execute("delete from book_entry")
                store.db.execute("delete from lake_row")
                ok, what = a_move(store, moved, lambda: None).the_same()
                self.assertFalse(ok, what)


# PR 147's CI, the failover run at the scale back (clock 1795462287.0): what
# each pod's served log held from four seconds before, trimmed to a pod's
# first acknowledgement in each second and the subjects it served in it.
# The pod that stayed served all 32 until the second before, then its 18;
# the pod scaled up served the other 14 from 0.074 s after.
SCALE_BACK_AT = 1795462287.0
SCALE_BACK_SERVED = {
    "shard-proof-ztzs2": ("1", [
        (1795462283.062, range(32)),
        (1795462284.070, range(32)),
        (1795462285.063, range(32)),
        (1795462286.064, (0, 4, 5, 6, 7, 8, 9, 11, 12, 13, 16, 17, 18, 19, 20, 23, 26, 28)),
        (1795462287.074, (0, 4, 5, 6, 7, 8, 9, 11, 12, 13, 16, 17, 18, 19, 20, 23, 26, 28))]),
    "shard-proof-jmsrs": ("0", [
        (1795462287.074, (1, 2, 3, 10, 14, 15, 21, 22, 24, 25, 27, 29, 30, 31))]),
}


class ServedOnce(unittest.TestCase):
    def test_the_wait_after_a_scale_back_waits_for_both_shards_serving(self):
        # The pod that stayed still covers all 32 inside the window when the
        # wait begins: a wait that does not ask for SHARDS pods serving
        # returns at once, and its check then finds one.
        clock = [SCALE_BACK_AT]

        def served_read():
            # Each pod records its subjects once a second: past the
            # trimmed log, each goes on serving its last second's.
            run.SERVED.clear()
            for pod, (shard, seconds) in SCALE_BACK_SERVED.items():
                last, subjects = seconds[-1]
                seconds = seconds + [(last + n, subjects) for n in range(1, 200)]
                run.SERVED[pod] = [(shard, run.SHARD_SUBJECTS[i], int(at), at)
                                   for at, held in seconds if at <= clock[0] for i in held]
            return run.SERVED

        def sleep(seconds):
            clock[0] += seconds
        said = Checks()
        with mock.patch.object(run, "SERVED", {}), \
                mock.patch.object(run, "served_read", served_read), \
                mock.patch.object(run.time, "time", lambda: clock[0]), \
                mock.patch.object(run.time, "sleep", sleep):
            run.served_once(said, "after the scale back")
        self.assertEqual([ok for ok, _ in said.said], [True, True], said.said)


class Unanswered:
    """kubectl with no cluster behind it: every call refused."""

    def __call__(self, *args, input=None, check=True, quiet=False):
        import subprocess
        return subprocess.CompletedProcess(args, 1, b"", b"no cluster in the self-test")


class Driven:
    """The driver's scratch as `driver_lines` reads it: its writes' times
    (`TOOK <kind> <answered ms> <ms taken>`) and its refusals (`REFUSED
    <kind> <id> <refused ms> <ms taken> <words>`), and the run's clock and
    files stood in for, in a directory of its own."""

    def __init__(self, took=(), refused=()):
        self.lines = {"/tmp/took": [line.split() for line in took],
                      "/tmp/refused": [line.split() for line in refused]}

    def __enter__(self):
        import contextlib
        import tempfile
        self.stack = contextlib.ExitStack()
        here = self.stack.enter_context(tempfile.TemporaryDirectory())
        was = os.getcwd()
        os.chdir(here)
        self.stack.callback(os.chdir, was)
        for patch in (mock.patch.object(run, "driver_lines", lambda path: self.lines.get(path, [])),
                      mock.patch.object(run.time, "sleep", lambda _: None),
                      mock.patch.object(run, "claim", lambda *_: None),
                      # No cluster is ever asked: a refusal keeps the
                      # members' logs, by kubectl, which answers nothing.
                      mock.patch.object(run, "kubectl", Unanswered()),
                      mock.patch.object(run, "STORE_LOGS_KEPT", set())):
            self.stack.enter_context(patch)
        return self

    def __exit__(self, *_):
        self.stack.close()


def held(window, took, refused):
    """`window_held` over one window (began, ended ms), the driver's lines
    given: whether its "no write refused" held, and its words."""
    said = Checks()
    with Driven(took, refused):
        run.window_held(said, "a move", *window)
    return next((ok, what) for ok, what in said.said if "no write refused" in what)


class RefusedByWhenItBegan(unittest.TestCase):
    """PR CI 45225932982 (failover, tolerant): an order begun at 17:41:18.69
    timed out at the client 5 s later, never committed, and was charged to
    the next move's window (an account's, begun about 17:41:22.6), though it
    began before it. A write is in a window by when it began, refused or
    answered alike."""

    B = (1_000_000, 1_004_000)

    def test_a_refusal_stamped_in_a_window_whose_write_began_before_it_is_not_of_it(self):
        at = self.B[0] + 1_100
        ok, what = held(self.B, [f"TOOK order {self.B[0] + 500} 20"],
                        [f"REFUSED order 1003172 {at} 5000 status=DEADLINE_EXCEEDED detail=Deadline Exceeded"])
        self.assertTrue(ok, what)

    def test_a_refusal_whose_write_began_in_the_window_is_of_it(self):
        at = self.B[0] + 5_600
        ok, what = held(self.B, [f"TOOK order {self.B[0] + 500} 20"],
                        [f"REFUSED order 1003173 {at} 5000 status=DEADLINE_EXCEEDED detail=Deadline Exceeded"])
        self.assertFalse(ok, what)


def between(windows, refused):
    """`refused_between` over a phase's windows, the driver's refusals
    given: whether its check held, and its words."""
    said = Checks()
    with Driven(refused=refused):
        run.refused_between(said, "a phase", windows)
    return next((ok, what) for ok, what in said.said if "refused between moves" in what)


class RefusedBetweenMoves(unittest.TestCase):
    """A write begun after one move's window ended and before the next
    began, with nothing moving, is refused in steady state: named as such,
    and the class fails, as a refusal inside a window does."""

    A, B = (1_000_000, 1_007_000), (1_010_000, 1_017_000)

    def test_a_write_begun_between_two_windows_and_refused_is_named_and_fails(self):
        # Refused inside B, begun before it: of no window, between the two.
        ok, what = between([self.A, self.B],
                           ["REFUSED order 1003172 1013100 5000 status=DEADLINE_EXCEEDED detail=Deadline Exceeded"])
        self.assertFalse(ok, what)
        self.assertIn("1003172", what)

    def test_a_write_refused_in_a_window_or_outside_the_phase_is_not_between(self):
        ok, what = between([self.A, self.B], [
            "REFUSED order 1 1003000 20 status=UNAVAILABLE",
            "REFUSED order 2 1012000 20 status=UNAVAILABLE",
            "REFUSED order 3 999000 20 status=UNAVAILABLE",
            "REFUSED order 4 1030000 20 status=UNAVAILABLE"])
        self.assertTrue(ok, what)


class Members:
    """kubectl as `keep_store_logs` calls it: the lake's and the book's
    members listed by name, and each one's logs; every call kept."""

    MEMBERS = ("pod/lake-0", "pod/lake-group-1-0", "pod/bor-0", "pod/bor-group-1-0")

    def __init__(self):
        self.calls = []

    def __call__(self, *args, input=None, check=True, quiet=False):
        import subprocess
        self.calls.append(args)
        if args[:2] == ("get", "pods"):
            said = "\n".join(self.MEMBERS)
        elif args[0] == "logs":
            said = f"{args[1]} said its lines"
        else:
            raise AssertionError(f"kubectl {' '.join(args)} was not expected")
        return subprocess.CompletedProcess(args, 0, said.encode(), b"")


class StoreLogsKept(unittest.TestCase):
    """PR CI 45225932982: the rebalance class upgraded and rolled every
    lake and book member for two databases before any log was taken, and
    the book's logs of the refused order were lost with them."""

    def kept(self, then):
        """`then()` run with kubectl stood in for: its calls, and the store
        logs' file as it was left."""
        members = Members()
        with Driven(), mock.patch.object(run, "kubectl", members):
            then()
            said = open(run.STORE_LOGS).read() if os.path.exists(run.STORE_LOGS) else ""
        return members.calls, said

    def test_every_member_s_logs_are_kept_each_call_bounded(self):
        calls, said = self.kept(lambda: run.keep_store_logs("a moment"))
        listed = next(c for c in calls if c[:2] == ("get", "pods"))
        for component in ("lake", "bor", "lake-group-1", "bor-group-1"):
            self.assertIn(component, " ".join(listed))
        for member in Members.MEMBERS:
            self.assertIn(f"{member} said its lines", said)
        self.assertIn("a moment", said)
        for call in calls:
            self.assertTrue(any(a.startswith("--request-timeout=") for a in call), call)
        self.assertTrue(run.STORE_LOGS.startswith(".e2e-"), "the job's failure upload keeps .e2e-*.log")

    def test_a_class_s_first_refused_write_keeps_them_once(self):
        def refused_twice():
            refusal = "REFUSED order 9 1000300 20 status=UNAVAILABLE"
            with mock.patch.object(run, "driver_lines", lambda path: [refusal.split()] if "refused" in path else []):
                run.window_held(Checks(), "a move", 1_000_000, 1_001_000)
                run.refused_between(Checks(), "a phase", [(999_000, 999_500), (1_001_000, 1_002_000)])
        calls, said = self.kept(refused_twice)
        self.assertEqual(sum(1 for c in calls if c[:2] == ("get", "pods")), 1, calls)
        self.assertIn("rebalance: the first refused write", said)

    def test_each_upgrade_of_the_class_keeps_them_first(self):
        order = []
        with mock.patch.object(run, "keep_store_logs", lambda when: order.append(("kept", when))), \
                mock.patch.object(run, "upgrade", lambda *sets: order.append(("upgraded", sets))):
            run.upgrade_kept("lake.databases=2", "bor.databases=2")
        self.assertEqual([what for what, _ in order], ["kept", "upgraded"], order)
        self.assertIn("lake.databases=2", order[0][1])


class Roll:
    """kubectl as `jetstream_rolled` calls it: the broker's roll outlives
    its first `rollout status` (exit 1) and finishes on the next; its
    servers' previous containers have logs. Every call is recorded."""

    def __init__(self):
        self.calls, self.statuses = [], 0

    def __call__(self, *args, input=None, check=True, quiet=False):
        import json
        import subprocess
        self.calls.append(" ".join(args))
        said, code = "", 0
        if args[:2] == ("rollout", "status") and args[2].startswith("statefulset/"):
            self.statuses += 1
            code = 1 if self.statuses == 1 else 0
        elif args[:2] == ("rollout", "status") or args[:2] == ("rollout", "restart"):
            pass
        elif args[:2] == ("get", "pods") and "meridian.dev/component=broker" in args[3]:
            said = json.dumps({"items": [{"metadata": {"name": f"{run.FULL}-broker-{i}"}} for i in range(3)]})
        elif args[:2] == ("get", "pods"):
            said = json.dumps({"items": []})
        elif args[0] == "get" and args[1].startswith("statefulset/"):
            said = json.dumps({"spec": {"replicas": 3}, "status": {
                "readyReplicas": 3, "updatedReplicas": 3, "currentRevision": "b", "updateRevision": "b"}})
        elif args[0] == "logs" and "--previous" in args:
            said = "[1] JetStream cluster not current with the meta leader\n"
        else:
            raise AssertionError(f"kubectl {' '.join(args)} was not expected")
        return subprocess.CompletedProcess(args, code, said.encode(), b"")


class BrokerRolledLate(unittest.TestCase):
    """PR #147's scaled run: the broker's roll outlived its 300 s, the run
    went straight on to the shard section, and the servers' previous logs
    were lost. Now the check fails, then each server's previous lines and
    health are printed and the run waits for the set rolled and ready
    before anything after it."""

    def test_a_roll_past_its_wait_fails_then_says_why_and_waits_for_it_rolled_and_ready(self):
        roll, checks = Roll(), Checks()
        asked = []
        with mock.patch.object(run, "kubectl", roll), \
                mock.patch.object(run, "monitored", lambda server, paths: asked.append(server) or {
                    "/healthz": (503, '{"status":"unavailable"}')}), \
                mock.patch.object(run.time, "sleep", lambda _: None):
            run.jetstream_rolled(checks)
        self.assertIn((False, "broker: its servers restarted one at a time"), checks.said)
        previous = [c for c in roll.calls if c.startswith("logs ") and "--previous" in c]
        self.assertEqual(len(previous), 3)
        self.assertEqual(len(asked), 3)
        statuses = [i for i, c in enumerate(roll.calls) if c.startswith("rollout status statefulset/")]
        self.assertEqual(len(statuses), 2)
        ready = max(i for i, c in enumerate(roll.calls) if c.startswith("get statefulset/"))
        analytics = roll.calls.index(f"rollout restart deployment/{run.FULL}-analytics-coordinator")
        self.assertLess(statuses[1], analytics)
        self.assertLess(ready, analytics)


class NoLossAllowed(unittest.TestCase):
    """Roadmap v31a item 10: the ha whole-run check passed a lake row lost
    after it was acknowledged, as a note (`lake_may_lose`, fault 7). No
    caller may allow loss now: in every profile a lost acknowledged row
    fails the check and names the row."""

    def lost(self, lake_lost):
        return {"book": {"acknowledged": 3, "lost": [], "lost_s": 0, "by_database": {"each": [3], "none": 0}},
                "lake": {"acknowledged": 5, "lost": lake_lost, "lost_s": 0.4,
                         "by_database": {"each": [5 - len(lake_lost)], "none": len(lake_lost)}}}

    def test_no_caller_can_allow_the_lake_to_lose(self):
        import inspect
        self.assertNotIn("lake_may_lose", inspect.signature(run.driver_rpo_held).parameters)

    def test_a_lake_row_lost_in_the_ha_whole_run_fails_and_is_named(self):
        for profile in ("ha", "tolerant"):
            checks = Checks()
            with mock.patch.object(run, "PROFILE", profile):
                run.driver_rpo_held(checks, "the whole run", rpo=self.lost(["price 7"]))
            failed = [what for ok, what in checks.said if not ok]
            self.assertEqual(len(failed), 1, (profile, checks.said))
            self.assertIn("every price the lake acknowledged is in it (1 lost: price 7", failed[0])


class Broker:
    """The broker's one server as the crash loop sees it: each crash
    restarts it; `back` says, by crash, whether the stores answer again."""

    def __init__(self, back=lambda n: True, history=""):
        self.crashes, self.back, self.history, self.rpo = 0, back, history, []

    def patched(self):
        return [
            mock.patch.object(run, "pods", lambda component: [
                {"metadata": {"name": f"{run.FULL}-broker-0"},
                 "status": {"conditions": [{"type": "Ready", "status": "True"}]}}]),
            mock.patch.object(run, "crash", self.crash),
            mock.patch.object(run, "restarts", lambda pod: self.crashes),
            mock.patch.object(run, "answering", lambda component: {"answers": {"ok": True}}
                              if self.back(self.crashes) else None),
            mock.patch.object(run, "acked_since", lambda began_ms: self.back(self.crashes)),
            mock.patch.object(run, "members_settled", lambda component: True),
            mock.patch.object(run, "driver_rpo_held", lambda s, after: self.rpo.append(after)),
            mock.patch.object(run, "driver_file", lambda instance, path: self.history),
            mock.patch.object(run, "claim", lambda *args, **kwargs: None),
            mock.patch.object(run, "DRIVEN", True),
            mock.patch.object(run.time, "sleep", lambda _: None),
        ]

    def crash(self, pod, component):
        assert component == "broker", component
        self.crashes += 1

    def loop(self, checks):
        patches = self.patched()
        for patch in patches:
            patch.start()
        try:
            run.FAILURES["broker-crash-loop"](checks)
        finally:
            for patch in reversed(patches):
                patch.stop()


class BrokerCrashLoop(unittest.TestCase):
    """Roadmap v31a item 6a: the broker's server crashed five times in a
    row, a NIGHT class (failover-repeat's schedule) and no profile's
    default. Each crash held to the tolerant broker bound, 60 s, and then
    no acknowledged write lost and none doubled."""

    def test_a_night_class_in_no_profiles_default(self):
        self.assertIn("broker-crash-loop", run.FAILURES)
        for profile, failures in run.DEFAULT_FAILURES.items():
            self.assertNotIn("broker-crash-loop", failures, profile)
        self.assertIn("broker-crash-loop", run.RPO_IN_CLASS)

    def test_five_crashes_each_back_within_60_s_none_lost_none_doubled(self):
        broker, checks = Broker(history='{:type :ok, :f :txn, :value [[:r "book/ACC-DRIVE-1" [1 2 3]]]}\n'), Checks()
        broker.loop(checks)
        self.assertEqual(broker.crashes, 5)
        self.assertEqual(broker.rpo, [f"broker-crash-loop: crash {n} of 5" for n in range(1, 6)])
        self.assertTrue(all(ok for ok, _ in checks.said), checks.said)
        for n in range(1, 6):
            self.assertTrue(any(f"crash {n} of 5" in what and "within 60s" in what for _, what in checks.said),
                            (n, checks.said))

    def test_a_crash_not_back_within_60_s_fails_naming_it_and_ends_the_loop(self):
        broker, checks = Broker(back=lambda n: n < 4), Checks()
        with mock.patch.object(run, "happens", lambda ready, seconds, every=2.0: ready()):
            broker.loop(checks)
        self.assertEqual(broker.crashes, 4)
        failed = [what for ok, what in checks.said if not ok]
        self.assertEqual(len(failed), 1, checks.said)
        self.assertIn("broker-crash-loop: crash 4 of 5", failed[0])
        self.assertIn("within 60s", failed[0])

    def test_an_order_read_twice_in_its_account_fails_naming_it(self):
        history = ('{:type :ok, :f :txn, :value [[:r "book/ACC-DRIVE-1" [1 2 2 3]]], :process 0}\n'
                   '{:type :ok, :f :txn, :value [[:r "book/ACC-DRIVE-2" [4 5]]], :process 0}\n'
                   '{:type :invoke, :f :txn, :value [[:r "book/ACC-DRIVE-3" nil]], :process 0}\n')
        self.assertEqual(run.doubled(history), ["book/ACC-DRIVE-1: 2"])
        broker, checks = Broker(history=history), Checks()
        broker.loop(checks)
        failed = [what for ok, what in checks.said if not ok]
        self.assertEqual(len(failed), 5, checks.said)
        self.assertIn("book/ACC-DRIVE-1: 2", failed[0])


if __name__ == "__main__":
    unittest.main(verbosity=2, buffer=True)
