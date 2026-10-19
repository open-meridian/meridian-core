//! v26's first benchmark (ENG §5: budgets to benchmark, not claims), kept
//! with the release in BENCHMARK.md. Run by hand, optimised:
//!
//!     cargo test -p meridian-analytics-engine --release bench -- --ignored --nocapture
//!
//! Synthetic documents in v26's universe: equities and funds quoted in USD,
//! crypto in USDT, EUR and USDT converted leg by leg, 501 business dates of
//! closes for every object, and event groups. The engine alone is measured:
//! reading the document, hashing it, and computing; the stores' reads, which
//! ENG §5 expects to dominate, are the component's and are not here.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::canonical;
use crate::case::Case;
use crate::exact::Dec;
use crate::graph::{Sequential, Threads};
use crate::{compute_with, pricing};

const USD: &str = "INS-CASHUSD";
const EUR: &str = "INS-CASHEUR";
const USDT: &str = "INS-USDT";

/// A deterministic sequence, so every run measures the same documents.
struct Draws(u64);

impl Draws {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn between(&mut self, low: u64, high: u64) -> u64 {
        low + self.next() % (high - low)
    }
}

fn instrument(id: &str, model: &str, class: &str, currency: &str, kind: &str) -> Value {
    json!({
        "instrument_id": id, "record_version": "1", "template_id": format!("TPL-{class}"),
        "template_version": "1", "calculation_version": "1", "model_id": model,
        "model_version": 1, "asset_class": class, "currency_instrument_id": currency,
        "price_kind": kind, "payout": if model == "binary_event" {
            json!({"amount": "1.00", "instrument_id": USD})
        } else { Value::Null }, "country": "US",
    })
}

fn price(subject: &str, kind: &str, amount: String, instrument: &str) -> Value {
    json!({"subject_id": subject, "kind": kind, "amount": amount, "instrument_id": instrument,
           "dataset": "bench-1:daily", "valid_at_ns": "1792299990000000000"})
}

