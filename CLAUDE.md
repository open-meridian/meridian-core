# meridian-core

The deployment runtime, in Rust. Message bus, sidecar, kernel, reference replica
and dashboard. This is what ships to a client and what runs locally with
`docker compose up`.

## Placement

Runtime only. Anything centrally operated belongs in the control plane, and the
boundary holds in both directions: deployment data never travels centrally, and
central concerns never grow a second home here. If something here needs data the
control plane owns, it fetches it over the documented surface.

## Rules with teeth

**Exact decimal for money.** Never floating point for a quantity, a price or a
balance, at any layer. Quantities cross the wire as scaled integers. This is the
class of code where rounding error becomes a reconciliation break, and a break
costs more to investigate than the discipline costs to keep.

**Cross-cutting competence lives in the sidecar, once.** Access control, codec,
health, lifecycle. SDKs stay thin. Do not reimplement any of it per plugin, and
do not let a plugin reach past its sidecar.

**Typed role facades are generated, never hand-written.** They are mechanical
wrappers derived from the function matrix. Hand-writing them produces thousands
of lines that must then be maintained by hand forever. Generate them, or do not
have them yet.

**No third-party dependencies in the kernel.** Translate external standards at
the plugin boundary. The kernel defines its own types and stays free to change
them.

**Plugins are ephemeral.** Anything a plugin holds can vanish at any moment. The
kernel is the seed on restart.

## Verification

    make ci-local

Local green is the completion signal; CI is confirmation, not the first place a
failure is discovered. `make install-hooks` wires that to `git push` on a fresh
clone. A diff touching a Dockerfile, a lock file, a workflow definition or a
`.proto` is promoted to `ci-local-deep` automatically.
