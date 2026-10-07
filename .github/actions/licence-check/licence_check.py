#!/usr/bin/env python3
"""Ask anyone outside the team who writes on an issue to agree, once, to the
suggestion licence in CONTRIBUTING.md.

The issue forms make the licence box required, but GitHub enforces a form only
in the browser: an issue opened through the API or the CLI skips it, an edit can
untick it, and a comment never had it. This closes those gaps.

Every run looks at the whole issue as it is now, not at the one event that
started it, so a run that is skipped or replaced loses nothing:

  * People inside the organisation (OWNER, MEMBER, COLLABORATOR) and bots are
    exempt.
  * Anyone else who wrote the issue or a comment on it needs an agreement. The
    issue's author agrees by the box ticked in the body; anyone agrees by
    replying "I agree" after the bot asked them; and an agreement the bot
    recorded, here or on any other issue in the organisation, covers them
    afterwards. An unticked box in the body overrides the record for that
    issue: unticking re-flags.
  * The bot asks each person once per flagging, labels the issue while anyone
    is outstanding, and records an agreement with a comment carrying
    <!-- licence-agreed: login -->, which later runs find through search.

Default to safe: whatever cannot be confirmed -- a search that fails or lags, an
API error -- counts as not agreed, so the person is asked.

The payload is read as JSON and nothing from it reaches a shell. Logs carry
logins and numbers only, never text someone wrote.

Usage (in the action):  python3 licence_check.py      reads GITHUB_EVENT_PATH
Tests:                  python3 -m unittest discover -s .github/actions/licence-check
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Callable, Optional

# The line in the issue forms. A test holds it to the templates word for word.
LICENCE_LINE = (
    "I give Societal Lab Inc. a perpetual, irrevocable, worldwide, royalty-free "
    "licence to use what I've written here for any purpose, as CONTRIBUTING.md says."
)

# Quoted, never paraphrased. A test holds it to CONTRIBUTING.md word for word.
CONTRIBUTING_TERMS = (
    "By opening an issue or making a suggestion, you give Societal Lab Inc. a "
    "perpetual, irrevocable, worldwide, royalty-free licence to use it for any "
    "purpose, with no obligation to you."
)

LABEL = "needs-licence"
LABEL_DESCRIPTION = "Waiting for agreement to the licence in CONTRIBUTING.md"
LABEL_COLOR = "fbca04"

BOT_LOGIN = "github-actions[bot]"
EXEMPT_ASSOCIATIONS = frozenset({"OWNER", "MEMBER", "COLLABORATOR"})
# Deleted accounts show as "ghost"; there is nobody left to ask.
UNASKABLE = frozenset({"ghost"})
# Issues in the organisation a person was involved in, most recently updated
# first, that a run reads looking for their recorded agreement.
SEARCH_CANDIDATES = 30

MARK = re.compile(r"<!-- licence-(request|agreed): ([A-Za-z0-9-]+) -->")
VALID_LOGIN = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$")
TASK_ITEM = re.compile(r"^\s*[-*+]\s+\[([ xX])\]\s+(.*?)\s*$")


# --- Messages ---------------------------------------------------------------
# WORDING.md in the review folder carries these for the product owner; this is
# the copy that runs.

def request_on_issue(login: str) -> str:
    return (
        f"<!-- licence-request: {login} -->\n"
        f"Thank you for opening this, @{login}.\n\n"
        "Please confirm that you agree to the licence in CONTRIBUTING.md:\n\n"
        f"> {CONTRIBUTING_TERMS}\n\n"
        "To confirm, reply to this comment with **I agree**, or edit the issue "
        "and tick the licence box if it has one. You only need to do this "
        "once.\n\n"
        f"Until then, this issue carries the `{LABEL}` label."
    )


def request_on_comment(login: str) -> str:
    return (
        f"<!-- licence-request: {login} -->\n"
        f"Thank you for your comment, @{login}.\n\n"
        "Please confirm that you agree to the licence in CONTRIBUTING.md:\n\n"
        f"> {CONTRIBUTING_TERMS}\n\n"
        "To confirm, reply to this comment with **I agree**. You only need to "
        "do this once."
    )


def acknowledgement(login: str) -> str:
    return (
        f"<!-- licence-agreed: {login} -->\n"
        f"Thank you, @{login}. We've recorded your agreement."
    )


# --- Reading text -----------------------------------------------------------

def _normalise(text: str) -> str:
    text = text.replace("’", "'").replace("‘", "'")
    return " ".join(text.split())


def box_state(body: Optional[str]) -> str:
    """'ticked', 'unticked' or 'absent': the licence line as a task item.

    An unticked copy anywhere wins over a ticked one, so pasting a ticked line
    beside an unticked one does not read as agreement.
    """
    want = _normalise(LICENCE_LINE)
    seen = set()
    for line in (body or "").splitlines():
        m = TASK_ITEM.match(line)
        if m and _normalise(m.group(2)) == want:
            seen.add("unticked" if m.group(1) == " " else "ticked")
    if "unticked" in seen:
        return "unticked"
    return "ticked" if seen else "absent"


def is_agreement_reply(body: Optional[str]) -> bool:
    """The words "I agree" and nothing else, ignoring quoted lines, case,
    emphasis and closing punctuation. "I agree with @someone" is not it."""
    text = re.sub(r"<!--.*?-->", " ", body or "", flags=re.S)
    kept = [ln for ln in text.splitlines() if not ln.lstrip().startswith(">")]
    words = re.sub(r"[*_`~.!]", " ", " ".join(kept)).lower().split()
    return words == ["i", "agree"]


def when(stamp: str) -> datetime:
    return datetime.fromisoformat(stamp.replace("Z", "+00:00"))


def is_bot_comment(comment: dict) -> bool:
    user = comment.get("user") or {}
    return user.get("login") == BOT_LOGIN and user.get("type") == "Bot"


def bot_marks(comments: list) -> dict:
    """login (lower case) -> [(kind, created_at)], oldest first, from the bot's
    own comments only. A marker anybody else typed counts for nothing."""
    marks: dict = {}
    for c in sorted(comments, key=lambda c: when(c["created_at"])):
        if not is_bot_comment(c):
            continue
        for kind, login in MARK.findall(c.get("body") or ""):
            marks.setdefault(login.lower(), []).append((kind, when(c["created_at"])))
    return marks


# --- Deciding ---------------------------------------------------------------

@dataclass
class Person:
    login: str
    associations: set = field(default_factory=set)
    is_bot: bool = False
    is_author: bool = False

    @property
    def key(self) -> str:
        return self.login.lower()


def participants(issue: dict, comments: list) -> list:
    people: dict = {}

    def note(user: dict, association: Optional[str], is_author: bool) -> None:
        login = (user or {}).get("login") or ""
        if not login:
            return
        p = people.setdefault(login.lower(), Person(login))
        p.associations.add(association or "NONE")
        p.is_bot = p.is_bot or (user or {}).get("type") == "Bot"
        p.is_author = p.is_author or is_author

    note(issue.get("user"), issue.get("author_association"), True)
    for c in comments:
        note(c.get("user"), c.get("author_association"), False)
    return list(people.values())


def agreement_here(person: Person, issue: dict, comments: list, marks: dict,
                   last_edited: Callable[[], datetime]) -> Optional[bool]:
    """True or False when this issue settles it, None when only a record
    elsewhere could."""
    mine = marks.get(person.key, [])
    requests = [t for kind, t in mine if kind == "request"]
    last_request = max(requests) if requests else None
    replies = [
        when(c["created_at"]) for c in comments
        if ((c.get("user") or {}).get("login") or "").lower() == person.key
        and is_agreement_reply(c.get("body"))
    ]

    box = box_state(issue.get("body")) if person.is_author else "absent"
    if box == "ticked":
        return True
    if box == "unticked":
        # An unticked box is a choice about this issue, so no record covers
        # it: only a reply to a request made since, and since the last edit.
        if last_request is None:
            return False
        since = max(last_request, last_edited())
        return any(r > since for r in replies)
    if any(kind == "agreed" for kind, _ in mine):
        return True
    if last_request is not None and any(r > last_request for r in replies):
        return True
    return None


@dataclass
class Decision:
    posts: list = field(default_factory=list)    # [(login, "issue"|"comment"|"agreed")]
    outstanding: list = field(default_factory=list)
    notes: list = field(default_factory=list)    # logins and reasons, for the log


def decide(issue: dict, comments: list, *,
           last_edited: Callable[[], datetime],
           has_write: Callable[[str], bool],
           agreed_elsewhere: Callable[[str], bool]) -> Decision:
    marks = bot_marks(comments)
    d = Decision()
    for p in participants(issue, comments):
        if p.is_bot or p.key in UNASKABLE:
            continue
        if p.associations & EXEMPT_ASSOCIATIONS:
            d.notes.append(f"{p.login}: exempt ({'/'.join(sorted(p.associations))})")
            continue
        agreed = agreement_here(p, issue, comments, marks, last_edited)
        if agreed is not True and has_write(p.login):
            # author_association reads CONTRIBUTOR for a member whose
            # membership is private; write access says what it means.
            d.notes.append(f"{p.login}: exempt (write access)")
            continue
        if agreed is None:
            agreed = agreed_elsewhere(p.login)
            d.notes.append(f"{p.login}: {'agreed' if agreed else 'no agreement'} on record")
        else:
            d.notes.append(f"{p.login}: {'agreed' if agreed else 'not agreed'} on this issue")

        mine = marks.get(p.key, [])
        latest = mine[-1][0] if mine else None
        if agreed:
            if latest == "request":
                d.posts.append((p.login, "agreed"))
        else:
            d.outstanding.append(p.login)
            if latest != "request":
                d.posts.append((p.login, "issue" if p.is_author else "comment"))
    return d


MESSAGES = {"issue": request_on_issue, "comment": request_on_comment,
            "agreed": acknowledgement}


# --- GitHub -----------------------------------------------------------------

class ApiError(Exception):
    def __init__(self, status: int, path: str):
        super().__init__(f"GitHub answered {status} for {path}")
        self.status = status


class GitHub:
    def __init__(self, token: str, api: str = "https://api.github.com"):
        self.token = token
        self.api = api.rstrip("/")

    def call(self, method: str, path: str, body=None, params=None):
        url = path if path.startswith("http") else self.api + path
        if params:
            url += ("&" if "?" in url else "?") + urllib.parse.urlencode(params)
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method, headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "open-meridian-licence-check",
        })
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                raw = resp.read()
                link = resp.headers.get("Link") or ""
        except urllib.error.HTTPError as e:
            raise ApiError(e.code, path) from None
        except urllib.error.URLError:
            raise ApiError(0, path) from None
        return (json.loads(raw) if raw else None), link

    def get(self, path, params=None):
        return self.call("GET", path, params=params)[0]

    def post(self, path, body):
        return self.call("POST", path, body=body)[0]

    def delete(self, path):
        return self.call("DELETE", path)[0]

    def paginate(self, path, max_pages=10):
        out, url, params = [], path, {"per_page": 100}
        for _ in range(max_pages):
            page, link = self.call("GET", url, params=params)
            out.extend(page or [])
            m = re.search(r'<([^>]+)>;\s*rel="next"', link)
            if not m:
                break
            url, params = m.group(1), None
        return out


def has_write(gh: GitHub, owner: str, repo: str, login: str) -> bool:
    if not VALID_LOGIN.match(login):
        return False
    try:
        got = gh.get(f"/repos/{owner}/{repo}/collaborators/{login}/permission")
    except ApiError:
        return False
    return (got or {}).get("permission") in ("admin", "write")


def agreed_elsewhere(gh: GitHub, owner: str, repo: str, number: int, login: str) -> bool:
    """Search finds candidate issues; only the bot's own marker, or a ticked box
    in an issue the person wrote, counts as agreement. Search can lag a new
    comment by a minute or more, and returns at most SEARCH_CANDIDATES here:
    a miss asks again, which is the safe way to be wrong."""
    if not VALID_LOGIN.match(login):
        return False
    try:
        found = gh.get("/search/issues", {
            "q": f"org:{owner} involves:{login} is:issue",
            "sort": "updated", "order": "desc", "per_page": SEARCH_CANDIDATES,
        })
    except ApiError:
        return False
    prefix = f"{gh.api}/repos/{owner}/".lower()
    candidates = []
    for item in (found or {}).get("items", []):
        repo_url = (item.get("repository_url") or "").lower()
        if not repo_url.startswith(prefix):
            continue
        other = repo_url[len(prefix):]
        if other == repo.lower() and item.get("number") == number:
            continue
        if (((item.get("user") or {}).get("login") or "").lower() == login.lower()
                and box_state(item.get("body")) == "ticked"):
            return True
        if item.get("comments"):
            candidates.append((other, item["number"]))
    for other, n in candidates:
        try:
            comments = gh.paginate(f"/repos/{owner}/{other}/issues/{n}/comments")
        except ApiError:
            continue
        if any(kind == "agreed" for kind, _ in bot_marks(comments).get(login.lower(), [])):
            return True
    return False


def last_edited(gh: GitHub, owner: str, repo: str, number: int, created_at: str) -> datetime:
    query = ("query($o:String!,$r:String!,$n:Int!){repository(owner:$o,name:$r)"
             "{issue(number:$n){lastEditedAt}}}")
    try:
        got = gh.post("/graphql", {"query": query,
                                   "variables": {"o": owner, "r": repo, "n": number}})
        stamp = got["data"]["repository"]["issue"]["lastEditedAt"]
    except (ApiError, KeyError, TypeError):
        # Unknown: treat it as later than any reply, so none counts.
        return datetime.max.replace(tzinfo=timezone.utc)
    return when(stamp or created_at)


def apply(gh: GitHub, owner: str, repo: str, issue: dict, decision: Decision) -> None:
    base = f"/repos/{owner}/{repo}"
    number = issue["number"]
    for login, kind in decision.posts:
        gh.post(f"{base}/issues/{number}/comments", {"body": MESSAGES[kind](login)})
        print(f"#{number}: posted {kind} for {login}")
    labelled = LABEL in [lb.get("name") for lb in issue.get("labels") or []]
    if decision.outstanding and not labelled:
        try:
            gh.get(f"{base}/labels/{LABEL}")
        except ApiError as e:
            if e.status != 404:
                raise
            try:
                gh.post(f"{base}/labels", {"name": LABEL, "color": LABEL_COLOR,
                                           "description": LABEL_DESCRIPTION})
            except ApiError as e2:
                if e2.status != 422:  # created meanwhile
                    raise
        gh.post(f"{base}/issues/{number}/labels", {"labels": [LABEL]})
        print(f"#{number}: labelled {LABEL}")
    elif not decision.outstanding and labelled:
        try:
            gh.delete(f"{base}/issues/{number}/labels/{LABEL}")
        except ApiError as e:
            if e.status != 404:
                raise
        print(f"#{number}: removed {LABEL}")


def run(gh: GitHub, event: dict) -> int:
    issue_ev = event.get("issue")
    if not issue_ev:
        print("not an issue event; nothing to do")
        return 0
    if issue_ev.get("pull_request"):
        print(f"#{issue_ev.get('number')} is a pull request; not checked")
        return 0
    owner = event["repository"]["owner"]["login"]
    repo = event["repository"]["name"]
    number = issue_ev["number"]

    # Read the issue as it is now: a queued run may be behind its payload.
    issue = gh.get(f"/repos/{owner}/{repo}/issues/{number}")
    comments = gh.paginate(f"/repos/{owner}/{repo}/issues/{number}/comments")

    edited: list = []

    def lazily_edited() -> datetime:
        if not edited:
            edited.append(last_edited(gh, owner, repo, number, issue["created_at"]))
        return edited[0]

    decision = decide(
        issue, comments,
        last_edited=lazily_edited,
        has_write=lambda login: has_write(gh, owner, repo, login),
        agreed_elsewhere=lambda login: agreed_elsewhere(gh, owner, repo, number, login),
    )
    for note in decision.notes:
        print(f"#{number}: {note}")
    apply(gh, owner, repo, issue, decision)
    return 0


def main() -> int:
    with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as f:
        event = json.load(f)
    gh = GitHub(os.environ["GITHUB_TOKEN"],
                os.environ.get("GITHUB_API_URL", "https://api.github.com"))
    return run(gh, event)


if __name__ == "__main__":
    sys.exit(main())
