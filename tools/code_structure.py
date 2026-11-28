#!/usr/bin/env python3
"""The code-structure rules' checks (standing rules adopted 2026-11-20; each
repository's CLAUDE.md states them). One copy, the same byte for
byte in every repository that runs it; standard library only.

    python3 tools/code_structure.py interims [--write]
    python3 tools/code_structure.py pins
    python3 tools/code_structure.py shellouts
    python3 tools/code_structure.py size
    python3 tools/code_structure.py licences
    python3 tools/code_structure.py baselines --base <ref>
    python3 tools/code_structure.py --self-test

Each reads its settings from tools/code-structure.toml:

    roadmap_version   the version this repository is building (S4), e.g. "v31a"
    interims_register the register S4 writes from the markers
    interims_register_lines  false: a marker's place there is its file alone
    pins_allowlist    S5's listed exceptions, each with its INTERIM marker
    pins_refs         S5: a workflow ref taken from an expression, and the
                      file holding the pin it gives
    shellouts         S9: the source globs read, and the kinds file
    size_baseline     S2: the files over the limit today, with their lines
    size_exempt       S2: generated and vendored globs
    licence_scan      S10: the globs that ship under Apache-2.0
    notice            S10: true where NOTICE must ship beside LICENSE
    notices           S10: each image (a Dockerfile's stage) and the licence
                      and notice files it ships, a bundled component's too

S4  INTERIM(vNN): reason. A marker is `INTERIM(` then a version (v31a, v32),
    `):` and a reason. A marker whose version is the roadmap version or
    earlier fails: its stop-gap was due. A marker naming no reason is said,
    held in the register with none, and their number only goes down. The
    register is generated from the markers and a stale one fails, as a
    generated file does (S3). Moving a
    marker to a later version needs the product owner's word and a
    `Baseline-Change:` trailer naming the marker (`baselines` reads the
    register at the base: a marker of the same file and reason at a later
    version now has moved).
S5  A dependency on another repository is an immutable reference: a released
    version or a full commit SHA, never a branch or a sibling checkout. Read
    in Cargo.toml (git rev, no branch or tag, no path outside the repo),
    pyproject.toml (git+ URLs at a full SHA) and .github/workflows (`uses:`
    at a version or a full SHA; a checkout of another repository at a full
    SHA or a version; a ref from an expression is read from the pin file
    pins_refs names). A path inside the repository is a workspace member.
    An exception is listed with its INTERIM marker.
S9  Each kubectl and helm call in the CLI's source names its kind (tool, verb
    and, for a verb that takes one, the resource) in an `// S9:` comment,
    and each kind is listed with its reason. A helper that passes its
    caller's arguments on is followed to its calls. A kind called and not
    listed fails, and so does one listed and not called. A new kind needs
    its reason and a `Baseline-Change:` trailer naming it; the number listed
    with no reason only goes down.
S2  A hand-written file over 1,500 lines: a new one, or a listed one grown
    past its baseline, is reported (a warning in v31a; failing from v31b,
    where growth needs a `Size-Exception:` trailer).
S10 What ships under Apache-2.0 (licence_scan) carries no GPL or AGPL
    licence text; where notice is set, NOTICE ships beside LICENSE. A crate
    whose licence is not GPL or AGPL holds no such text and depends on no
    GPL or AGPL crate of the repository. Each image `notices` names ships
    the licence and notice files listed for it.
    Baselines and allowlists change only in a commit with a
    `Baseline-Change:` trailer saying why, and a ratchet's baseline only goes
    down.

Exit: 0 holds, 1 broken (each finding said), 2 asked wrongly.
"""

from __future__ import annotations

import fnmatch
import posixpath
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

LIMIT = 1500
MARKER = re.compile(r"INTERIM\((v\d+[a-z]?)\)(:\s*\S.*)?")
SHA = re.compile(r"^[0-9a-f]{40}$")
VERSION = re.compile(r"^v?\d+(\.\d+)*$")
TRAILER = "Baseline-Change:"


def version_key(version: str) -> tuple[int, str]:
    m = re.fullmatch(r"v(\d+)([a-z]?)", version)
    if not m:
        raise ValueError(f"{version!r} is not a roadmap version (v31a, v32)")
    return int(m.group(1)), m.group(2)


def tracked(root: Path) -> list[str]:
    out = subprocess.run(["git", "-C", str(root), "ls-files"], capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"code-structure: git ls-files: {out.stderr.strip()}")
    return [line for line in out.stdout.splitlines() if (root / line).is_file()]


def matches(path: str, globs: list[str]) -> bool:
    return any(fnmatch.fnmatch(path, glob) for glob in globs)


def text_of(root: Path, path: str) -> str | None:
    try:
        return (root / path).read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        return None


# ── S4 ──────────────────────────────────────────────────────────────────────


def markers(root: Path, files: list[str], skip: list[str]) -> tuple[list[tuple[str, str, str]], list[str]]:
    """Each marker as (version, place, reason), and each one naming no reason
    said. A marker naming no reason is held too, with an empty reason: it is
    still due at its version, and how many there are only goes down."""
    found, malformed = [], []
    for path in files:
        if matches(path, skip):
            continue
        text = text_of(root, path)
        if text is None or "INTERIM(v" not in text:
            continue
        for n, line in enumerate(text.splitlines(), 1):
            for m in MARKER.finditer(line):
                if not m.group(2):
                    malformed.append(f"S4: {path}:{n}: INTERIM({m.group(1)}) names no reason; "
                                     f"write `INTERIM({m.group(1)}): <why, and what ends it>`")
                    found.append((m.group(1), f"{path}:{n}", ""))
                    continue
                reason = m.group(2)[1:].strip().rstrip("*/#-> ").strip()
                found.append((m.group(1), f"{path}:{n}", reason))
    return found, malformed


def register(found: list[tuple[str, str, str]], version: str, lines_too: bool = True) -> str:
    """With lines_too false a marker's place is its file alone, so an edit
    above a marker leaves the register as it was."""
    lines = [f"# Interims (S4), generated from the INTERIM(vNN) markers by "
             f"`tools/code_structure.py interims --write`. Roadmap version: {version}.",
             "# version\tplace\treason"]
    rows = [(v, place if lines_too else place.rsplit(":", 1)[0], reason) for v, place, reason in found]
    for v, place, reason in sorted(rows, key=lambda f: (version_key(f[0]), f[1], f[2])):
        lines.append(f"{v}\t{place}\t{reason}")
    return "\n".join(lines) + "\n"


def check_interims(root: Path, cfg: dict, write: bool, warnings: list[str] | None = None) -> list[str]:
    """What fails; each marker naming no reason goes into `warnings`, and
    check_baselines holds their number to the base's."""
    version = cfg["roadmap_version"]
    version_key(version)
    out = root / cfg["interims_register"]
    found, unreasoned = markers(root, tracked(root), [cfg["interims_register"], "tools/code_structure.py"])
    if warnings is not None:
        warnings += unreasoned
    failures = []
    for v, place, reason in found:
        if version_key(v) <= version_key(version):
            failures.append(f"S4: {place}: INTERIM({v}) is due: the roadmap version is {version}. "
                            "End the stop-gap, or move its version with the product owner's word "
                            "and a trailer saying so")
    wanted = register(found, version, cfg.get("interims_register_lines", True))
    if write:
        out.write_text(wanted, encoding="utf-8")
    elif text_of(root, cfg["interims_register"]) != wanted:
        failures.append(f"S4: {cfg['interims_register']} is not the markers' register; run "
                        "`python3 tools/code_structure.py interims --write`")
    return failures


def moved_interims(root: Path, cfg: dict, base: str) -> list[tuple[str, str, str]]:
    """Each marker now at a later version than the base's register held it,
    as (place, version then, version now). A marker is the same one when its
    file and reason are."""
    then: dict[tuple[str, str], str] = {}
    for line in git(root, "show", f"{base}:{cfg['interims_register']}").splitlines():
        parts = line.split("\t")
        if line.startswith("#") or len(parts) != 3:
            continue
        version, place, reason = parts
        then[(place.rsplit(":", 1)[0], reason)] = version
    found, _ = markers(root, tracked(root), [cfg["interims_register"], "tools/code_structure.py"])
    moved = []
    for version, place, reason in found:
        was = then.get((place.rsplit(":", 1)[0], reason))
        if was and version_key(version) > version_key(was):
            moved.append((place, was, version))
    return moved


