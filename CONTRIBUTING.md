# Contributing

Open Meridian is open source, and built by its team: we don't accept pull
requests from outside it. The best way to help is an issue.

- **Found a bug?** Open an issue with **Report a bug**: what you did, what you
  expected, and what happened instead.
- **Have an idea?** Open an issue with **Suggest an improvement**: the problem
  you have, and what would solve it for you.

Describe rather than paste code. We build every change ourselves, from your
description.

## Your suggestions

By opening an issue or making a suggestion, you give Societal Lab Inc. a
perpetual, irrevocable, worldwide, royalty-free licence to use it for any
purpose, with no obligation to you. That lets us act on what you tell us
without any question of who owns the result.

## Building on Open Meridian

Plugins are how you extend Open Meridian, and they live in your own
repositories, not ours: the Python SDK and the plugin contract are Apache-2.0,
so a plugin you write stays yours. Start with `meridian plugin new`; see
[open-meridian.dev](https://open-meridian.dev).

## For the team

Run `make ci-local` before pushing; `make install-hooks` makes that automatic.
