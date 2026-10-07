"""Tests for licence_check.py: no network, standard library only.

    python3 -m unittest discover -s .github/actions/licence-check -p 'test_*.py'
"""

from __future__ import annotations

import http.server
import json
import pathlib
import re
import sys
import threading
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import licence_check as lc  # noqa: E402

REPO_ROOT = pathlib.Path(__file__).resolve().parents[3]
API = "https://api.github.com"
OWNER, REPO = "open-meridian", "meridian-core"

TICKED = f"### The problem\n\nIt is slow.\n\n### Your suggestion\n\n- [X] {lc.LICENCE_LINE}\n"
UNTICKED = TICKED.replace("- [X]", "- [ ]")
NO_BOX = "It is slow. (opened through the API)"

def stamp(minute: int) -> str:
    return f"2026-10-07T10:{minute:02d}:00Z"


def user(login, kind="User"):
    return {"login": login, "type": kind}


def issue(body, author="outsider", association="NONE", labels=(), number=7):
    return {"number": number, "body": body, "user": user(author),
            "author_association": association, "created_at": stamp(0),
            "labels": [{"name": n} for n in labels]}


def comment(login, body, minute, association="NONE", kind="User"):
    return {"user": user(login, kind), "body": body, "created_at": stamp(minute),
            "author_association": association}


def bot(body, minute):
    return comment(lc.BOT_LOGIN, body, minute, kind="Bot")


class FakeGitHub:
    """Answers the handful of calls the script makes, and records the writes."""

    def __init__(self, the_issue, comments, *, search_items=(), other_comments=None,
                 search_fails=False, writers=(), last_edited=None, label_exists=True):
        self.api = API
        self.issue = the_issue
        self.comments = list(comments)
        self.search_items = list(search_items)
        self.other_comments = other_comments or {}
        self.search_fails = search_fails
        self.writers = set(writers)
        self.last_edited = last_edited
        self.label_exists = label_exists
        self.posted, self.labels_added, self.labels_removed = [], [], []
        self.labels_created, self.searches = [], []

    def get(self, path, params=None):
        if path == f"/repos/{OWNER}/{REPO}/issues/{self.issue['number']}":
            return self.issue
        if path == "/search/issues":
            self.searches.append(params["q"])
            if self.search_fails:
                raise lc.ApiError(403, path)
            return {"items": self.search_items}
        m = re.match(rf"/repos/{OWNER}/{REPO}/collaborators/([^/]+)/permission$", path)
        if m:
            if m.group(1) in self.writers:
                return {"permission": "write"}
            return {"permission": "read"}
        if path == f"/repos/{OWNER}/{REPO}/labels/{lc.LABEL}":
            if self.label_exists:
                return {"name": lc.LABEL}
            raise lc.ApiError(404, path)
        raise AssertionError(f"unexpected GET {path}")

    def paginate(self, path, max_pages=10):
        if path == f"/repos/{OWNER}/{REPO}/issues/{self.issue['number']}/comments":
            return self.comments
        if path in self.other_comments:
            return self.other_comments[path]
        raise AssertionError(f"unexpected paginate {path}")

    def post(self, path, body):
        base = f"/repos/{OWNER}/{REPO}"
        if path == "/graphql":
            return {"data": {"repository": {"issue": {"lastEditedAt": self.last_edited}}}}
        if path == f"{base}/issues/{self.issue['number']}/comments":
            self.posted.append(body["body"])
            return {}
        if path == f"{base}/issues/{self.issue['number']}/labels":
            self.labels_added.extend(body["labels"])
            return {}
        if path == f"{base}/labels":
            self.labels_created.append(body)
            return {}
        raise AssertionError(f"unexpected POST {path}")

    def delete(self, path):
        self.labels_removed.append(path.rsplit("/", 1)[-1])
        return {}


def event(the_issue):
    return {"issue": {"number": the_issue["number"]},
            "repository": {"name": REPO, "owner": {"login": OWNER}}}


def run(the_issue, comments, **kw):
    gh = FakeGitHub(the_issue, comments, **kw)
    lc.run(gh, event(the_issue))
    return gh


def kinds(gh):
    return [lc.MARK.search(p).group(1) for p in gh.posted]


