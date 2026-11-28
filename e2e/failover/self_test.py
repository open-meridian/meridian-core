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
        self.notes = []

    def check(self, ok, what):
        self.said.append((bool(ok), what))
        return ok

    def note(self, what):
        self.notes.append(what)

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
                mock.patch.object(run, "follow_store_logs", lambda when: order.append(("followed", when))), \
                mock.patch.object(run, "upgrade", lambda *sets: order.append(("upgraded", sets))):
            run.upgrade_kept("lake.databases=2", "bor.databases=2")
        self.assertEqual([what for what, _ in order], ["kept", "followed", "upgraded"], order)
        self.assertIn("lake.databases=2", order[0][1])


class Taken:
    """psql as `takeovers_since` calls it, per database pod: how many
    takeovers each store's database recorded since, and each one."""

    ROWS = {("book", "db-0"): "2|9 bor-0 1.5 assigned; 10 bor-group-1-0 2.0 assigned",
            ("book", "db-1"): "1|11 bor-old-0 3.1 assigned",
            ("lake", "db-0"): "0|",
            ("lake", "db-1"): "1|7 lake-group-1-0 0.4 lease free"}

    def __init__(self, failing=False):
        self.failing, self.asked = failing, []

    def __call__(self, sql, database="meridian", quiet=False, pod=None):
        self.asked.append((sql, pod))
        if self.failing:
            raise SystemExit("psql: no answer")
        store = "book" if "book_writer_took_over" in sql else "lake"
        return self.ROWS[(store, pod)]


class RaiseEvidence(unittest.TestCase):
    """Merge-queue run 46082830508 ("rollout-ha-groups-raised (bor)", 11.9 s
    against 10 s): the book's writers took its partitions from each other
    through a writer-group raise, and nothing the run kept said who. The
    record does (`book_writer_took_over`, `lake_writer_took_over`); the old
    members' logs are gone with the rollout before `rollout_served` reads
    (the CTO's check of 2026-11-27)."""

    def served(self, bor_held=True, failing=False):
        """`rollout_served` of one rollout, the book's claim held or not:
        what it printed, the members' logs it kept, the figures, and the
        databases asked."""
        import contextlib
        import io
        kept, taken, printed = [], Taken(failing), io.StringIO()
        with Driven(), \
                mock.patch.object(run, "claim", lambda s, failure, component, *_: bor_held or component != "bor"), \
                mock.patch.object(run, "store_databases", lambda store: [(0, "db-0"), (1, "db-1")]), \
                mock.patch.object(run, "psql", taken), \
                mock.patch.object(run, "keep_store_logs", kept.append), \
                contextlib.redirect_stdout(printed):
            run.rollout_served(Checks(), "ha-groups-raised", 1_000, 2, False)
            figures = open(run.FIGURES).read() if os.path.exists(run.FIGURES) else ""
        return printed.getvalue(), kept, figures, taken.asked

    def test_a_failed_rollout_claim_prints_every_takeover_since_it_began_in_every_database(self):
        said, kept, _, asked = self.served(bor_held=False)
        for row in Taken.ROWS.values():
            for took in row.split("|", 1)[1].split("; "):
                if took:
                    self.assertIn(took, said)
        self.assertIn("the book's takeovers in database 1", said)
        self.assertIn("the lake's takeovers in database 0 (epoch, member, seconds from the rollout's start, why): "
                      "none", said)
        self.assertTrue(all("at_ns >= 1000000000" in sql for sql, _ in asked), asked)

    def test_a_failed_rollout_claim_keeps_the_surviving_members_logs(self):
        _, kept, _, _ = self.served(bor_held=False)
        self.assertEqual(kept, ["rollout-ha-groups-raised: bor past its bound"])

    def test_the_takeover_count_is_printed_and_a_figure_on_every_run(self):
        said, kept, figures, _ = self.served(bor_held=True)
        self.assertIn("the book's takeovers since the rollout began: 3, over 2 database(s)", said)
        self.assertIn("the lake's takeovers since the rollout began: 1, over 2 database(s)", said)
        self.assertIn('"took_over": 3', figures)
        self.assertNotIn("takeovers in database", said, "the rows printed only for a claim missed")
        self.assertEqual(kept, [])

    def test_a_takeover_read_that_fails_is_said_and_never_ends_the_run(self):
        said, kept, _, _ = self.served(bor_held=False, failing=True)
        self.assertIn("the book's takeovers since the rollout began were not read: psql: no answer", said)
        self.assertEqual(kept, ["rollout-ha-groups-raised: bor past its bound"])


