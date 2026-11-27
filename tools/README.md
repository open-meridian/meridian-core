# tools

`ci_mirror_check.py` is the canonical copy from `meridian-design/tools/`. It is
duplicated rather than shared because a gate that depends on a sibling checkout
cannot run on a fresh clone. Drift between copies is a bug; the canonical
version wins.

`code_structure.py` is the same file in every repository that runs the
code-structure rules (meridian-python, meridian-cli and here); this copy is
the CLI's with core's additions (S5's workspace paths and pin files, S10's
crates and images, S4's markers naming no reason and a register by file), each opt-in by a setting
or a no-op where a repository has nothing of the kind, so it can replace the
others' copies as it is.
