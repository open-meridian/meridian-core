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
"""

import os
import socket
import sys
import threading
import unittest

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


if __name__ == "__main__":
    unittest.main(verbosity=2, buffer=True)
