# Contributing

Contributions are welcome. Before your first one, read [CLA.md](CLA.md), the
contributor licence agreement: you keep your copyright, and give Societal Lab
Inc. the right to use your work in Open Meridian and to release it under other
licences, including commercial ones.

To agree, open a pull request from your own account that adds
`- @your-github-login` on its own line to [CONTRIBUTORS.md](CONTRIBUTORS.md).
Once it is merged, the check on your pull requests passes. Do the same in each
Open Meridian repository you contribute to.

Changes to the contract -- a protobuf message, a bus topic, a route -- start as
an issue, since they are decided before they are built.

Run `make ci-local` before pushing; `make install-hooks` makes that automatic.