# ── S5 ──────────────────────────────────────────────────────────────────────


def cargo_pins(path: str, text: str) -> list[str]:
    failures = []
    data = tomllib.loads(text)
    tables = []
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        tables.append((key, data.get(key, {})))
        tables.append((f"workspace.{key}", data.get("workspace", {}).get(key, {})))
    for target, spec in data.get("target", {}).items():
        for key in ("dependencies", "dev-dependencies", "build-dependencies"):
            tables.append((f"target.{target}.{key}", spec.get(key, {})))
    for where, table in tables:
        for name, dep in table.items():
            if not isinstance(dep, dict):
                continue
            if "git" in dep:
                if "branch" in dep or "tag" in dep or not SHA.match(str(dep.get("rev", ""))):
                    failures.append(f"S5: {path} [{where}] {name}: a git dependency at "
                                    f"{dep.get('branch') or dep.get('tag') or dep.get('rev') or 'its default branch'}; "
                                    "pin `rev` to a full commit SHA")
            # A path inside the repository is a workspace member; one that
            # leaves it is a sibling checkout.
            if "path" in dep and posixpath.normpath(
                    posixpath.join(posixpath.dirname(path), str(dep["path"]))).startswith(".."):
                failures.append(f"S5: {path} [{where}] {name}: a sibling checkout ({dep['path']}); "
                                "depend on a release or a full commit SHA")
    return failures


def pyproject_pins(path: str, text: str) -> list[str]:
    failures = []
    data = tomllib.loads(text)
    project = data.get("project", {})
    deps = list(project.get("dependencies", []))
    for extra in project.get("optional-dependencies", {}).values():
        deps += extra
    for group in data.get("dependency-groups", {}).values():
        deps += [d for d in group if isinstance(d, str)]
    for dep in deps:
        if "git+" in dep:
            ref = dep.rsplit("@", 1)[-1] if dep.count("@") >= 2 else ""
            if not SHA.match(ref.split("#")[0]):
                failures.append(f"S5: {path}: {dep}: a git dependency not at a full commit SHA")
        if "file:" in dep or re.search(r"@\s*\.\.?/", dep):
            failures.append(f"S5: {path}: {dep}: a sibling checkout")
    return failures


OWNER_EXPR = re.compile(r"^\$\{\{\s*github\.repository_owner\s*\}\}/")
REF_EXPR = re.compile(r"^\$\{\{\s*([\w.-]+)\s*\}\}$")


def pinned_in(root: Path, file: str) -> str:
    """The first value a pin file holds: its first line that is not blank or a
    comment, with any trailing comment and spaces taken off."""
    for line in (text_of(root, file) or "").splitlines():
        value = line.split("#", 1)[0].strip()
        if value:
            return value
    return ""


def workflow_pins(path: str, text: str, own: str, refs: dict[str, str] | None = None,
                  root: Path | None = None) -> list[str]:
    """`refs` names, for a ref a workflow takes from an expression (a step's
    output), the file holding the pin that expression gives; the pin there is
    what is held to a version or a full SHA."""
    failures = []
    refs = refs or {}
    steps = re.split(r"\n(?=\s*- )", text)
    for step in steps:
        for m in re.finditer(r"uses:\s*([^\s#'\"]+)", step):
            used = m.group(1)
            if used.startswith(("./", "docker://")):
                continue
            ref = used.rsplit("@", 1)[-1] if "@" in used else ""
            if not (SHA.match(ref) or VERSION.match(ref)):
                failures.append(f"S5: {path}: uses {used}: not a released version or a full SHA")
        repo = re.search(r"\brepository:\s*['\"]?((?:\$\{\{\s*github\.repository_owner\s*\}\}|[\w.-]+)/[\w.-]+)",
                         step)
        name = OWNER_EXPR.sub(own.split("/")[0] + "/", repo.group(1)) if repo else ""
        if repo and name != own:
            ref = re.search(r"\bref:\s*['\"]?(\$\{\{[^}]*\}\}|[^\s'\"#]+)", step)
            value = ref.group(1) if ref else "its default branch"
            expr = REF_EXPR.match(value)
            if expr and expr.group(1) in refs and root is not None:
                file = refs[expr.group(1)]
                held = pinned_in(root, file)
                if not (SHA.match(held) or VERSION.match(held)):
                    failures.append(f"S5: {path}: checks out {name} at {held or 'nothing'} ({file}): "
                                    "not a released version or a full SHA")
                continue
            if not (SHA.match(value) or VERSION.match(value)):
                failures.append(f"S5: {path}: checks out {name} at {value}: not a "
                                "released version or a full SHA")
    return failures


def check_pins(root: Path, cfg: dict) -> list[str]:
    failures = []
    own = cfg["repository"]
    for path in tracked(root):
        name = Path(path).name
        text = text_of(root, path)
        if text is None:
            continue
        if name == "Cargo.toml":
            failures += cargo_pins(path, text)
        elif name == "pyproject.toml" and not matches(path, cfg.get("pins_skip", [])):
            failures += pyproject_pins(path, text)
        elif path.startswith(".github/workflows/") and name.endswith((".yml", ".yaml")):
            failures += workflow_pins(path, text, own, cfg.get("pins_refs", {}), root)
    listed = cfg.get("pins_allowlist")
    allowed: list[str] = []
    if listed and (root / listed).exists():
        for n, line in enumerate((root / listed).read_text(encoding="utf-8").splitlines(), 1):
            if not line.strip() or line.startswith("#"):
                continue
            entry, _, why = line.partition(" # ")
            if not MARKER.search(why) or not MARKER.search(why).group(2):
                failures.append(f"S5: {listed}:{n}: an exception with no `INTERIM(vNN): reason`")
            allowed.append(entry.strip())
    return [f for f in failures if not any(a and a in f for a in allowed)]


# ── S9 ──────────────────────────────────────────────────────────────────────
#
# Every kubectl or helm call names its kind, tool + verb (+ resource, for a
# verb that takes one), in an `// S9: kubectl get secret` comment on its lines
# or above its statement (several kinds separated by `;`), and every kind is
# listed with its reason in the kinds file. What is a call is read from the
# code, not trusted to the comments:
# - a "kubectl" or "helm" string literal given to a call is a call;
# - a function or closure that hands one of its own string or list
#   parameters to a call's verb or resource (or as the whole argument list)
#   is a wrapper; each call of it names its kind instead, found by name, to a
#   fixed point. A helper can move a call; it cannot hide its kind;
# - a tool name outside a call fails: its kind would be out of sight;
# - each word of a named kind is in a string of that file, of a function its
#   function calls, or of a wrapper, so a comment cannot name a kind the code
#   does not have. A kubectl resource is listed by its plural name.

TOOL_LITERAL = re.compile(r"^(kubectl|helm)$")
KIND = re.compile(r"^(kubectl|helm)((?: -{0,2}[a-z0-9][a-z0-9-]*){1,2})$")
ANNOTATION = re.compile(r"//\s*S9:\s*(.*)")
WORD = re.compile(r"-{0,2}[a-z0-9][a-z0-9-]*")
IDENT = re.compile(r"(?<![\w.:])([A-Za-z_]\w*)")
STRINGLIKE = re.compile(r"\bstr\b|\bString\b|\bVec\b|\[")
# The verbs that take no resource: the word after one is a name, not part of
# its kind. Any other verb's next word is its resource.
NO_RESOURCE = {
    "kubectl": {"logs", "exec", "cp", "apply", "cluster-info", "version", "debug", "port-forward",
                "run", "attach", "proxy", "drain", "cordon", "uncordon", "taint"},
    "helm": {"install", "upgrade", "uninstall", "status", "list", "history", "rollback", "version",
             "template", "lint", "package", "pull", "push", "test", "env"},
}


# kubectl's resources, by the plural name a kind is listed under; a short or
# singular name in the code or a comment stands for it.
RESOURCES = {"configmaps", "cronjobs", "daemonsets", "deployments", "endpoints", "events",
             "ingressclasses", "ingresses", "jobs", "leases", "namespaces", "networkpolicies",
             "nodes", "persistentvolumeclaims", "persistentvolumes", "poddisruptionbudgets", "pods",
             "replicasets", "rolebindings", "roles", "secrets", "serviceaccounts", "services",
             "statefulsets", "storageclasses"}