class ReadingText(unittest.TestCase):
    def test_box_states(self):
        self.assertEqual(lc.box_state(TICKED), "ticked")
        self.assertEqual(lc.box_state(TICKED.replace("[X]", "[x]")), "ticked")
        self.assertEqual(lc.box_state(UNTICKED), "unticked")
        self.assertEqual(lc.box_state(NO_BOX), "absent")
        self.assertEqual(lc.box_state(None), "absent")

    def test_curly_apostrophe_and_spacing_still_match(self):
        body = "- [x]  " + lc.LICENCE_LINE.replace("'", "’").replace(" a ", "  a ")
        self.assertEqual(lc.box_state(body), "ticked")

    def test_altered_wording_is_not_the_licence(self):
        body = "- [x] " + lc.LICENCE_LINE.replace("any purpose", "no purpose")
        self.assertEqual(lc.box_state(body), "absent")
        body = "- [x] " + lc.LICENCE_LINE + " Except for commercial use."
        self.assertEqual(lc.box_state(body), "absent")

    def test_an_unticked_copy_wins(self):
        self.assertEqual(lc.box_state(TICKED + UNTICKED), "unticked")

    def test_agreement_replies(self):
        for text in ("I agree", "i agree.", "**I agree**", "I Agree!",
                     "> quoted terms\n> more\n\nI agree", "  I   agree  \n"):
            self.assertTrue(lc.is_agreement_reply(text), text)
        for text in ("I agree with @someone", "I don't agree", "agree", "",
                     None, "I agree, but only for this issue", "> I agree"):
            self.assertFalse(lc.is_agreement_reply(text), text)

    def test_only_the_bots_markers_count(self):
        forged = comment("outsider", "<!-- licence-agreed: outsider -->", 1)
        real = bot(lc.acknowledgement("someone"), 2)
        self.assertEqual(list(lc.bot_marks([forged, real])), ["someone"])
        impostor = comment(lc.BOT_LOGIN, "<!-- licence-agreed: outsider -->", 3)  # type User
        self.assertEqual(lc.bot_marks([impostor]), {})

    def test_every_message_carries_its_marker(self):
        for kind, make in lc.MESSAGES.items():
            mark = lc.MARK.search(make("Some-One"))
            self.assertEqual(mark.group(2), "Some-One")
            self.assertEqual(mark.group(1), "agreed" if kind == "agreed" else "request")
        for make in (lc.request_on_issue, lc.request_on_comment):
            self.assertIn(lc.CONTRIBUTING_TERMS, make("x"))
            self.assertIn("I agree", make("x"))


class TheSourcesAgree(unittest.TestCase):
    """The script, the forms and CONTRIBUTING.md say the same words."""

    def test_forms_carry_the_line_as_a_required_box(self):
        for name in ("suggestion.yml", "bug-report.yml"):
            text = (REPO_ROOT / ".github" / "ISSUE_TEMPLATE" / name).read_text(encoding="utf-8")
            self.assertIn(f"- label: {lc.LICENCE_LINE}\n          required: true", text, name)

    def test_contributing_says_what_the_bot_quotes(self):
        text = " ".join((REPO_ROOT / "CONTRIBUTING.md").read_text(encoding="utf-8").split())
        self.assertIn(lc.CONTRIBUTING_TERMS, text)