class Following:
    """`follow_logs_of` stood in for: each pod's follower writes its lines
    and ends at once (its pod gone), runs past its bound, or runs until it
    is killed."""

    def __init__(self, ends="at once"):
        self.ends, self.started, self.killed = ends, [], []

    def __call__(self, name, sink):
        sink.write(f"{name} said its last lines\n".encode())
        sink.flush()
        self.started.append(name)
        return Follower(self, name)


class Follower:
    def __init__(self, following, name):
        self.following, self.name, self.returncode = following, name, None
        self.gone = threading.Event()

    def poll(self):
        return self.returncode

    def wait(self, timeout=None):
        import subprocess
        if self.gone.is_set():
            return self.returncode
        if self.following.ends == "at once":
            self.returncode = 0
            return 0
        if self.following.ends == "past its bound":
            raise subprocess.TimeoutExpired("kubectl", timeout)
        self.gone.wait()
        return self.returncode

    def kill(self):
        self.returncode = -9
        self.following.killed.append(self.name)
        self.gone.set()


class OldMembersFollowed(unittest.TestCase):
    """The same run: an upgrade that changes a count rolls the old members
    out, and their logs with them, before anything reads after it. Each
    member listed at its start is followed until its pod ends."""

    def followed(self, then, ends="at once"):
        """`then()` with kubectl and the followers stood in for: the
        followers, and the followed logs' file once every one has ended."""
        following = Following(ends)
        with Driven(), mock.patch.object(run, "kubectl", Members()), \
                mock.patch.object(run, "follow_logs_of", following), \
                mock.patch.object(run, "FOLLOWERS", []):
            then()
            if ends == "never":
                happened = run.happens(lambda: len(following.started) == len(Members.MEMBERS), 10, every=0.01)
                self.assertTrue(happened, following.started)
            run.stop_following()
            said = open(run.FOLLOWED_LOGS).read() if os.path.exists(run.FOLLOWED_LOGS) else ""
        return following, said

    def test_only_an_upgrade_that_changes_a_count_is_followed(self):
        for sets in (["lake.writerGroups=2", "bor.writerGroups=2"], ["lake.databases=2"], ["bor.partitions=4"]):
            self.assertTrue(run.changes_a_count(sets), sets)
        self.assertFalse(run.changes_a_count([f"lake.scrub.intervalDays={run.SCRUB_DAYS}"]))

    def test_each_member_is_followed_from_before_the_upgrade_until_its_pod_ends(self):
        order = []

        def upgraded():
            with mock.patch.object(run, "keep_store_logs", lambda when: order.append("kept")), \
                    mock.patch.object(run, "upgrade", lambda *sets: order.append(("upgraded", len(run.FOLLOWERS)))):
                run.upgrade_kept("lake.databases=2", "bor.databases=2")
        following, said = self.followed(upgraded)
        self.assertEqual([step if step == "kept" else step[0] for step in order], ["kept", "upgraded"], order)
        self.assertGreaterEqual(order[1][1], len(Members.MEMBERS), "each followed before the upgrade began")
        for member in Members.MEMBERS:
            self.assertIn(f"{member} said its last lines", said)
            self.assertIn(f"{member}, followed from", said)
        self.assertIn("(its follower ended, status 0)", said)
        self.assertIn("the upgrade to lake.databases=2 bor.databases=2", said)
        self.assertTrue(run.FOLLOWED_LOGS.startswith(".e2e-"), "the job's failure upload keeps .e2e-*.log")

    def test_a_writer_group_raise_is_followed_before_its_upgrade_and_a_scrub_change_is_not(self):
        for sets, follows in ((("lake.writerGroups=2", "bor.writerGroups=2"), True),
                              ((f"lake.scrub.intervalDays={run.SCRUB_DAYS}",), False)):
            order = []
            with mock.patch.object(run, "record_now", lambda: {}), \
                    mock.patch.object(run, "follow_store_logs", lambda when: order.append("followed")), \
                    mock.patch.object(run, "upgrade", lambda *sets: order.append("upgraded") or
                                      __import__("subprocess").CompletedProcess(sets, 1, b"", b"refused")):
                self.assertFalse(run.groups_upgraded(Checks(), "a raise: two groups", 2, False, *sets))
            self.assertEqual(order, ["followed", "upgraded"] if follows else ["upgraded"], sets)

    def test_a_follower_past_its_bound_is_ended_and_its_logs_kept(self):
        following, said = self.followed(lambda: run.follow_store_logs("a raise"), ends="past its bound")
        self.assertEqual(sorted(following.killed), sorted(Members.MEMBERS))
        self.assertIn(f"(its bound of {run.FOLLOW_SECONDS} s)", said)
        self.assertIn("pod/bor-group-1-0 said its last lines", said)

    def test_a_follower_still_running_at_the_runs_end_is_ended_and_its_logs_kept(self):
        following, said = self.followed(lambda: run.follow_store_logs("a raise"), ends="never")
        self.assertEqual(sorted(following.killed), sorted(Members.MEMBERS))
        self.assertIn("(its follower ended, status -9)", said)
        for member in Members.MEMBERS:
            self.assertIn(f"{member} said its last lines", said)

    def test_members_not_listed_are_said_in_the_file(self):
        with Driven():
            self.assertEqual(run.follow_store_logs("a raise"), 0)
            said = open(run.FOLLOWED_LOGS).read()
        self.assertIn("a raise: the members were not listed", said)
        self.assertIn("no cluster in the self-test", said)


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
        self.claims = []

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
            mock.patch.object(run, "claim", lambda s, *said: self.claims.append(said)),
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
    row, a NIGHT class (failover-repeat, by hand) and no profile's
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

    def test_a_crash_not_back_within_60_s_is_claimed_as_not_held(self):
        """The 4b-restarts review, finding 6: the crash that missed its
        bound left no figure, so the ledger held only the crashes that made
        it. It is claimed too: past its bound, and said not recovered."""
        broker, checks, now = Broker(back=lambda n: n < 4), Checks(), [1000.0]

        def happens(ready, seconds, every=2.0):
            # Given up only once the whole wait has passed.
            if not ready():
                now[0] += seconds + 0.5
                return False
            return True
        with mock.patch.object(run, "happens", happens), mock.patch.object(run.time, "time", lambda: now[0]):
            broker.loop(checks)
        self.assertEqual([said[4]["crash"] for said in broker.claims], [1, 2, 3, 4], broker.claims)
        failure, component, seconds, bound, extra = broker.claims[-1]
        self.assertEqual((failure, component, bound), ("broker-crash-loop", "bor", 60))
        self.assertGreater(seconds, bound)
        self.assertIs(extra["recovered"], False)
        self.assertTrue(all("recovered" not in said[4] for said in broker.claims[:-1]), broker.claims)

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