SHORT = {"cm": "configmaps", "cj": "cronjobs", "ds": "daemonsets", "deploy": "deployments",
         "ns": "namespaces", "netpol": "networkpolicies", "pvc": "persistentvolumeclaims",
         "pv": "persistentvolumes", "pdb": "poddisruptionbudgets", "po": "pods", "rs": "replicasets",
         "sa": "serviceaccounts", "svc": "services", "sts": "statefulsets", "sc": "storageclasses"}


def resource(word: str) -> str:
    """A kubectl resource by its plural name."""
    for name in (SHORT.get(word, word), word + "s", word + "es", word[:-1] + "ies"):
        if name in RESOURCES:
            return name
    return word


def canonical(kind: str) -> str:
    """A kind with its kubectl resource by its plural name."""
    words = kind.split()
    if words[0] == "kubectl" and len(words) == 3:
        words[2] = resource(words[2])
    return " ".join(words)


def rust_lex(text: str) -> tuple[str, list[tuple[int, int, str]]]:
    """The text with comments and strings' contents blanked (offsets kept),
    and each string literal as (start, end, content)."""
    out, strings, i, n = list(text), [], 0, len(text)

    def blank(a: int, b: int) -> None:
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        word_before = i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_")
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif not word_before and (m := re.match(r'b?r(#*)"', text[i:i + 260])):
            close, start = '"' + m.group(1), i + len(m.group(0))
            j = text.find(close, start)
            j = n if j < 0 else j
            strings.append((i, j + len(close), text[start:j]))
            blank(start, j)
            i = j + len(close)
        elif c == '"' or (c == "b" and not word_before and text.startswith('b"', i)):
            start = i + (2 if c == "b" else 1)
            j = start
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            strings.append((i, j + 1, text[start:j]))
            blank(start, j)
            i = j + 1
        elif c == "'" and (m := re.match(r"'(\\.[^']*|[^\\'])'", text[i:i + 12])):
            blank(i + 1, i + len(m.group(0)) - 1)
            i += len(m.group(0))
        else:
            i += 1
    return "".join(out), strings


