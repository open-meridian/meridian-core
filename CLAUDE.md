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

## Review

Every pull request gets the same passes, in the same order, whether a person or
an agent wrote it. Uniformity is the point: a review that varies by author is a
review whose absence is invisible.

Mechanical checks do not belong in a review. Formatting, link targets, the
derivation chain, codegen staleness and the task ledger are enforced by
`make ci-local`. Attention spent on something a gate could catch is a missing
gate, and the fix is to write the gate.

Run the passes in order. Report at the first pass that finds an Important
finding; later passes still run, but the finding does not wait.

1. **Correctness.** Does it do what the spec says, and does it fail safely when
   it does not? State that survives a restart when it should not, or does not
   when it should. Error paths that swallow rather than surface. Concurrency
   assumptions that hold only under test timing. Arithmetic on money that is not
   exact decimal.
2. **Security.** Credentials in the diff, in a log line, in a fixture or in a
   compose file. Anything widening what an agent session may do without a human
   in the loop. New outbound network calls. Changes to `.claude/settings.json`,
   to hooks or to permissions are read line by line, every time.
3. **Contract compliance.** Does the change respect the derivation direction? A
   build-stage pull request touching a workflow, the matrix, the topic registry
   or a proto is a finding on its own, whether or not the change is a good one.
   The route is a contract-revision task, not an edit.
4. **Spec and plan alignment.** Does the diff match the plan it was approved
   against, and does that plan trace to a spec and an intent? A diff that
   quietly grew beyond its plan is a finding even when every added line is
   sound, because the scope was never reviewed.

**Important** findings are ship-blocking: data loss, a security exposure, a
wrong result, a broken contract, or scope that was never approved. State the
concrete failure, meaning the inputs, the state, and what goes wrong. A finding
without a failure scenario is a Nit wearing a costume.

**Nit** is everything else. Cap Nits at five and drop the weakest past that. A
review returning thirty Nits trains its reader to skim.

Excluded from review entirely: generated files, vendored dependencies, and
anything a CI gate already enforces.

**Always post a result, including when every pass is clean.** Say which
passes ran and that nothing was found. Silence is indistinguishable from
never having run, and a review nobody can tell apart from an absent one is
the failure the review gate in this repo exists to catch. There is no diff
small enough to be worth staying quiet about.

People decide whether a finding merges or escalates, and whether the spec solves
the problem it claims to. Those stay with a named person.

> Canonical text: `meridian-design/REVIEW.md`. This copy exists because the
> reviewer runs in this repository and cannot read a private one. Drift between
> the two is a bug, not a variation.
