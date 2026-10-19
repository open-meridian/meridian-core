# The analytics engine's first benchmark (v26)

Kept with the release, as the first release's §5 asks: its budgets are budgets
to benchmark, not claims. The numbers below are measurements of this
engine, on one machine, on synthetic documents. They say nothing about the
stores' reads, which §5 expects to dominate and which the component makes
before the engine runs.

## How it was run

    cargo test -p meridian-analytics-engine --release bench -- --ignored --nocapture

`src/bench.rs` writes the documents: v26's universe (equities and funds in
USD, crypto in USDT, a tenth of equities in EUR, EUR and USDT converted leg
by leg to USD), 501 business dates of closes for every market object, a
deterministic draw so every run measures the same input. "Read" is the inputs
document parsed into the engine's case, "hash" the RFC 8785 canonical JSON
and its SHA-256 (the answer's hash), "compute" everything after. Every
number is the whole job's, not one figure's.

Machine: Apple M3 Pro (12 cores), macOS, Rust 1.90, release profile, 2026-10-18.
The test binary carries the allocation-counting allocator the engine's tests
use, which costs a thread-local read per allocation.

## Results

| Workload (ENG §5) | Budget | Measured |
|---|---|---|
| Interactive: one account, 500 positions, market value, exposure, 500-day VaR and ES | p95 ≤ 250 ms warm | read 75 ms, hash 104 ms; read + hash + compute p50 261 ms, p95 266 ms on one worker; p50 225 ms, p95 231 ms on four |
| The same job's what-if | p95 ≤ 100 ms over the base | p50 350 ms against the base's 261 ms: 89 ms over it |
| Event group: implied distribution (100 outcomes) and resolution payoff | p95 ≤ 50 ms warm | p50 0.7 ms, p95 0.8 ms |
| Batch: 1,000 accounts × 100 positions, six measures (market value, exposure, concentration, pnl, historical VaR, ES) over 501 dates | ≤ 60 s on the pool's floor | 14.1 s on one worker, 4.4 s on four (3.2× faster); answers and hashes identical |
| Scale-out | identical on 1 and 4 workers; near-linear on the batch | identical, held by a test on every pinned case and on the batch above; 3.2× on four |
| Inner loops | zero allocations per iteration | zero, held by the allocation-counting tests (the 500-day revaluation loop and a batch of 500 pricings) |
| Exact width (Q11) | the fast path against the wide one | 500 pricings, price × two legs: 10 ns each on the i128 fast path, 24 ns each on the 256-bit form |

Document sizes: the interactive job's inputs are 25 MB written out
canonically (250,000 closes), the batch's 62 MB. The whole benchmark peaks at
2.5 GB resident, holding the interactive, what-if and batch documents at once.

## What the numbers say

- **Reading and hashing the document cost more than computing it.** Of the
  interactive job's 261 ms, 179 ms is reading and hashing 25 MB; the
  revaluation over 500 days, exact, is about 80 ms. ENG §5's "warm" budget
  assumes price leaves cached by hash, which v26 does not have: the engine
  reads and hashes the whole document on every job. One worker misses the
  250 ms budget by 16 ms at p95; four meet it.
- **Exact VaR is cheap because only the k worst days are exact.** Every day
  is taken in floating point with a bound on its error, and only the days
  that could be among the k worst are computed as exact rationals, so the
  figure is the reference's to the digit (the SDK's pinned cases and 1,000
  generated documents agree) at a floating-point loop's cost.
- **The what-if recomputes its account whole.** It meets its budget, at
  89 ms over the base, but v26 does not recompute only the overridden nodes
  (requirement 26 states it as the aim): the what-if's units are computed
  in full.
- **A unit larger than the broker carries is computed by the coordinator.**
  A unit is sent to the pool with its document sliced to its account; a
  500-day history over 500 objects is 25 MB, past the broker's 1 MB message,
  so such a unit is computed in the coordinator, the same code to the same
  part. The pool takes the linear and event measures' units, and history
  jobs over small universes. Sharing large documents with the pool (a
  JetStream object store keyed by the inputs' hash, or units by scenario
  day) belongs to the hardening track.

## The chart's defaults, from these numbers (the v26 plan's question 245)

One coordinator and one worker, each requesting 100m CPU and 256 MiB and
limited to 2 GiB: an interactive job holds its 25 MB document as parsed
values (a few hundred MB), and a batch-sized job about ten times the
document. A deployment running batch-sized jobs raises the limit, or
`analytics.worker.replicaCount`, and the batch above shows what four
workers buy.