def closing(code: str, i: int) -> int:
    """The index of the bracket that closes the one at i."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    depth, j = 0, i
    while j < len(code):
        if code[j] in pairs:
            depth += 1
        elif code[j] in ")]}":
            depth -= 1
            if depth == 0:
                return j
        j += 1
    return len(code) - 1


def split_top(code: str, a: int, b: int) -> list[tuple[int, int]]:
    """code[a:b] split at the commas outside brackets, as spans."""
    spans, depth, start = [], 0, a
    for j in range(a, b):
        if code[j] in "([{":
            depth += 1
        elif code[j] in ")]}":
            depth -= 1
        elif code[j] == "," and depth == 0:
            spans.append((start, j))
            start = j + 1
    if code[start:b].strip():
        spans.append((start, b))
    return spans


def test_regions(code: str) -> list[tuple[int, int]]:
    regions = []
    for m in re.finditer(r"#\[cfg\(test\)\]", code):
        j = m.end()
        while j < len(code) and code[j] not in "{;":
            j += 1
        if j < len(code) and code[j] == "{":
            regions.append((m.start(), closing(code, j)))
    return regions


def parameters(parts: list[str]) -> tuple[list[tuple[str, bool]], bool]:
    """Each parameter as (name, string-like), self aside, and whether there
    was a self."""
    params, method = [], False
    for part in parts:
        name, typed, kind = part.partition(":")
        name = name.strip().replace("mut ", "").lstrip("&").strip()
        if name.endswith("self"):
            method = True
        elif name:
            params.append((name, bool(STRINGLIKE.search(kind)) or not typed))
    return params, method


def scopes(code: str) -> list[dict]:
    """Each fn and named closure: name, whether a method, parameters, body."""
    found = []
    arrows = code.replace("->", "=>")
    for m in re.finditer(r"\bfn\s+(\w+)\s*", code):
        j = m.end()
        if code[j:j + 1] == "<":
            depth = 0
            while j < len(code):
                depth += {"<": 1, ">": -1}.get(arrows[j], 0)
                j += 1
                if depth == 0:
                    break
        if code[j:j + 1] != "(":
            continue
        end = closing(code, j)
        params, method = parameters([arrows[a:b] for a, b in split_top(arrows, j + 1, end)])
        k = end
        while k < len(code) and code[k] not in "{;":
            k += 1
        if k < len(code) and code[k] == "{":
            line_start = code.rfind("\n", 0, m.start()) + 1
            public = re.search(r"\bpub\b", code[line_start:m.start()]) is not None
            found.append({"name": m.group(1), "method": method, "params": params,
                          "span": (k, closing(code, k)), "within": None if public or method else "module"})
    for m in re.finditer(r"\blet\s+(?:mut\s+)?(\w+)\s*=\s*(?:move\s+)?\|([^|]*)\|", code):
        params, _ = parameters(m.group(2).split(","))
        j, depth = m.end(), 0
        while j < len(code) and not (code[j] == ";" and depth == 0):
            depth += 1 if code[j] in "([{" else -1 if code[j] in ")]}" else 0
            j += 1
        found.append({"name": m.group(1), "method": False, "params": params, "span": (m.end(), j),
                      "within": (m.start(), j)})
    for closure in found:
        if isinstance(closure["within"], tuple):
            around = [s for s in found if not isinstance(s["within"], tuple)
                      and s["span"][0] <= closure["within"][0] <= s["span"][1]]
            if around:
                closure["within"] = min((s["span"] for s in around), key=lambda s: s[1] - s[0])
    return found


def kind_names(code: str, strings: dict[int, str], spans: list[tuple[int, int]], tool: str) -> set[str]:
    """The names that reach a call's kind: a list given in place gives those
    in its verb's place and, for a verb that takes one, its resource's; any
    other argument gives all of its own."""
    names: set[str] = set()

    def idents(a: int, b: int) -> None:
        names.update(IDENT.findall(code[a:b]))
        names.update(n for s, c in strings.items() if a <= s < b for n in re.findall(r"\{(\w+)", c))

    for a, b in spans:
        if not re.match(r"\s*&?\s*(?:vec!\s*)?\[", code[a:b]):
            idents(a, b)
            continue
        open_at = code.index("[", a)
        places = 2
        for n, (ea, eb) in enumerate(split_top(code, open_at + 1, closing(code, open_at))):
            if n >= places:
                break
            lead = ea + len(code[ea:eb]) - len(code[ea:eb].lstrip())
            if lead not in strings:
                idents(ea, eb)
            elif strings[lead].startswith("-"):
                break
            elif n == 0 and strings[lead] in NO_RESOURCE.get(tool, set()):
                places = 1
    return names


def shellout_sites(root: Path, cfg: dict) -> tuple[list[dict], list[str], dict, dict]:
    """Each call that must name its kind, the findings on the way, the
    wrappers ((name, method): tools and parameter places), and the files."""
    spec = cfg["shellouts"]
    files = {}
    for path in tracked(root):
        if matches(path, spec["count"]) and not matches(path, spec.get("skip", [])):
            text = text_of(root, path) or ""
            code, strings = rust_lex(text)
            files[path] = {"text": text, "code": code, "strings": strings, "tests": test_regions(code),
                           "scopes": scopes(code), "at": {s: c for s, _, c in strings}}

    def in_tests(f: dict, i: int) -> bool:
        return any(a <= i <= b for a, b in f["tests"])

    def scope_of(f: dict, i: int) -> dict | None:
        inside = [s for s in f["scopes"] if s["span"][0] <= i <= s["span"][1]]
        return min(inside, key=lambda s: s["span"][1] - s["span"][0]) if inside else None

    findings: list[str] = []
    literal_sites = []
    for path, f in files.items():
        code = f["code"]
        for start, end, content in f["strings"]:
            if not TOOL_LITERAL.match(content) or in_tests(f, start):
                continue
            line = code.count("\n", 0, start) + 1
            j, depth = start - 1, 0
            while j >= 0 and not (code[j] in "([{;" and depth == 0):
                depth += 1 if code[j] in ")]}" else -1 if code[j] in "([{" else 0
                j -= 1
            if j < 0 or code[j] != "(":
                findings.append(f"S9: {path}:{line}: \"{content}\" outside a call: give it to the "
                                "call, so its kind is named where it is made")
                continue
            close = closing(code, j)
            if re.search(r"\bnew\s*$", code[:j]):
                lists, single, k = [], [], close + 1
                while m := re.match(r"\s*\.\s*(\w+)\s*\(", code[k:]):
                    o = k + m.end() - 1
                    {"args": lists, "arg": single}.get(m.group(1), []).append((o + 1, closing(code, o)))
                    k = closing(code, o) + 1
                spans = lists + single[:2]
            else:
                spans = [s for s in split_top(code, j + 1, close) if s[0] >= end][:1]
            last = max([close] + [b for _, b in spans])
            literal_sites.append({"path": path, "at": start, "line": line, "tools": {content},
                                  "spans": spans, "wrapper": None, "last": code.count("\n", 0, last) + 1})

    wrappers: dict[tuple[str, bool], dict] = {}
    while True:
        sites = list(literal_sites)
        for path, f in files.items():
            code = f["code"]
            for (name, method, within), wrapper in wrappers.items():
                # A closure is called within its function; a private fn within
                # its module and the modules under it; the rest anywhere.
                if within and not (path == within[0] or (len(within) == 2 and path.startswith(within[1]))):
                    continue
                for m in re.finditer(rf"\b{name}\b(?=\s*\()", code):
                    i = m.start()
                    before = code[max(0, i - 300):i]
                    dot = re.search(r"\.\s*$", before) is not None
                    if (in_tests(f, i) or re.search(r"\bfn\s+$", before)
                            or (within and len(within) == 3 and not within[1] <= i <= within[2])
                            or (method and not (dot or re.search(r"\bSelf\s*::\s*$", before)))
                            or (not method and dot)):
                        continue
                    o = code.index("(", m.end())
                    args = split_top(code, o + 1, closing(code, o))
                    last = code.count("\n", 0, closing(code, o)) + 1
                    shift = 1 if method and not dot else 0
                    sites.append({"path": path, "at": i, "line": code.count("\n", 0, i) + 1,
                                  "tools": wrapper["tools"], "wrapper": name, "last": last,
                                  "spans": [s for n, s in enumerate(args) if n - shift in wrapper["places"]]})
        grown, required = False, []
        for site in sites:
            f = files[site["path"]]
            scope = scope_of(f, site["at"])
            names: set[str] = set()
            for tool in site["tools"]:
                names |= kind_names(f["code"], f["at"], site["spans"], tool)
            places = {n for n, (p, stringlike) in enumerate(scope["params"]) if stringlike and p in names} \
                if scope else set()
            if not places:
                required.append(site)
                continue
            if scope["within"] == "module":
                stem = site["path"].removesuffix(".rs").removesuffix("/mod")
                within = (site["path"], "" if stem in ("src/main", "src/lib") else stem + "/")
            else:
                within = (site["path"], *scope["within"]) if scope["within"] else None
            known = wrappers.setdefault((scope["name"], scope["method"], within),
                                        {"tools": set(), "places": set()})
            if not (site["tools"] <= known["tools"] and places <= known["places"]):
                known["tools"] |= site["tools"]
                known["places"] |= places
                grown = True
        if not grown:
            break
    return required, list(dict.fromkeys(findings)), wrappers, files


def annotations_at(text: str, line: int, last: int) -> list[str]:
    """The kinds named for a call from this line to that: in an `// S9:`
    comment on its lines, on the lines of its statement above it, or just
    above that."""
    lines = text.splitlines()
    named = [kind.strip() for raw in lines[line:last] if (m := ANNOTATION.search(raw))
             for kind in m.group(1).split(";") if kind.strip()]
    k = line - 1
    while k >= 0:
        raw = lines[k]
        if k < line - 1:
            code = re.sub(r"//.*", "", raw).rstrip()
            if code and (code.endswith((";", "{", "}")) or ("=>" in code and code.endswith(","))):
                break
        if m := ANNOTATION.search(raw):
            named += [kind.strip() for kind in m.group(1).split(";") if kind.strip()]
        k -= 1
    return named


def read_kinds(text: str) -> dict[str, str]:
    """The kinds file: kind, a tab, and its reason (empty where none yet)."""
    kinds = {}
    for line in text.splitlines():
        if line.strip() and not line.startswith("#"):
            kind, _, reason = line.partition("\t")
            kinds[kind.strip()] = reason.strip()
    return kinds


def shellout_kinds(root: Path, cfg: dict) -> tuple[dict[str, list[str]], list[str]]:
    """Each kind named at a call, with its places, and the findings. A kind's
    words are looked for in the strings of the call's file, of the functions
    its function calls, and, for a wrapper's call, of the wrappers."""
    required, findings, wrappers, files = shellout_sites(root, cfg)

    def words_of(f: dict, a: int, b: int) -> set[str]:
        return {w for start, _, c in f["strings"] if a <= start <= b for w in WORD.findall(c.lower())}

    by_name: dict[str, set[str]] = {}
    wrapper_words: set[str] = set()
    for f in files.values():
        for s in f["scopes"]:
            found = words_of(f, *s["span"])
            by_name.setdefault(s["name"], set()).update(found)
            if any(k[:2] == (s["name"], s["method"]) for k in wrappers):
                wrapper_words |= found
    used: dict[str, list[str]] = {}
    for site in required:
        f = files[site["path"]]
        place = f"{site['path']}:{site['line']}"
        named = annotations_at(f["text"], site["line"], site["last"])
        what = f"this call of `{site['wrapper']}`" if site["wrapper"] else "this call"
        if not named:
            findings.append(f"S9: {place}: {what} names no kind; add `// S9: <tool> <verb> [<resource>]` "
                            "on its lines or above its statement")
            continue
        words = words_of(f, 0, len(f["code"]))
        inside = [s for s in f["scopes"] if s["span"][0] <= site["at"] <= s["span"][1]]
        for s in inside:
            for called in set(re.findall(r"\b(\w+)\s*\(", f["code"][s["span"][0]:s["span"][1]])):
                words |= by_name.get(called, set())
        if site["wrapper"]:
            words |= wrapper_words
        words |= {resource(w) for w in words}
        for kind in named:
            m = KIND.match(kind)
            if not m:
                findings.append(f"S9: {place}: `{kind}` is not a kind: <kubectl|helm> <verb> [<resource>]")
            elif m.group(1) not in site["tools"]:
                findings.append(f"S9: {place}: `{kind}` names {m.group(1)}; {what} runs "
                                f"{' or '.join(sorted(site['tools']))}")
            elif canonical(kind) != kind:
                findings.append(f"S9: {place}: write `{kind}` as `{canonical(kind)}`, by the "
                                "resource's plural name")
            elif missing := [w for w in m.group(2).split() if resource(w) not in words]:
                findings.append(f"S9: {place}: `{kind}`: {', '.join(missing)} is in no string of "
                                f"{site['path']} or of what it calls")
            else:
                used.setdefault(kind, []).append(place)
    return used, findings


def check_shellouts(root: Path, cfg: dict) -> list[str]:
    listed_at = cfg["shellouts"]["kinds"]
    listed = read_kinds(text_of(root, listed_at) or "")
    used, findings = shellout_kinds(root, cfg)
    for kind, places in sorted(used.items()):
        if kind not in listed:
            findings.append(f"S9: {places[0]}: `{kind}` is not in {listed_at}: call the deployment's API "
                            f"instead, or list it with its reason and a `{TRAILER}` trailer naming it")
    for kind in listed:
        if not KIND.match(kind):
            findings.append(f"S9: {listed_at}: `{kind}` is not a kind: <kubectl|helm> <verb> [<resource>]")
        elif canonical(kind) != kind:
            findings.append(f"S9: {listed_at}: `{kind}` is listed as `{canonical(kind)}`, by the "
                            "resource's plural name")
        elif kind not in used:
            findings.append(f"S9: {listed_at}: `{kind}` is no longer called: remove it, with a "
                            f"`{TRAILER}` trailer")
    return findings


# ── S2 ──────────────────────────────────────────────────────────────────────


def sizes(root: Path, cfg: dict) -> dict[str, int]:
    out = {}
    for path in tracked(root):
        if matches(path, cfg.get("size_exempt", [])):
            continue
        text = text_of(root, path)
        if text is not None:
            lines = text.count("\n")
            if lines > LIMIT:
                out[path] = lines
    return out


