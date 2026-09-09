# tools

`ci_mirror_check.py` is the canonical copy from `meridian-design/tools/`. It is
duplicated rather than shared because a gate that depends on a sibling checkout
cannot run on a fresh clone. Drift between copies is a bug; the canonical
version wins.
