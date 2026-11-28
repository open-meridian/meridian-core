# meridian-core

The deployment runtime, in Rust. Message bus, sidecar, street store, instrument
store, conductor and dashboard. This is what ships to a client and what runs locally
with `docker compose up`.

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

"Kernel" here is the tier, not a component: the deployment's own core as
against plugins. Decision 012 freed the word by renaming the component that
held it, because in the v1 vocabulary `meridian_kernel` is the book of record
and this store holds the custodian's belief instead.

**Stateless compute, owned storage.** Anything a plugin's process holds can
vanish at any moment; the kernel is the seed on restart. A plugin holding an
edge role may also own the storage the deployment gives its instance, for its
raw external records, and rebuild from it; the launcher and the chart mount it
for that instance alone, never delete it, and no plugin's storage is a channel
to another (decisions/028).

## Code structure

Each rule is a check in `make check-structure`, which runs in the pre-push
gate and in CI's checks job and is red on a planted violation
(`make check-structure-selftest`). Settings, baselines and allowlists are in
tools/code-structure.toml and the files it names.

- **S3 generated is generated:** `make check-codegen` (the domain bindings,
  the book's transition table and the dictionary; change them with
  `make codegen`, never by hand), `make check-nats-permissions` (the broker's
  permissions; `make nats-permissions`), `make check-reference-cases` (the
  SDK's cases, byte for byte) and `make check-interims` (the register). A new
  generator adds its drift check.
- **S4 interims name their end:** every stop-gap carries `INTERIM(vNN): reason`.
  `make check-interims` keeps tools/interims.tsv as the markers' register, by
  file (run `python3 tools/code_structure.py interims --write` after adding,
  removing or rewording one) and fails on one due at `roadmap_version`. A marker naming no reason is reported,
  and their number only goes down. Moving a marker's version needs the product
  owner's word and a `Baseline-Change:` trailer naming the marker.
- **S5 pins are immutable:** `make check-pins`. Another repository by a release
  or a full SHA, never a branch or a sibling checkout: in Cargo.toml, pyproject
  and workflows, where a ref from a step's output is read from its pin file
  (tools/ci-sdk.txt). An exception goes in tools/pins-allowlist.txt with its
  marker.
- **S10 licences by layer:** `make check-licences`. Core is AGPL-3.0-or-later;
  a crate under another licence holds no GPL or AGPL text and depends on no
  AGPL crate. Each published image ships core's LICENSE and NOTICE, and the
  licence and notice of what it bundles (the UI kit's).
- **S2 file size:** `make check-size` reports a hand-written file over 1,500
  lines that is new, or has grown past tools/size-baseline.tsv (a warning now,
  failing from v31b; growth then needs a `Size-Exception:` trailer). Generated
  files are exempt.
- A baseline or allowlist changes only in a commit with a `Baseline-Change:`
  trailer saying why (`make check-baselines`); baselines only go down.

## Verification

    make pre-push

The fast gate (accepted 2026-11-07, superseding ruling 1075's full local
gate): lint, the build, the changed crates' unit tests and the stages the
changed paths select (`PRE_PUSH_MAP` in the Makefile). `make install-hooks`
wires it to `git push` on a fresh clone. CI on the pull request runs
everything, and the merge queue is the only authority for main. A failing CI
job is reproduced with `make ci-stage STAGE=<stage>` before pushing again,
never rerun until green; `make ci-local` runs every stage, for when CI is
down.

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