def read_size_baseline(root: Path, cfg: dict) -> dict[str, int]:
    held = {}
    for line in (root / cfg["size_baseline"]).read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.startswith("#"):
            path, lines = line.split("\t")
            held[path] = int(lines)
    return held


def check_size(root: Path, cfg: dict) -> list[str]:
    held = read_size_baseline(root, cfg)
    warnings = []
    for path, lines in sorted(sizes(root, cfg).items()):
        if path not in held:
            warnings.append(f"S2: {path} has {lines} lines, over {LIMIT}, and is not in "
                            f"{cfg['size_baseline']}: split it")
        elif lines > held[path]:
            warnings.append(f"S2: {path} grew from {held[path]} to {lines} lines, over {LIMIT}; "
                            "from v31b this needs a `Size-Exception:` trailer")
    return warnings


# ── S10 ─────────────────────────────────────────────────────────────────────

COPYLEFT = re.compile(r"GNU (Affero )?General Public License|SPDX-License-Identifier:\s*\(?\s*A?GPL")


def check_licences(root: Path, cfg: dict) -> list[str]:
    """What licence_scan names ships under Apache-2.0, whatever this
    repository's own licence: no GPL or AGPL text in it. With notice set,
    NOTICE ships beside LICENSE."""
    failures = []
    files = tracked(root)
    if cfg.get("notice") and "NOTICE" not in files:
        failures.append("S10: no NOTICE beside LICENSE")
    for path in files:
        if path == "LICENSE" or not matches(path, cfg.get("licence_scan", [])):
            continue
        text = text_of(root, path)
        if text and COPYLEFT.search(text):
            failures.append(f"S10: {path} carries a GPL or AGPL licence, and it ships under "
                            "Apache-2.0")
    return failures + crate_licences(root, files) + image_notices(root, cfg)


COPYLEFT_SPDX = re.compile(r"\bA?GPL")


def crate_licences(root: Path, files: list[str]) -> list[str]:
    """A crate whose licence is not GPL or AGPL (Apache-2.0, say) holds no
    GPL or AGPL text and depends on no crate of this repository that is GPL
    or AGPL: what it links is shipped with it. Dev-dependencies ship nothing."""
    manifests = {p: tomllib.loads(text_of(root, p) or "") for p in files if Path(p).name == "Cargo.toml"}
    workspace = manifests.get("Cargo.toml", {}).get("workspace", {}).get("package", {}).get("license", "")

    def licence(data: dict) -> str:
        held = data.get("package", {}).get("license", "")
        return workspace if isinstance(held, dict) and held.get("workspace") else str(held)

    crates = {posixpath.dirname(p): (data, licence(data)) for p, data in manifests.items()
              if "package" in data}
    failures = []
    for here, (data, own) in sorted(crates.items()):
        if not own or COPYLEFT_SPDX.search(own):
            continue
        name = data["package"].get("name", here)
        prefix = f"{here}/" if here else ""
        for path in files:
            if path.startswith(prefix) and not any(path.startswith(f"{d}/") for d in crates
                                                   if d != here and d.startswith(prefix)):
                text = text_of(root, path)
                if text and COPYLEFT.search(text):
                    failures.append(f"S10: {path} carries a GPL or AGPL licence, in {name}, "
                                    f"which ships under {own}")
        for key in ("dependencies", "build-dependencies"):
            for dep, spec in data.get(key, {}).items():
                if not isinstance(spec, dict) or "path" not in spec:
                    continue
                at = posixpath.normpath(posixpath.join(here, str(spec["path"])))
                theirs = crates.get("" if at == "." else at, ({}, ""))[1]
                if COPYLEFT_SPDX.search(theirs):
                    failures.append(f"S10: {name} ({own}) depends on {dep} ({theirs}) in [{key}]: "
                                    f"what it links ships under {theirs}")
    return failures


def dockerfile_stages(text: str) -> dict[str, list[str]]:
    """Each named stage's instructions, one per entry, continuations joined."""
    stages: dict[str, list[str]] = {}
    current: list[str] | None = None
    joined, held = [], ""
    for line in text.splitlines():
        if not held and line.lstrip().startswith("#"):
            continue
        held += line.rstrip()
        if held.endswith("\\"):
            held = held[:-1] + " "
            continue
        joined.append(held.strip())
        held = ""
    for instruction in joined:
        m = re.match(r"FROM\s+\S+(?:\s+AS\s+(\S+))?", instruction, re.I)
        if m:
            current = stages.setdefault(m.group(1) or "", [])
        elif current is not None and instruction:
            current.append(instruction)
    return stages


def image_notices(root: Path, cfg: dict) -> list[str]:
    """Each image named in `notices` ships the licence and notice files it
    names: `FILE` copied from the build context, `STAGE:PATH` copied from
    another stage (a bundled component's own)."""
    failures = []
    for entry in cfg.get("notices", []):
        dockerfile, stage = entry["dockerfile"], entry["stage"]
        instructions = dockerfile_stages(text_of(root, dockerfile) or "").get(stage)
        if instructions is None:
            failures.append(f"S10: {dockerfile} has no stage {stage!r} to hold its notices")
            continue
        copied: set[tuple[str, str]] = set()
        for instruction in instructions:
            words = instruction.split()
            if not words or words[0].upper() != "COPY":
                continue
            source = next((w.split("=", 1)[1] for w in words if w.startswith("--from=")), "")
            paths = [w for w in words[1:] if not w.startswith("--")]
            copied |= {(source, p) for p in paths[:-1]}
        for wanted in entry["files"]:
            source, _, path = wanted.rpartition(":")
            if (source, path) not in copied:
                where = f" from the {source} stage" if source else ""
                failures.append(f"S10: {dockerfile}'s {stage} image ships no {path}{where}: COPY it "
                                "into the image beside what it ships")
            elif not source and not (root / path).is_file():
                failures.append(f"S10: {dockerfile}'s {stage} image copies {path}, which is not here")
    return failures


# ── Baselines change in the open ────────────────────────────────────────────


def git(root: Path, *args: str) -> str:
    out = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    return out.stdout if out.returncode == 0 else ""


def check_baselines(root: Path, cfg: dict, base: str) -> list[str]:
    files = [cfg["size_baseline"], cfg["interims_register"]]
    for key in ("pins_allowlist",):
        if cfg.get(key):
            files.append(cfg[key])
    if cfg.get("shellouts"):
        files.append(cfg["shellouts"]["kinds"])
    if cfg.get("output_baseline"):
        files.append(cfg["output_baseline"])
    if not git(root, "rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"):
        return [f"the base {base} is not a commit here, so what changed cannot be read: fetch it"]
    changed = set(git(root, "diff", "--name-only", f"{base}...HEAD").split())
    watched = [f for f in files if f in changed and f != cfg["interims_register"]]
    failures = []
    trailers = [line for line in git(root, "log", "--format=%B", f"{base}..HEAD").splitlines()
                if line.startswith(TRAILER)]
    if watched and not trailers:
        failures.append(f"{', '.join(watched)} changed with no `{TRAILER}` trailer saying why")
    kinds_at = cfg.get("shellouts", {}).get("kinds")
    was = git(root, "show", f"{base}:{kinds_at}") if kinds_at in watched else ""
    if was:
        before, after = read_kinds(was), read_kinds(text_of(root, kinds_at) or "")
        for kind in sorted(set(after) - set(before)):
            if not after[kind]:
                failures.append(f"S9: {kinds_at}: `{kind}` is new and gives no reason; a new kind "
                                "enters with its reason")
            if not any(kind in line for line in trailers):
                failures.append(f"S9: `{kind}` is new in {kinds_at} with no `{TRAILER}` trailer line "
                                "naming it")
        unexplained = [sum(1 for reason in kinds.values() if not reason) for kinds in (before, after)]
        if unexplained[1] > unexplained[0]:
            failures.append(f"S9: {kinds_at} lists {unexplained[1]} kinds with no reason, up from "
                            f"{unexplained[0]}; that number only goes down")
    # A baseline new on this branch starts at today's figures: nothing to rise from.
    was = git(root, "show", f"{base}:{cfg['size_baseline']}")
    if cfg["size_baseline"] in watched and was:
        old = {l.split("\t")[0]: int(l.split("\t")[1]) for l in was.splitlines()
               if l.strip() and not l.startswith("#")}
        for path, lines in read_size_baseline(root, cfg).items():
            if lines > old.get(path, 0):
                failures.append(f"S2: {cfg['size_baseline']} raises {path} to {lines}; it only goes down")
    # The output budget (src/budget.rs measures it): each figure only goes down.
    at = cfg.get("output_baseline")
    was = git(root, "show", f"{base}:{at}") if at in watched else ""
    if was:
        def figures(text: str) -> dict[str, int]:
            return {l.split("\t")[0]: int(l.split("\t")[1]) for l in text.splitlines()
                    if l.strip() and not l.startswith("#")}
        old, new = figures(was), figures(text_of(root, at) or "")
        for name, figure in new.items():
            if name in old and figure > old[name]:
                failures.append(f"output: {at} raises {name} to {figure}; it only goes down")
    # The markers naming no reason: their number only goes down.
    was = git(root, "show", f"{base}:{cfg['interims_register']}")
    if was:
        before = sum(1 for line in was.splitlines()
                     if not line.startswith("#") and len(line.split("\t")) == 3
                     and not line.split("\t")[2].strip())
        found, _ = markers(root, tracked(root), [cfg["interims_register"], "tools/code_structure.py"])
        now = sum(1 for _, _, reason in found if not reason)
        if now > before:
            failures.append(f"S4: {now} INTERIM markers name no reason, up from {before}; that number "
                            "only goes down: write `INTERIM(vNN): reason`")
    for place, was, now in moved_interims(root, cfg, base):
        if not any(f"INTERIM({was})" in line for line in trailers):
            failures.append(f"S4: {place}: INTERIM({was}) moved to INTERIM({now}) with no "
                            f"`{TRAILER}` trailer line naming INTERIM({was}); moving a marker's "
                            "version needs the product owner's word and a trailer saying so")
    return failures


