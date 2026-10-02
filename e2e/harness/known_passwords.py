#!/usr/bin/env python3
"""No fixed password or hash in the plugin harness's files.

The harness once carried its admin's password and its Argon2id hash, the
database's password and every broker user's, written into its compose file
and so into every image that shipped it (kernel/the-plugin-harness-is-its-own-image).
It now draws each at random per run. This reads the files the harness image
holds, and the plugins file its `compose` writes, and fails on anything that
looks like one written down again:

  * a password-like variable given a literal value (`POSTGRES_PASSWORD: x`,
    `MERIDIAN_NATS_RUNTIME: x`, `..._PASSWORD_HASH=x`), where a value read
    at run time (`$...`) is fine;
  * a URL carrying a literal password (`postgres://user:secret@host`);
  * an Argon2 hash in PHC form (`$argon2id$v=19$m=...$<salt>$<hash>`).

Usage:  known_passwords.py FILE ...    exit 0 clean, 1 naming each line found
        known_passwords.py --self-test
"""
import re
import sys

FOUND = [
    ("a password variable with a literal value",
     re.compile(r"\b[A-Z0-9_]*(?:PASSWORD|PASSWD)(?:_HASH)?\b[\"']?\s*[:=]\s*[\"']?(?![$\s\"'])\S"
                r"|\bMERIDIAN_NATS_[A-Z0-9_]+\b[\"']?\s*[:=]\s*[\"']?(?![$\s\"'])\S")),
    ("a URL carrying a literal password",
     re.compile(r"\b[a-z][a-z0-9+.-]*://[^/\s:@\"'$]+:(?![$\s\"'])[^@\s\"']+@")),
    ("an Argon2 hash",
     re.compile(r"\$\$?argon2(?:id|i|d)\$\$?v=\d+\$\$?m=\d+,t=\d+,p=\d+\$\$?[A-Za-z0-9+/]{11,}")),
]


def findings(text):
    for number, line in enumerate(text.splitlines(), 1):
        for what, pattern in FOUND:
            if pattern.search(line):
                yield number, what


def self_test():
    caught = [
        "      POSTGRES_PASSWORD: example",
        "      MERIDIAN_NATS_PLUGIN_1: plugin-1-not-real",
        "      MERIDIAN_HARNESS_ADMIN_PASSWORD: Example1!",
        '      MERIDIAN_LOCAL_ACCOUNT_PASSWORD_HASH="$$argon2id$$v=19$$m=19456,t=2,p=1$$c2FsdHNhbHRzYWx0$$bm90YWhhc2hub3RhaGFzaG5vdGFoYXNo"',
        "x-database: &database postgres://meridian:example@postgres:5432/meridian",
        "      MERIDIAN_BROKER_URL: nats://runtime:example@nats:4222",
    ]
    clean = [
        "      POSTGRES_PASSWORD_FILE: /secrets/postgres",
        "      MERIDIAN_HARNESS_ADMIN_PASSWORD_FILE: /secrets/admin/password",
        "        export MERIDIAN_BROKER_URL=nats://runtime:$$broker@nats:4222",
        "        db=postgres://meridian:$$(cat /secrets/postgres)@postgres:5432/meridian",
        '        export MERIDIAN_LOCAL_ACCOUNT_PASSWORD_HASH="$$hash"',
        "        PGPASSWORD=$$(cat /secrets/postgres)",
        "          printf '$$argon2id$$v=19$$m=19456,t=2,p=1$$%s$$%s' \\",
        '          export "MERIDIAN_NATS_$$user=$$(cat "$$drawn")"',
        "      MERIDIAN_DASHBOARD_URL: http://dashboard:8080",
        'PASSWORD_FILE = os.environ.get("MERIDIAN_HARNESS_ADMIN_PASSWORD_FILE", "/secrets/admin/password")',
    ]
    missed = [line for line in caught if not list(findings(line))]
    wrong = [line for line in clean if list(findings(line))]
    for line in missed:
        print(f"known_passwords self-test: missed {line.strip()!r}", file=sys.stderr)
    for line in wrong:
        print(f"known_passwords self-test: wrongly found {line.strip()!r}", file=sys.stderr)
    return 1 if missed or wrong else 0


def main(argv):
    if argv == ["--self-test"]:
        return self_test()
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    failed = False
    for path in argv:
        with open(path, encoding="utf-8") as file:
            for number, what in findings(file.read()):
                # The line is not printed: what it holds may be the secret.
                print(f"{path}:{number}: {what}", file=sys.stderr)
                failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