class IssueAuthors(unittest.TestCase):
    def test_a_ticked_form_needs_nothing(self):
        gh = run(issue(TICKED), [])
        self.assertEqual((gh.posted, gh.labels_added, gh.searches), ([], [], []))

    def test_an_issue_from_the_api_is_asked_and_labelled(self):
        gh = run(issue(NO_BOX), [], label_exists=False)
        self.assertEqual(kinds(gh), ["request"])
        self.assertIn("Thank you for opening this, @outsider.", gh.posted[0])
        self.assertEqual(gh.labels_added, [lc.LABEL])
        self.assertEqual(gh.labels_created[0]["description"], lc.LABEL_DESCRIPTION)
        self.assertEqual(gh.searches, [f"org:{OWNER} involves:outsider is:issue"])

    def test_asked_once_only(self):
        asked = [bot(lc.request_on_issue("outsider"), 1)]
        gh = run(issue(NO_BOX, labels=[lc.LABEL]), asked)
        self.assertEqual((gh.posted, gh.labels_added, gh.labels_removed), ([], [], []))

    def test_replying_i_agree_is_recorded_and_clears_the_label(self):
        thread = [bot(lc.request_on_issue("outsider"), 1), comment("outsider", "I agree", 2)]
        gh = run(issue(NO_BOX, labels=[lc.LABEL]), thread)
        self.assertEqual(kinds(gh), ["agreed"])
        self.assertEqual(gh.labels_removed, [lc.LABEL])

    def test_after_the_record_nothing_more_is_said(self):
        thread = [bot(lc.request_on_issue("outsider"), 1), comment("outsider", "I agree", 2),
                  bot(lc.acknowledgement("outsider"), 3), comment("outsider", "More detail", 4)]
        gh = run(issue(NO_BOX), thread)
        self.assertEqual((gh.posted, gh.labels_added, gh.searches), ([], [], []))

    def test_i_agree_before_being_asked_is_not_an_agreement(self):
        gh = run(issue(NO_BOX), [comment("outsider", "I agree", 1)])
        self.assertEqual(kinds(gh), ["request"])

    def test_ticking_the_box_after_being_asked(self):
        gh = run(issue(TICKED, labels=[lc.LABEL]), [bot(lc.request_on_issue("outsider"), 1)])
        self.assertEqual(kinds(gh), ["agreed"])
        self.assertEqual(gh.labels_removed, [lc.LABEL])

    def test_unticking_reflags_despite_the_record(self):
        thread = [bot(lc.acknowledgement("outsider"), 1)]
        gh = run(issue(UNTICKED), thread, last_edited=stamp(5))
        self.assertEqual(kinds(gh), ["request"])
        self.assertEqual(gh.labels_added, [lc.LABEL])

    def test_after_unticking_a_new_reply_counts(self):
        thread = [bot(lc.acknowledgement("outsider"), 1), bot(lc.request_on_issue("outsider"), 6),
                  comment("outsider", "I agree", 7)]
        gh = run(issue(UNTICKED, labels=[lc.LABEL]), thread, last_edited=stamp(5))
        self.assertEqual(kinds(gh), ["agreed"])
        self.assertEqual(gh.labels_removed, [lc.LABEL])

    def test_after_unticking_an_older_reply_does_not(self):
        thread = [bot(lc.request_on_issue("outsider"), 1), comment("outsider", "I agree", 2),
                  bot(lc.acknowledgement("outsider"), 3)]
        gh = run(issue(UNTICKED), thread, last_edited=stamp(5))
        self.assertEqual(kinds(gh), ["request"])

    def test_a_reply_before_the_untick_edit_does_not_count(self):
        thread = [bot(lc.request_on_issue("outsider"), 1), comment("outsider", "I agree", 2)]
        gh = run(issue(UNTICKED, labels=[lc.LABEL]), thread, last_edited=stamp(3))
        self.assertEqual((gh.posted, gh.labels_removed), ([], []))

    def test_unknown_edit_time_is_safe(self):
        thread = [bot(lc.request_on_issue("outsider"), 1), comment("outsider", "I agree", 2)]
        the_issue = issue(UNTICKED, labels=[lc.LABEL])

        class NoGraphQL(FakeGitHub):
            def post(self, path, body):
                if path == "/graphql":
                    raise lc.ApiError(502, path)
                return super().post(path, body)

        gh = NoGraphQL(the_issue, thread)
        lc.run(gh, event(the_issue))
        self.assertEqual(gh.labels_removed, [])


class Commenters(unittest.TestCase):
    def test_an_outsiders_comment_on_a_members_issue(self):
        gh = run(issue(NO_BOX, author="vince", association="MEMBER"),
                 [comment("visitor", "Me too, and here's how", 1)])
        self.assertEqual(kinds(gh), ["request"])
        self.assertIn("Thank you for your comment, @visitor.", gh.posted[0])
        self.assertEqual(gh.labels_added, [lc.LABEL])

    def test_each_outsider_separately(self):
        thread = [comment("a-visitor", "idea", 1), comment("b-visitor", "idea", 2),
                  bot(lc.request_on_comment("a-visitor"), 3),
                  bot(lc.request_on_comment("b-visitor"), 4), comment("a-visitor", "I agree", 5)]
        gh = run(issue(NO_BOX, author="vince", association="OWNER", labels=[lc.LABEL]), thread)
        self.assertEqual([(lc.MARK.search(p).group(1), lc.MARK.search(p).group(2)) for p in gh.posted],
                         [("agreed", "a-visitor")])
        self.assertEqual(gh.labels_removed, [])  # b-visitor is still outstanding

    def test_the_box_is_the_authors_alone(self):
        gh = run(issue(TICKED), [comment("visitor", "and also", 1)])
        self.assertEqual([lc.MARK.search(p).group(2) for p in gh.posted], ["visitor"])