/// A document: `accounts` accounts of `positions` positions each over a
/// universe of `universe` instruments, with `days` history dates (0 for
/// none), the measures named, and optionally a what-if and an event group of
/// `outcomes` members.
fn document(
    accounts: usize,
    positions: usize,
    universe: usize,
    days: usize,
    measures: &[(&str, &[&str])],
    what_if: bool,
    outcomes: usize,
) -> Value {
    let mut draws = Draws(0x2545_F491_4F6C_DD1D);
    let mut instruments = vec![
        instrument(USD, "cash", "cash", USD, ""),
        instrument(EUR, "cash", "cash", EUR, ""),
        instrument(USDT, "cash", "cash", USDT, ""),
    ];
    let mut market = vec![
        price(EUR, "close", "1.0850".into(), USD),
        price(USDT, "close", "0.9998".into(), USD),
    ];
    let mut subjects = vec![EUR.to_string(), USDT.to_string()];
    for i in 0..universe {
        let id = format!("INS-B{i:05}");
        let (class, currency, kind) = match i % 10 {
            0 => ("crypto_asset", USDT, "close"),
            1 => ("fund", USD, "nav"),
            2 => ("equity", EUR, "close"),
            _ => ("equity", USD, "close"),
        };
        instruments.push(instrument(&id, "quoted_price", class, currency, kind));
        let amount = format!("{}.{:02}", draws.between(5, 900), draws.between(0, 100));
        market.push(price(&id, kind, amount, currency));
        subjects.push(id);
    }
    let mut members = Vec::new();
    for i in 0..outcomes {
        let id = format!("INS-E{i:04}");
        instruments.push(instrument(
            &id,
            "binary_event",
            "event_contract",
            USD,
            "last",
        ));
        let bid = draws.between(1, 90);
        market.push(price(&id, "bid", format!("0.{bid:02}"), USD));
        market.push(price(&id, "ask", format!("0.{:02}", bid + 2), USD));
        market.push(price(&id, "last", format!("0.{:02}", bid + 1), USD));
        members.push(id);
    }
    let mut rows = Vec::new();
    let mut ids = Vec::new();
    for a in 0..accounts {
        let account = format!("ACC-{a:05}");
        ids.push(account.clone());
        rows.push(json!({"account_id": account, "base_currency_instrument_id": USD}));
    }
    let mut held = Vec::new();
    for account in &ids {
        held.push(
            json!({"account_id": account, "instrument_id": USD, "quantity": "25000.00",
                         "cost": "25000.00", "sleeve": "core"}),
        );
        for p in 0..positions.saturating_sub(1) {
            let i = (draws.next() as usize + p) % universe.max(1);
            let quantity = draws.between(1, 2_000) as i64 * if p % 17 == 0 { -1 } else { 1 };
            held.push(
                json!({"account_id": account, "instrument_id": format!("INS-B{i:05}"),
                             "quantity": quantity.to_string(),
                             "cost": format!("{}.00", quantity * 100),
                             "sleeve": if p % 3 == 0 { "hedge" } else { "core" }}),
            );
        }
        for (m, member) in members.iter().enumerate().take(3) {
            held.push(json!({"account_id": account, "instrument_id": member,
                             "quantity": format!("{}", 100 * (m + 1)), "cost": "42.00", "sleeve": ""}));
        }
    }
    let dates: Vec<String> = (0..days).map(|d| format!("D{d:04}")).collect();
    let mut closes = Vec::new();
    if days > 0 {
        for subject in &subjects {
            let mut level = draws.between(50, 500) as f64;
            for date in &dates {
                level *= 1.0 + ((draws.next() % 2001) as f64 - 1000.0) / 50_000.0;
                closes.push(
                    json!({"subject_id": subject, "amount": format!("{level:.2}"),
                                   "instrument_id": USD, "business_date": date}),
                );
            }
        }
    }
    let group_id = "EVG-BENCH";
    json!({
        "engine_version": "0.1.0",
        "job": {
            "account_ids": ids,
            "measures": measures.iter().map(|(definition, breakdowns)| json!({
                "definition": definition, "version": 1, "breakdowns": breakdowns,
                "event_group_id": if matches!(*definition, "resolution_payoff" | "implied_distribution") { group_id } else { "" },
            })).collect::<Vec<_>>(),
            "what_if": if what_if { json!({"account_id": "ACC-00000", "trades": [
                {"instrument_id": "INS-B00003", "side": "SIDE_BUY", "quantity": "100", "price": null},
                {"instrument_id": "INS-B00004", "side": "SIDE_SELL", "quantity": "50",
                 "price": {"amount": "101.25", "instrument_id": USD}},
            ]}) } else { Value::Null },
        },
        "book_watermark": [{"partition": "P0", "sequence": "1"}],
        "lake_as_of_ns": "1792300000000000000",
        "routing_version": "1",
        "accounts": rows,
        "positions": held,
        "closed_lots": [],
        "instruments": instruments,
        "market": market,
        "history": {"dates": dates, "closes": closes},
        "outcomes": [],
        "event_groups": if outcomes > 0 { json!([{"event_group_id": group_id, "version": "1",
            "member_instrument_ids": members, "mutually_exclusive": "EVENT_GROUP_FLAG_TRUE",
            "exhaustive": "EVENT_GROUP_FLAG_TRUE"}]) } else { json!([]) },
        "stale_after_ns": {},
    })
}

/// The p50 and p95 of `runs` timings of `f`.
fn timed(runs: usize, mut f: impl FnMut()) -> (Duration, Duration) {
    let mut taken: Vec<Duration> = (0..runs)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed()
        })
        .collect();
    taken.sort();
    (taken[runs / 2], taken[(runs * 95).div_ceil(100) - 1])
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1_000.0)
}

