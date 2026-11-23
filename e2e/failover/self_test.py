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


if __name__ == "__main__":
    unittest.main(verbosity=2, buffer=True)