class BrokerDiskFull(unittest.TestCase):
    """Fault 1 (ruled 2026-11-27): the broker's disk full, every JetStream
    server's store on one bounded filesystem, a NIGHT class in ha alone
    (failover-repeat, by hand) and no profile's default."""

    def test_a_night_class_in_no_profiles_default(self):
        self.assertIn("broker-disk-full", run.FAILURES)
        for profile, failures in run.DEFAULT_FAILURES.items():
            self.assertNotIn("broker-disk-full", failures, profile)
        self.assertIn("broker-disk-full", run.RPO_IN_CLASS)

    def test_asked_for_by_name_in_ha_alone(self):
        for profile, failure, asked in (("ha", "broker-disk-full", True), ("ha", "zone,broker-disk-full", True),
                                        ("ha", "", False), ("ha", "write-ahead-bound", False),
                                        ("tolerant", "broker-disk-full", False)):
            with mock.patch.object(run, "PROFILE", profile), mock.patch.dict(os.environ, {"FAILURE": failure}):
                self.assertIs(run.broker_disk_asked(), asked, (profile, failure))

    def mounted(self, said, total=1_000_000):
        checks = Checks()
        with mock.patch.object(run, "jetstream_servers", lambda: list(said)), \
                mock.patch.object(run, "disk_of", lambda pod: said[pod]), \
                mock.patch.object(run, "host_disk", lambda: (total, total // 2)):
            run.broker_disk_mounted(checks)
        return [(ok, what) for ok, what in checks.said if "stores on the bounded filesystem" in what]

    def test_every_server_s_store_on_the_bounded_filesystem_holds(self):
        said = {f"js-{n}": (1_000_000, 400_000, "/dev/loop3") for n in range(3)}
        self.assertEqual([ok for ok, _ in self.mounted(said)], [True])

    def test_a_server_s_store_on_another_filesystem_fails(self):
        said = {"js-0": (1_000_000, 1, "/dev/loop3"), "js-1": (1_000_000, 1, "/dev/loop3"),
                "js-2": (72_000_000_000, 1, "overlay")}
        self.assertEqual([ok for ok, _ in self.mounted(said)], [False])
        said["js-2"] = None
        self.assertEqual([ok for ok, _ in self.mounted(said)], [False])

    def test_fewer_than_three_servers_fails(self):
        said = {f"js-{n}": (1_000_000, 1, "/dev/loop3") for n in range(2)}
        self.assertEqual([ok for ok, _ in self.mounted(said)], [False])

    def test_each_server_s_volume_bound_ahead_to_its_claim_on_the_filesystem(self):
        import json
        import tempfile
        applied = []
        with tempfile.TemporaryDirectory() as disk, mock.patch.object(run, "BROKER_DISK", disk), \
                mock.patch.object(run, "SHAPE", "ha"), \
                mock.patch.object(run, "run", lambda *command, input=None, **_: applied.append((command, input))):
            run.broker_disk_volumes(3)
            for n in range(3):
                self.assertEqual(os.stat(os.path.join(disk, f"jetstream-{n}")).st_mode & 0o777, 0o777)
        (command, body), = applied
        self.assertEqual(command, ("kubectl", "apply", "-f", "-"))
        volumes = json.loads(body)["items"]
        self.assertEqual([v["spec"]["claimRef"]["name"] for v in volumes],
                         [f"jetstream-{run.FULL}-broker-jetstream-{n}" for n in range(3)])
        self.assertTrue(all(v["spec"]["storageClassName"] == run.BROKER_DISK_CLASS for v in volumes))
        self.assertTrue(all(v["spec"]["hostPath"]["path"].startswith(disk) for v in volumes))


class DiskFull:
    """The class's world, its clock stood in for: the broker refuses a
    price `refused_after` s into the fault, the notice is filed
    `notice_after` s after the first refusal (None: never), the lake
    acknowledges again `back_after` s after the room is given (None:
    never); the prices lost, the gaps and what the broker said given."""

    def __init__(self, refused_after=4, notice_after=50, back_after=20, lost=(), gaps=(), said=None,
                 made_again=False, raise_in_fill=False):
        self.now = [1_000_000.0]
        self.refused_after, self.notice_after, self.back_after = refused_after, notice_after, back_after
        self.lost, self.gaps, self.said, self.made_again = list(lost), list(gaps), said or {}, made_again
        self.raise_in_fill, self.rooms, self.claims = raise_in_fill, 0, []
        self.start = self.now[0]

    def refused(self, began_ms):
        at = self.start + self.refused_after
        return [(int(at * 1000), "UNAVAILABLE: no space")] if self.now[0] >= at else []

    def notices(self, since_ns):
        if self.notice_after is None:
            return []
        at = self.start + self.refused_after + self.notice_after
        return [int(at * 1e9)] if self.now[0] >= at else []

    def acked_after(self, at_ms):
        if self.back_after is None or self.room_at is None:
            return None
        at = self.room_at + self.back_after
        return int(at * 1000) if self.now[0] >= at else None

    def fill(self):
        if self.raise_in_fill:
            raise SystemExit("kubectl exec failed")
        return 1 << 30

    def give_room(self):
        self.rooms += 1
        self.room_at = self.now[0]

    def run(self):
        self.room_at = None
        checks = Checks()
        acked = {f"price {n}": int((self.start + n) * 1000) for n in range(1, 30)}
        patches = [
            mock.patch.object(run, "PROFILE", "ha"), mock.patch.object(run, "BROKER_DISK", "/disk"),
            mock.patch.object(run, "DRIVEN", True),
            mock.patch.object(run, "broker_disk_mounted", lambda s: True),
            mock.patch.object(run, "wait_for", lambda *a, **k: None),
            mock.patch.object(run, "fill_disk", self.fill), mock.patch.object(run, "give_room", self.give_room),
            mock.patch.object(run, "host_disk", lambda: (1 << 30, 0)),
            mock.patch.object(run, "prices_refused", self.refused),
            mock.patch.object(run, "stuck_notices", self.notices),
            mock.patch.object(run, "price_acked_after", self.acked_after),
            mock.patch.object(run, "store_logs_at_first_refusal", lambda *a: None),
            mock.patch.object(run, "broker_disk_said", lambda began: self.said),
            mock.patch.object(run, "lake_said_since", lambda began: {
                "lake-0": run.MADE_AGAIN if self.made_again else ""}),
            mock.patch.object(run, "gap_windows", lambda dataset, since: self.gaps),
            mock.patch.object(run, "gaps_found_in", lambda dataset, since: {}),
            mock.patch.object(run, "store_rpo", lambda: {"book": {"lost": []}, "lake": {"lost": self.lost}}),
            mock.patch.object(run, "driver_acked", lambda: {"price": acked}),
            mock.patch.object(run, "claim", lambda s, *said: self.claims.append(said)),
            mock.patch.object(run, "figure", lambda *a, **k: None),
            mock.patch.object(run.time, "time", lambda: self.now[0]),
            mock.patch.object(run.time, "sleep", lambda seconds: self.now.__setitem__(0, self.now[0] + seconds)),
        ]
        for patch in patches:
            patch.start()
        try:
            run.FAILURES["broker-disk-full"](checks)
        except SystemExit:
            pass
        finally:
            for patch in reversed(patches):
                patch.stop()
        return checks

    def failed(self, checks):
        return [what for ok, what in checks.said if not ok]


class BrokerDiskFullAsserted(unittest.TestCase):
    """The class's assertions (the spec's row, its bounds candidates): the
    standing notice within 2 minutes of the first refusal, the lake's
    writes acknowledged again within 2 minutes of the room given back, no
    acknowledged price missing outside a marked gap, and a stream deleted
    as orphaned coming out as a gap."""

    def test_all_held_is_green_and_the_room_given_back_once(self):
        world = DiskFull()
        checks = world.run()
        self.assertEqual(world.failed(checks), [])
        self.assertEqual(world.rooms, 1)
        (failure, component, seconds, bound, extra), = world.claims
        self.assertEqual((failure, component, bound), ("broker-disk-full", "lake", 120))
        self.assertLessEqual(seconds, 120)
        self.assertGreaterEqual(extra["held_s"], 90)

    def test_no_notice_fails_naming_its_bound(self):
        world = DiskFull(notice_after=None)
        failed = world.failed(world.run())
        self.assertEqual(len(failed), 1, failed)
        self.assertIn("the standing notice (component-write-ahead-stuck-full) within 120s", failed[0])
        self.assertIn("none within", failed[0])

    def test_a_late_notice_fails_and_says_how_late(self):
        failed = DiskFull(notice_after=150).failed(DiskFull(notice_after=150).run())
        self.assertEqual(len(failed), 1, failed)
        self.assertRegex(failed[0], r"\(15\d(\.\d)?s\)")

    def test_writes_never_acknowledged_again_are_claimed_past_the_bound(self):
        world = DiskFull(back_after=None)
        world.run()
        (_, _, seconds, bound, extra), = world.claims
        self.assertGreater(seconds, bound)
        self.assertIs(extra["acknowledged_again"], False)

    def test_how_long_the_lake_took_to_acknowledge_again_is_said(self):
        world = DiskFull(back_after=211)
        checks = world.run()
        self.assertTrue(any("acknowledged a price again 21" in note and "after the room given back" in note
                            for note in checks.notes), checks.notes)

    def test_a_lost_price_inside_a_marked_gap_holds_and_outside_fails(self):
        # Acknowledged at the start + n s; the gap marks 20 s to 30 s in.
        gap = [(int((1_000_000 + 20) * 1e9), int((1_000_000 + 30) * 1e9))]
        inside = DiskFull(lost=["price 25"], gaps=gap)
        self.assertEqual(inside.failed(inside.run()), [])
        outside = DiskFull(lost=["price 5"], gaps=gap)
        failed = outside.failed(outside.run())
        self.assertEqual(len(failed), 1, failed)
        self.assertIn("1 of them in no gap: price 5", failed[0])

    def test_a_stream_deleted_as_orphaned_without_a_gap_fails(self):
        said = {"js-0": ["[WRN] Detected orphaned stream '$G > LAKE_WRITE_AHEAD', will cleanup"]}
        for world in (DiskFull(said=said), DiskFull(made_again=True)):
            failed = world.failed(world.run())
            self.assertEqual(len(failed), 1, failed)
            self.assertIn("deleted as orphaned", failed[0])
        held = DiskFull(said=said, gaps=[(1, 2)])
        self.assertEqual(held.failed(held.run()), [])

    def test_the_room_is_given_back_whatever_stops_the_fill(self):
        world = DiskFull(raise_in_fill=True)
        world.run()
        self.assertEqual(world.rooms, 1)

    def test_a_price_outside_every_gap_s_span_is_named(self):
        acked = {"a": 100_000, "b": 200_000}
        windows = [(99_000 * 10**6, 99_500 * 10**6)]
        self.assertEqual(run.outside_gaps(["a", "b"], acked, windows), ["b"])


if __name__ == "__main__":
    unittest.main(verbosity=2, buffer=True)