#[test]
#[ignore = "the benchmark: run optimised, by hand (see the module)"]
fn bench() {
    let interactive = [
        ("market_value", &[] as &[&str]),
        ("exposure", &["asset_class"]),
        ("historical_var", &[]),
        ("expected_shortfall", &[]),
    ];

    // 1. Interactive: one account, 500 positions over 500 instruments,
    //    market value, exposure, 500-day VaR and ES.
    let doc = document(1, 500, 500, 501, &interactive, false, 0);
    let (read50, read95) = timed(20, || {
        Case::from_json(&doc).unwrap();
    });
    let (hash50, hash95) = timed(20, || {
        canonical::answer_hash(&doc).unwrap();
    });
    let (all50, all95) = timed(20, || {
        compute_with(&doc, &Sequential).unwrap();
    });
    let (four50, four95) = timed(20, || {
        compute_with(&doc, &Threads(4)).unwrap();
    });
    println!(
        "BENCH interactive 1x500, 501 dates: read p50 {} p95 {}; hash p50 {} p95 {}; \
              read+hash+compute, one worker p50 {} p95 {}; four p50 {} p95 {}",
        ms(read50),
        ms(read95),
        ms(hash50),
        ms(hash95),
        ms(all50),
        ms(all95),
        ms(four50),
        ms(four95)
    );

    // 2. The same job with a what-if: the what-if's figures beside the base.
    let what_if = document(1, 500, 500, 501, &interactive, true, 0);
    let (wi50, wi95) = timed(20, || {
        compute_with(&what_if, &Sequential).unwrap();
    });
    println!(
        "BENCH what-if over the same: p50 {} p95 {} (base p50 {})",
        ms(wi50),
        ms(wi95),
        ms(all50)
    );

    // 3. Event group: 100 outcomes, implied distribution and resolution
    //    payoff.
    let event = document(
        1,
        10,
        10,
        0,
        &[("implied_distribution", &[]), ("resolution_payoff", &[])],
        false,
        100,
    );
    let (ev50, ev95) = timed(50, || {
        compute_with(&event, &Sequential).unwrap();
    });
    println!(
        "BENCH event group, 100 outcomes: p50 {} p95 {}",
        ms(ev50),
        ms(ev95)
    );

    // 4. Batch: 1,000 accounts x 100 positions, every linear measure; VaR
    //    and ES over 501 dates, on one worker and on four.
    let batch_measures = [
        ("market_value", &["asset_class"] as &[&str]),
        ("exposure", &["currency"]),
        ("concentration", &["instrument"]),
        ("pnl", &["sleeve"]),
        ("historical_var", &[]),
        ("expected_shortfall", &[]),
    ];
    let batch = document(1_000, 100, 1_000, 501, &batch_measures, false, 0);
    let started = Instant::now();
    let one = compute_with(&batch, &Sequential).unwrap();
    let one_took = started.elapsed();
    let started = Instant::now();
    let four = compute_with(&batch, &Threads(4)).unwrap();
    let four_took = started.elapsed();
    assert_eq!(one.answer_hash(), four.answer_hash());
    assert_eq!(
        canonical::canonical(&one.to_json()).unwrap(),
        canonical::canonical(&four.to_json()).unwrap()
    );
    println!(
        "BENCH batch 1000x100, six measures, 501 dates: one worker {}, four {} \
              (speed-up {:.2}x; identical answers and hash)",
        ms(one_took),
        ms(four_took),
        one_took.as_secs_f64() / four_took.as_secs_f64()
    );

    // 5. Exact arithmetic: the i128 fast path against the 256-bit form, one
    //    batch of 500 pricings, price x two legs x quantity.
    let mut columns = pricing::Columns::default();
    let mut wide = pricing::Columns::default();
    let legs = [Dec::parse("1.0850").unwrap(), Dec::parse("0.9998").unwrap()];
    let wide_legs: Vec<Dec> = legs.iter().map(Dec::widened).collect();
    for i in 0..500 {
        let amount = Dec::parse(&format!("{}.{:06}", 100 + i, i)).unwrap();
        columns.push("x", amount.clone(), &legs);
        wide.push("x", amount.widened(), &wide_legs);
    }
    let mut out = vec![Dec::zero(); 500];
    let (fast50, _) = timed(200, || columns.price(&mut out));
    let (wide50, _) = timed(200, || wide.price(&mut out));
    println!("BENCH 500 pricings: i128 fast path p50 {:.1} us ({:.0} ns each); 256-bit p50 {:.1} us ({:.0} ns each)",
             fast50.as_secs_f64() * 1e6, fast50.as_secs_f64() * 1e9 / 500.0,
             wide50.as_secs_f64() * 1e6, wide50.as_secs_f64() * 1e9 / 500.0);
    println!(
        "BENCH document sizes: interactive {} bytes canonical; batch {} bytes",
        canonical::canonical(&doc).unwrap().len(),
        canonical::canonical(&batch).unwrap().len()
    );
}
