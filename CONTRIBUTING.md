# Contributing to meridian-core

## Before a pull request can be merged

If you work for Societal Lab Inc., nothing. Your employment settles who owns
what, and the check in CI reads GitHub's own view of who is inside the
organisation.

If you do not, you need to have agreed to the contribution terms once, ever,
across all of our repositories. Read `CLA.md`, then open a pull request adding
one line to `CONTRIBUTORS.md`:

    - @your-github-login

That pull request is the record of agreement, because it is made from your
account rather than being a note somebody wrote about you.

**`CLA.md` does not exist yet.** Until it does, outside contributions cannot be
merged, and the CI check says so rather than waving them through. If you have
arrived here wanting to contribute, open an issue and we will hurry it along.

## What the check does

It reads two files and nothing else. No bot, no hosted service, no token with
write access to this repository. A check that reads two files should not need a
credential to rotate.

## Licence

This repository is AGPL-3.0-or-later. Your contribution is offered under the same terms, and
the agreement covers what else may be done with it.

## Before you push

`make ci-local`. Green locally is the completion signal; CI is confirmation
rather than the first place a failure is found. A hook runs it on push, and
`make install-hooks` activates that on a fresh clone.