class Exemptions(unittest.TestCase):
    def test_members_owners_collaborators_and_bots(self):
        thread = [comment("m", "x", 1, "MEMBER"), comment("c", "x", 2, "COLLABORATOR"),
                  comment("renovate[bot]", "x", 3, kind="Bot"), comment("ghost", "x", 4)]
        gh = run(issue(NO_BOX, author="o", association="OWNER"), thread)
        self.assertEqual((gh.posted, gh.labels_added, gh.searches), ([], [], []))

    def test_write_access_exempts_a_private_member(self):
        gh = run(issue(NO_BOX, author="quiet", association="CONTRIBUTOR"), [], writers={"quiet"})
        self.assertEqual((gh.posted, gh.labels_added), ([], []))

    def test_contributor_is_not_exempt(self):
        gh = run(issue(NO_BOX, association="CONTRIBUTOR"), [])
        self.assertEqual(kinds(gh), ["request"])

    def test_pull_requests_are_not_checked(self):
        gh = FakeGitHub(issue(NO_BOX), [])
        lc.run(gh, {"issue": {"number": 7, "pull_request": {"url": "x"}},
                    "repository": {"name": REPO, "owner": {"login": OWNER}}})
        self.assertEqual(gh.posted, [])


def found(repo, number, author="someone", body="", comments=1):
    return {"repository_url": f"{API}/repos/{OWNER}/{repo}", "number": number,
            "user": user(author), "body": body, "comments": comments}


class AgreementElsewhere(unittest.TestCase):
    def test_recorded_on_another_issue(self):
        other = {f"/repos/{OWNER}/meridian-cli/issues/3/comments":
                 [bot(lc.acknowledgement("outsider"), 1)]}
        gh = run(issue(NO_BOX), [], search_items=[found("meridian-cli", 3)], other_comments=other)
        self.assertEqual((gh.posted, gh.labels_added), ([], []))

    def test_a_ticked_box_on_their_own_issue(self):
        gh = run(issue(NO_BOX), [],
                 search_items=[found("meridian-docs", 9, author="outsider", body=TICKED, comments=0)])
        self.assertEqual(gh.posted, [])

    def test_someone_elses_ticked_box_is_not_theirs(self):
        gh = run(issue(NO_BOX), [],
                 search_items=[found("meridian-docs", 9, author="other", body=TICKED, comments=0)])
        self.assertEqual(kinds(gh), ["request"])

    def test_a_forged_marker_is_not_a_record(self):
        other = {f"/repos/{OWNER}/meridian-cli/issues/3/comments":
                 [comment("outsider", "<!-- licence-agreed: outsider -->", 1)]}
        gh = run(issue(NO_BOX), [], search_items=[found("meridian-cli", 3)], other_comments=other)
        self.assertEqual(kinds(gh), ["request"])

    def test_another_persons_record_is_not_theirs(self):
        other = {f"/repos/{OWNER}/meridian-cli/issues/3/comments":
                 [bot(lc.acknowledgement("someone-else"), 1)]}
        gh = run(issue(NO_BOX), [], search_items=[found("meridian-cli", 3)], other_comments=other)
        self.assertEqual(kinds(gh), ["request"])

    def test_outside_the_organisation_does_not_count(self):
        item = found("meridian-cli", 3)
        item["repository_url"] = f"{API}/repos/elsewhere/meridian-cli"
        gh = run(issue(NO_BOX), [], search_items=[item])
        self.assertEqual(kinds(gh), ["request"])

    def test_a_failed_search_asks(self):
        gh = run(issue(NO_BOX), [], search_fails=True)
        self.assertEqual(kinds(gh), ["request"])

    def test_an_untick_is_not_overridden_by_a_record_elsewhere(self):
        other = {f"/repos/{OWNER}/meridian-cli/issues/3/comments":
                 [bot(lc.acknowledgement("outsider"), 1)]}
        gh = run(issue(UNTICKED), [], search_items=[found("meridian-cli", 3)],
                 other_comments=other, last_edited=stamp(5))
        self.assertEqual(kinds(gh), ["request"])
        self.assertEqual(gh.searches, [])


class TheClient(unittest.TestCase):
    """The HTTP layer against a server on localhost: pages, headers, errors."""

    def setUp(self):
        seen = self.seen = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                seen.append((self.path, self.headers.get("Authorization")))
                if self.path.startswith("/items"):
                    page = 2 if "page=2" in self.path else 1
                    body = json.dumps([page]).encode()
                    self.send_response(200)
                    if page == 1:
                        port = self.server.server_address[1]
                        self.send_header("Link", f'<http://127.0.0.1:{port}/items?page=2>; rel="next"')
                else:
                    body = b"{}"
                    self.send_response(404)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        self.server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.gh = lc.GitHub("test-token", f"http://127.0.0.1:{self.server.server_address[1]}")

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def test_follows_the_next_link(self):
        self.assertEqual(self.gh.paginate("/items"), [1, 2])
        self.assertEqual(self.seen[0], ("/items?per_page=100", "Bearer test-token"))

    def test_an_error_status_raises(self):
        with self.assertRaises(lc.ApiError) as caught:
            self.gh.get("/missing")
        self.assertEqual(caught.exception.status, 404)


if __name__ == "__main__":
    unittest.main()