# ── self-test: each check red on a planted violation ────────────────────────


def self_test() -> int:
    cases, failed = 0, []

    def expect(name: str, cond: bool) -> None:
        nonlocal cases
        cases += 1
        if not cond:
            failed.append(name)

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        cfg = {"repository": "open-meridian/x", "roadmap_version": "v31a",
               "interims_register": "tools/interims.tsv", "size_baseline": "tools/size.tsv",
               "pins_allowlist": "tools/pins.txt", "notice": True,
               "licence_scan": ["src/*"], "size_exempt": ["gen/*"],
               "shellouts": {"count": ["src/*.rs"], "skip": ["src/*tests.rs"],
                             "kinds": "tools/kinds.tsv"}}

        def write(path: str, text: str) -> None:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text(text, encoding="utf-8")
            subprocess.run(["git", "-C", str(root), "add", path], check=True)

        a_rs = ('fn pods(m: &M, ns: &str) {{\n    // S9: kubectl get pods\n'
                '    m.run("kubectl", &["get", "pods", "-n", ns]);\n}}\n'
                '// INTERIM({}): the conductor\'s API, then gone\n'
                '#[cfg(test)]\nmod tests {{\n    fn t(m: &M) {{ m.run("helm", &["list"]); }}\n}}\n')
        write("src/a.rs", a_rs.format("v33"))
        write("src/a_tests.rs", 'run("helm", &["list"]);\n')
        write("tools/kinds.tsv", "# kind\treason\nkubectl get pods\tthe pods' state\n")
        write("tools/size.tsv", "# path\tlines\nsrc/big.py\t1600\n")
        write("src/big.py", "x\n" * 1600)
        write("tools/pins.txt", "")
        write("NOTICE", "n\n")
        write("Cargo.toml", '[dependencies]\nc = { git = "https://x/c", rev = "' + "a" * 40 + '" }\n')
        write(".github/workflows/ci.yml", "steps:\n  - uses: actions/checkout@v4\n")
        write("tools/interims.tsv", "")
        check_interims(root, cfg, write=True)
        subprocess.run(["git", "-C", str(root), "add", "-A"], check=True)

        expect("S4 holds on a later marker", check_interims(root, cfg, False) == [])
        expect("S9 holds on a named, listed kind; tests not counted", check_shellouts(root, cfg) == [])
        expect("S5 holds on a SHA and a version", check_pins(root, cfg) == [])
        expect("S2 is quiet at its baseline", check_size(root, cfg) == [])
        expect("S10 holds", check_licences(root, cfg) == [])

        write("src/b.rs", "// INTERIM(v31a): due now\n// INTERIM(v40)\n")
        said: list[str] = []
        found = check_interims(root, cfg, False, warnings=said)
        expect("S4: a due marker fails", any("INTERIM(v31a) is due" in f for f in found))
        expect("S4: a marker with no reason is said", any("names no reason" in f for f in said))
        write("src/b.rs", "// (INTERIM(v31a))\n")
        expect("S4: a marker with no reason is still due at its version",
               any("INTERIM(v31a) is due" in f for f in check_interims(root, cfg, False)))
        expect("S4: a stale register fails", any("is not the markers' register" in f for f in found))
        (root / "src/b.rs").unlink()
        subprocess.run(["git", "-C", str(root), "rm", "-q", "--cached", "src/b.rs"], check=True)
        cfg_files = dict(cfg, interims_register="tools/interims-files.tsv", interims_register_lines=False)
        check_interims(root, cfg_files, write=True)
        expect("S4: a register of files names no line",
               "\tsrc/a.rs\t" in (root / "tools/interims-files.tsv").read_text(encoding="utf-8"))
        write("src/a.rs", "// a line above\n" + a_rs.format("v33"))
        expect("S4: an edit above a marker leaves a register of files as it was",
               check_interims(root, cfg_files, False) == [])
        write("src/a.rs", a_rs.format("v33"))
        (root / "tools/interims-files.tsv").unlink()

        def s9(name: str, text: str, wanted: str) -> None:
            write("src/c.rs", text)
            expect(f"S9: {name}", any(wanted in f for f in check_shellouts(root, cfg)))
            write("src/c.rs", "")

        s9("a call that names no kind fails", 'fn c(m: &M) { m.run("helm", &["list"]); }\n',
           "src/c.rs:1: this call names no kind")
        s9("a kind not listed fails", 'fn c(m: &M) {\n    // S9: helm list\n    m.run("helm", &["list"]);\n}\n',
           "`helm list` is not in tools/kinds.tsv")
        s9("a kind the code does not have fails",
           'fn c(m: &M) {\n    // S9: kubectl get pods\n    m.run("kubectl", &["get", "secrets"]);\n}\n',
           "`kubectl get pods`: pods is in no string")
        s9("a kind named by its singular fails",
           'fn c(m: &M) {\n    // S9: kubectl get pod\n    m.run("kubectl", &["get", "pod"]);\n}\n',
           "write `kubectl get pod` as `kubectl get pods`")
        s9("a helper's call names its kind",
           'fn kube(m: &M, args: &[&str]) -> S { m.run("kubectl", args) }\n'
           'fn c(m: &M) { kube(m, &["get", "secrets"]); }\n', "src/c.rs:2: this call of `kube` names no kind")
        s9("a helper's helper's call names its kind",
           'fn kube(m: &M, args: &[&str]) -> S { m.run("kubectl", args) }\n'
           'fn again(m: &M, args: &[&str]) -> S { kube(m, args) }\n'
           'fn c(m: &M) { again(m, &["get", "secrets"]); }\n', "src/c.rs:3: this call of `again`")
        s9("a helper given the resource names it at its call",
           'fn listed(m: &M, what: &str) -> S { m.run("kubectl", &["get", what, "-o", "json"]) }\n'
           'fn c(m: &M) { listed(m, "secrets"); }\n', "src/c.rs:2: this call of `listed`")
        s9("a closure's call names its kind",
           'fn c(m: &M) {\n    // S9: kubectl get pods\n    let k = |args: &[&str]| m.run("kubectl", args);\n'
           '    k(&["get", "secrets"]);\n}\n', "src/c.rs:4: this call of `k`")
        s9("a tool's name outside a call fails", 'const K: &str = "kubectl";\n', "outside a call")
        write("src/c.rs", 'fn listed(m: &M, what: &str) -> S { m.run("kubectl", &["get", what, "-o", "json"]) }\n'
                          'fn c(m: &M) {\n    // S9: kubectl get pods\n    listed(m, "pods");\n}\n')
        expect("S9: a helper's call that names its kind holds", check_shellouts(root, cfg) == [])
        write("src/c.rs", "")
        write("src/a.rs", "// INTERIM(v33): the conductor's API, then gone\n")
        expect("S9: a kind no longer called fails",
               any("`kubectl get pods` is no longer called" in f for f in check_shellouts(root, cfg)))
        write("src/a.rs", a_rs.format("v33"))
        write("tools/kinds.tsv", "kubectl get pod\tthe pods' state\n")
        expect("S9: a kind listed by its singular fails",
               any("listed as `kubectl get pods`" in f for f in check_shellouts(root, cfg)))
        write("tools/kinds.tsv", "# kind\treason\nkubectl get pods\tthe pods' state\n")

        write("Cargo.toml", '[dependencies]\nc = { git = "https://x/c", branch = "main" }\n'
                            'd = { path = "../d" }\n')
        write(".github/workflows/ci.yml", "steps:\n  - uses: o/a@main\n  - uses: actions/checkout@v4\n"
                                          "    with:\n      repository: o/b\n      ref: main\n")
        write("pyproject.toml", '[project]\ndependencies = ["m @ git+https://x/m@main"]\n')
        found = check_pins(root, cfg)
        expect("S5: a git branch fails", any("[dependencies] c" in f for f in found))
        expect("S5: a sibling checkout fails", any("sibling checkout" in f and "../d" in f for f in found))
        expect("S5: an action at a branch fails", any("uses o/a@main" in f for f in found))
        expect("S5: another repository checked out at a branch fails",
               any("checks out o/b at main" in f for f in found))
        expect("S5: a pyproject git dependency at a branch fails", any("pyproject.toml" in f for f in found))
        write("crates/x/Cargo.toml", '[package]\nname = "x"\n[dependencies]\ny = { path = "../y" }\n')
        write(".github/workflows/sdk.yml",
              "steps:\n  - uses: actions/checkout@v4\n    with:\n"
              "      repository: ${{ github.repository_owner }}/c\n"
              "  - uses: actions/checkout@v4\n    with:\n"
              "      repository: ${{ github.repository_owner }}/s\n"
              "      ref: ${{ steps.sdk.outputs.ref }}\n"
              "  - uses: actions/checkout@v4\n    with:\n"
              "      repository: ${{ github.repository_owner }}/t\n"
              "      ref: ${{ steps.other.outputs.ref }}\n")
        write("tools/sdk.txt", "# the SDK's release\nmain\n")
        cfg_refs = dict(cfg, pins_refs={"steps.sdk.outputs.ref": "tools/sdk.txt"})
        found = check_pins(root, cfg_refs)
        expect("S5: a path inside the repository is a workspace member",
               not any("crates/x/Cargo.toml" in f for f in found))
        expect("S5: the owner's repository checked out at its default branch fails",
               any("checks out open-meridian/c at its default branch" in f for f in found))
        expect("S5: a ref read from a pin file holding a branch fails",
               any("checks out open-meridian/s at main (tools/sdk.txt)" in f for f in found))
        expect("S5: a ref from an expression no pin file names fails",
               any("checks out open-meridian/t at ${{ steps.other.outputs.ref }}" in f for f in found))
        write("tools/sdk.txt", "# the SDK's release\nv0.37.1\n")
        expect("S5: a ref read from a pin file holding a release holds",
               not any("open-meridian/s" in f for f in check_pins(root, cfg_refs)))
        write("crates/x/Cargo.toml", '[package]\nname = "x"\n[dependencies]\ny = { path = "../../../y" }\n')
        expect("S5: a path leaving the repository from a member is a sibling checkout",
               any("crates/x/Cargo.toml" in f and "sibling" in f for f in check_pins(root, cfg_refs)))
        write("crates/x/Cargo.toml", "")
        write(".github/workflows/sdk.yml", "")
        write("tools/pins.txt", "uses o/a@main # INTERIM(v33): o/a has no release yet\n"
                                "checks out o/b # no marker\n")
        found = check_pins(root, cfg)
        expect("S5: a listed exception with its marker is allowed",
               not any("uses o/a@main" in f for f in found))
        expect("S5: a listed exception with no marker fails", any("no `INTERIM" in f for f in found))
        write("Cargo.toml", "")
        write("pyproject.toml", "")
        write(".github/workflows/ci.yml", "")

        write("src/big.py", "x\n" * 1700)
        write("src/new.py", "x\n" * 1501)
        found = check_size(root, cfg)
        expect("S2: a listed file that grew is reported", any("grew from 1600 to 1700" in f for f in found))
        expect("S2: a new file over the limit is reported", any("src/new.py" in f for f in found))
        write("gen/huge.py", "x\n" * 5000)
        expect("S2: a generated file is exempt", not any("gen/huge.py" in f for f in check_size(root, cfg)))

        write("src/copied.py", "# SPDX-License-Identifier: AGPL-3.0-or-later\n")
        expect("S10: an AGPL file in an Apache repository fails",
               any("src/copied.py" in f for f in check_licences(root, cfg)))
        (root / "NOTICE").unlink()
        subprocess.run(["git", "-C", str(root), "rm", "-q", "--cached", "NOTICE"], check=True)
        expect("S10: a missing NOTICE fails", any("NOTICE" in f for f in check_licences(root, cfg)))
        write("NOTICE", "n\n")
        (root / "src/copied.py").unlink()
        subprocess.run(["git", "-C", str(root), "rm", "-q", "--cached", "src/copied.py"], check=True)

        write("Cargo.toml", '[workspace]\nmembers = ["crates/*"]\n'
                            '[workspace.package]\nlicense = "AGPL-3.0-or-later"\n')
        write("crates/core/Cargo.toml", '[package]\nname = "core"\nlicense.workspace = true\n')
        write("crates/kit/Cargo.toml", '[package]\nname = "kit"\nlicense = "Apache-2.0"\n'
                                       '[dev-dependencies]\ncore = { path = "../core" }\n')
        write("crates/kit/src/lib.rs", "pub fn f() {}\n")
        expect("S10: an Apache crate whose dev-dependency is AGPL holds", check_licences(root, cfg) == [])
        write("crates/kit/Cargo.toml", '[package]\nname = "kit"\nlicense = "Apache-2.0"\n'
                                       '[dependencies]\ncore = { path = "../core" }\n')
        expect("S10: an Apache crate depending on an AGPL crate fails",
               any("kit (Apache-2.0) depends on core (AGPL-3.0-or-later)" in f for f in check_licences(root, cfg)))
        write("crates/kit/Cargo.toml", '[package]\nname = "kit"\nlicense = "Apache-2.0"\n')
        write("crates/kit/src/lib.rs", "// SPDX-License-Identifier: AGPL-3.0-or-later\n")
        expect("S10: AGPL text in an Apache crate fails",
               any("crates/kit/src/lib.rs carries a GPL or AGPL licence, in kit" in f
                   for f in check_licences(root, cfg)))
        write("crates/core/src/lib.rs", "// SPDX-License-Identifier: AGPL-3.0-or-later\n")
        write("crates/kit/src/lib.rs", "pub fn f() {}\n")
        expect("S10: AGPL text in an AGPL crate holds", check_licences(root, cfg) == [])
        for path in ("Cargo.toml", "crates/core/Cargo.toml", "crates/kit/Cargo.toml"):
            write(path, "")

        cfg_img = dict(cfg, notices=[{"dockerfile": "Dockerfile", "stage": "runtime",
                                      "files": ["LICENSE", "NOTICE", "kit:/kit/NOTICE"]}])
        write("LICENSE", "l\n")
        write("Dockerfile", "FROM scratch AS kit\nCOPY k /kit/\n"
                            "FROM debian AS runtime\n# COPY LICENSE NOTICE /doc/\n"
                            "COPY --from=kit /kit/x /x\n")
        found = check_licences(root, cfg_img)
        expect("S10: an image that ships no LICENSE fails", any("runtime image ships no LICENSE" in f for f in found))
        expect("S10: a bundled component's NOTICE not shipped fails",
               any("ships no /kit/NOTICE from the kit stage" in f for f in found))
        write("Dockerfile", "FROM scratch AS kit\nCOPY k /kit/\n"
                            "FROM debian AS runtime\nCOPY LICENSE \\\n     NOTICE /doc/\n"
                            "COPY --from=kit /kit/x /kit/NOTICE /x/\n")
        expect("S10: an image shipping its notices holds", check_licences(root, cfg_img) == [])
        write("Dockerfile", "FROM debian\n")
        expect("S10: a stage that is not there fails",
               any("no stage 'runtime'" in f for f in check_licences(root, cfg_img)))
        write("Dockerfile", "")

        env = ["-c", "user.name=t", "-c", "user.email=t@t"]
        write("tools/kinds.tsv", "kubectl get pods\tthe pods' state\nhelm list\t\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm", "base"], check=True)
        write("tools/more.tsv", "")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm", "empty"], check=True)
        cfg_new = dict(cfg, size_baseline="tools/new-size.tsv")
        write("tools/new-size.tsv", "src/big.py\t1700\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm",
                        "start\n\nBaseline-Change: today's figures"], check=True)
        expect("a baseline new on the branch starts at today's figures",
               check_baselines(root, cfg_new, "HEAD~1") == [])
        write("tools/new-size.tsv", "src/big.py\t1800\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm",
                        "grow\n\nBaseline-Change: grew"], check=True)
        expect("S2: a baseline entry may not rise, trailer or not",
               any("raises src/big.py to 1800" in f for f in check_baselines(root, cfg_new, "HEAD~1")))
        cfg_out = dict(cfg, output_baseline="tools/output.tsv")
        write("tools/output.tsv", "root_help_lines\t50\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm",
                        "budget\n\nBaseline-Change: today's figures"], check=True)
        write("tools/output.tsv", "root_help_lines\t55\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm",
                        "longer\n\nBaseline-Change: longer help"], check=True)
        expect("output: a figure of the output budget may not rise, trailer or not",
               any("raises root_help_lines to 55" in f for f in check_baselines(root, cfg_out, "HEAD~1")))
        write("tools/output.tsv", "root_help_lines\t45\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm",
                        "shorter\n\nBaseline-Change: shorter help"], check=True)
        expect("output: a figure of the output budget may go down",
               check_baselines(root, cfg_out, "HEAD~1") == [])
        write("tools/kinds.tsv", "kubectl get pods\tthe pods' state\nhelm list\t\nhelm history\twhy\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-qm", "add a kind"], check=True)
        found = check_baselines(root, cfg, "HEAD~1")
        expect("a baseline changed with no trailer fails", any("no `Baseline-Change:`" in f for f in found))
        expect("S9: a new kind without a trailer naming it fails",
               any("`helm history` is new in tools/kinds.tsv with no" in f for f in found))
        subprocess.run(["git", *env, "-C", str(root), "commit", "-q", "--amend", "-m",
                        "add a kind\n\nBaseline-Change: helm history, the release's revisions"], check=True)
        expect("S9: a new kind with its reason and a trailer naming it holds",
               check_baselines(root, cfg, "HEAD~1") == [])
        write("tools/kinds.tsv", "kubectl get pods\tthe pods' state\nhelm list\t\nhelm history\t\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-q", "--amend", "-m",
                        "add a kind\n\nBaseline-Change: helm history"], check=True)
        found = check_baselines(root, cfg, "HEAD~1")
        expect("S9: a new kind with no reason fails, trailer or not",
               any("`helm history` is new and gives no reason" in f for f in found))
        expect("S9: the number of kinds with no reason may not rise",
               any("2 kinds with no reason, up from 1" in f for f in found))
        write("tools/kinds.tsv", "kubectl get pods\t\nhelm list\t\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-q", "--amend", "-m",
                        "drop a reason\n\nBaseline-Change: kubectl get pods"], check=True)
        expect("S9: a reason taken away counts against the ratchet",
               any("2 kinds with no reason, up from 1" in f for f in check_baselines(root, cfg, "HEAD~1")))
        write("tools/kinds.tsv", "kubectl get pods\tthe pods' state\n")
        subprocess.run(["git", *env, "-C", str(root), "commit", "-q", "--amend", "-m",
                        "lower it\n\nBaseline-Change: helm list moved to the API"], check=True)
        expect("a kind removed, with its trailer, holds", check_baselines(root, cfg, "HEAD~1") == [])
        expect("a base that is not there fails rather than passing unread",
               any("not a commit here" in f for f in check_baselines(root, cfg, "origin/nowhere")))

        def move(version: str, message: str) -> None:
            write("src/a.rs", a_rs.format(version))
            check_interims(root, cfg, write=True)
            subprocess.run(["git", "-C", str(root), "add", "-A"], check=True)
            subprocess.run(["git", *env, "-C", str(root), "commit", "-qm", message], check=True)

        move("v34", "later")
        expect("S4: a marker moved to a later version with no trailer fails",
               any("INTERIM(v33) moved to INTERIM(v34)" in f for f in check_baselines(root, cfg, "HEAD~1")))
        subprocess.run(["git", *env, "-C", str(root), "commit", "-q", "--amend", "-m",
                        "later\n\nBaseline-Change: the size baseline, lowered"], check=True)
        expect("S4: a trailer that names no INTERIM marker does not cover a move",
               any("moved to INTERIM(v34)" in f for f in check_baselines(root, cfg, "HEAD~1")))
        subprocess.run(["git", *env, "-C", str(root), "commit", "-q", "--amend", "-m",
                        "later\n\nBaseline-Change: INTERIM(v40) to v41, on the product owner's word"],
                       check=True)
        expect("S4: a trailer naming another marker does not cover a move",
               any("moved to INTERIM(v34)" in f for f in check_baselines(root, cfg, "HEAD~1")))
        subprocess.run(["git", *env, "-C", str(root), "commit", "-q", "--amend", "-m",
                        "later\n\nBaseline-Change: INTERIM(v33) to v34, on the product owner's word"],
                       check=True)
        expect("S4: a marker moved with its trailer holds", check_baselines(root, cfg, "HEAD~1") == [])
        move("v32", "sooner")
        expect("S4: a marker brought sooner needs no trailer", check_baselines(root, cfg, "HEAD~1") == [])
        write("src/d.rs", "// the stop-gap's end (INTERIM(v40))\n")
        move("v32", "a marker naming no reason")
        expect("S4: the markers naming no reason may not grow in number",
               any("1 INTERIM markers name no reason, up from 0" in f
                   for f in check_baselines(root, cfg, "HEAD~1")))
        write("src/d.rs", "")
        move("v32", "its reason")
        expect("S4: fewer markers naming no reason holds", check_baselines(root, cfg, "HEAD~1") == [])

    for name in failed:
        print(f"FAIL  {name}", file=sys.stderr)
    if failed:
        print(f"code-structure self-test FAILED: {len(failed)} of {cases}", file=sys.stderr)
        return 1
    print(f"code-structure self-test OK: {cases} cases, each check red on its planted violation")
    return 0


def main(argv: list[str]) -> int:
    if argv[:1] == ["--self-test"]:
        return self_test()
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    root = Path(__file__).resolve().parent.parent
    cfg = tomllib.loads((root / "tools" / "code-structure.toml").read_text(encoding="utf-8"))
    command = argv[0]
    if command == "interims":
        warnings: list[str] = []
        found = check_interims(root, cfg, write="--write" in argv, warnings=warnings)
        for warning in warnings:
            print(f"warning: {warning}", file=sys.stderr)
    elif command == "pins":
        found = check_pins(root, cfg)
    elif command == "shellouts":
        found = check_shellouts(root, cfg)
    elif command == "size":
        for warning in check_size(root, cfg):
            print(f"warning: {warning}", file=sys.stderr)
        print("check-size: reported (a warning until v31b)")
        return 0
    elif command == "licences":
        found = check_licences(root, cfg)
    elif command == "baselines":
        if "--base" not in argv:
            return 2
        found = check_baselines(root, cfg, argv[argv.index("--base") + 1])
    else:
        print(f"code-structure: no check {command!r}", file=sys.stderr)
        return 2
    for finding in found:
        print(finding, file=sys.stderr)
    if found:
        print(f"check-{command} FAILED: {len(found)} finding(s)", file=sys.stderr)
        return 1
    print(f"check-{command} OK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
