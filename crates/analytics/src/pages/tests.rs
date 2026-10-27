//! A job's answer in pages (ruling 794), on an answer shaped as dev's: a
//! listed chain's positioning over a 45-day window, past 1 MiB.

use std::str::FromStr;

use meridian_bus::{read_refusal_status, RefusalStatus};
use meridian_domain::exact::Exact;
use meridian_domain::v1::*;
use prost::Message;

use super::*;

const JOB: &str = "JOB-01JAG0A1B2C30000000000002";
const HASH: &str = "sha256:3c5e7a9b1d3f5a7c9e1b3d5f7a9c1e3b5d7f9a1c3e5b7d9f1a9f2c4e6a8b0d1f";

fn d(text: &str) -> Option<meridian_pb::v1::Decimal> {
    Some(Exact::from_str(text).unwrap().to_wire())
}

fn osi(expiry: &str, right: char, strike: u32) -> String {
    format!("TQQQ  {expiry}{right}{:08}", strike * 1000)
}

fn key(symbol: &str) -> Vec<Identifier> {
    vec![Identifier {
        scheme: "occ_osi".into(),
        value: symbol.into(),
        ..Identifier::default()
    }]
}

fn provenance() -> AnswerProvenance {
    AnswerProvenance {
        lake_as_of_ns: 1_792_300_000_000_000_000,
        datasets: vec![
            "tradier-1:options".into(),
            "tradier-1:statistics".into(),
            "treasury-1:rates".into(),
        ],
        models: vec![ModelRef {
            model_id: "american_binomial".into(),
            model_version: 1,
            implementation: "built-in".into(),
            implementation_version: "0.1.0".into(),
        }],
        measures: vec![MeasureRef {
            definition: "positioning_exposure".into(),
            version: 1,
            ..MeasureRef::default()
        }],
        engine_version: "0.1.0".into(),
        answer_hash: HASH.into(),
        ..AnswerProvenance::default()
    }
}

/// `expiries` expirations, `strikes` strikes each, calls and puts, four
/// greeks a series, and every 25th series not evaluated.
fn chain_answer(expiries: usize, strikes: u32) -> JobAnswer {
    let mut answer = JobAnswer {
        job_id: JOB.into(),
        provenance: Some(provenance()),
        ..JobAnswer::default()
    };
    let measure = MeasureRef {
        definition: "positioning_exposure".into(),
        version: 1,
        breakdowns: vec!["expiry".into(), "greek".into(), "series".into()],
        ..MeasureRef::default()
    };
    let mut n = 0u32;
    for e in 0..expiries {
        let date = format!("2026-11-{:02}", 2 + e * 3);
        let expiry = format!("2611{:02}", 2 + e * 3);
        for strike in 0..strikes {
            for right in ['C', 'P'] {
                let symbol = osi(&expiry, right, 20 + strike);
                n += 1;
                if n.is_multiple_of(25) {
                    answer.not_evaluated.push(NotEvaluated {
                        measure: Some(measure.clone()),
                        reason: NotEvaluatedReason::PriceStale as i32,
                        detail: "tradier-1:options".into(),
                        age_ns: 1_900_000_000_000,
                        ceiling_ns: 900_000_000_000,
                        series_key: key(&symbol),
                        ..NotEvaluated::default()
                    });
                    continue;
                }
                for greek in ["gex", "dex", "vex", "chex"] {
                    answer.figures.push(Figure {
                        measure: Some(measure.clone()),
                        breakdown: vec![
                            BreakdownValue {
                                breakdown: "expiry".into(),
                                value: date.clone(),
                            },
                            BreakdownValue {
                                breakdown: "greek".into(),
                                value: greek.into(),
                            },
                            BreakdownValue {
                                breakdown: "series".into(),
                                value: symbol.clone(),
                            },
                        ],
                        scenario_key: Some(ScenarioKey::default()),
                        value: d(&format!("-1234567.{:06}", n)),
                        currency_instrument_id: "INS-01J8XQ4M7K00000000CASHUSD".into(),
                        computed_as: ComputedAs::Model as i32,
                        oldest_input_source_ns: 1_792_299_000_000_000_000,
                        series_key: key(&symbol),
                        ..Figure::default()
                    });
                }
            }
        }
    }
    answer
}

fn ask(cursor: &str, page_size: i32) -> ReadJobAnswerRequest {
    ReadJobAnswerRequest {
        job_id: JOB.into(),
        page_size,
        cursor: cursor.into(),
    }
}

/// Every page, read in turn from the first.
fn every_page(answer: &JobAnswer, page_size: i32, budget: usize) -> Vec<JobAnswer> {
    let mut pages = Vec::new();
    let mut cursor = String::new();
    loop {
        let read = page(answer, &ask(&cursor, page_size), budget).unwrap();
        cursor = read.next_cursor.clone();
        pages.push(read);
        if cursor.is_empty() {
            return pages;
        }
        assert!(pages.len() < 100_000, "a page always moves the reader on");
    }
}

fn reassembled(pages: &[JobAnswer]) -> JobAnswer {
    let mut whole = JobAnswer {
        job_id: pages[0].job_id.clone(),
        provenance: pages[0].provenance.clone(),
        ..JobAnswer::default()
    };
    for page in pages {
        whole.figures.extend(page.figures.iter().cloned());
        whole
            .not_evaluated
            .extend(page.not_evaluated.iter().cloned());
    }
    whole
}

fn refused_naming(refused: String) -> (RefusalStatus, Vec<String>) {
    let (status, fields, _) = read_refusal_status(&refused).expect("a refusal with a status");
    (status, fields)
}

#[test]
fn an_answer_past_a_mib_pages_and_reassembles_equal() {
    // Twelve expirations of 58 strikes, calls and puts: 5,348 figures and
    // 55 series not evaluated, about the 1,199,742 bytes dev's was.
    let answer = chain_answer(12, 58);
    let whole = answer.encoded_len();
    assert!(whole > 1_048_576, "{whole} bytes");
    assert!(!whole_fits(&answer, PAGE_BUDGET));
    let pages = every_page(&answer, 0, PAGE_BUDGET);
    let sizes: Vec<usize> = pages.iter().map(|p| p.encoded_len()).collect();
    eprintln!(
        "a {whole}-byte answer of {} figures and {} not evaluated: {} pages of {sizes:?} bytes",
        answer.figures.len(),
        answer.not_evaluated.len(),
        pages.len(),
    );
    assert!(
        pages.len() >= whole.div_ceil(PAGE_BUDGET),
        "{} pages",
        pages.len()
    );
    for page in &pages {
        assert!(page.encoded_len() <= PAGE_BUDGET - KEPT_FOR_THE_REST);
        assert_eq!(page.job_id, JOB);
        assert_eq!(
            page.provenance, answer.provenance,
            "the provenance on every page"
        );
    }
    // Every page but the last is near full.
    for size in &sizes[..sizes.len() - 1] {
        assert!(*size > PAGE_BUDGET - KEPT_FOR_THE_REST - 1_024, "{size}");
    }
    assert_eq!(reassembled(&pages), answer);
}

#[test]
fn the_first_page_reads_from_the_start_and_the_last_ends_with_an_empty_cursor() {
    let answer = chain_answer(12, 58);
    let pages = every_page(&answer, 0, PAGE_BUDGET);
    let first = &pages[0];
    assert_eq!(first.figures[0], answer.figures[0]);
    assert!(first.not_evaluated.is_empty(), "the figures first");
    assert!(first
        .next_cursor
        .starts_with(&format!("{JOB}/3c5e7a9b1d3f5a7c/")));
    let last = pages.last().unwrap();
    assert!(last.next_cursor.is_empty());
    assert_eq!(last.not_evaluated.last(), answer.not_evaluated.last());
    // The same cursor read twice answers the same page.
    let again = page(&answer, &ask(&first.next_cursor, 0), PAGE_BUDGET).unwrap();
    assert_eq!(again, pages[1]);

    // An answer within a page's budget is one page, as before.
    let small = chain_answer(1, 3);
    assert!(whole_fits(&small, PAGE_BUDGET));
    let one = page(&small, &ask("", 0), PAGE_BUDGET).unwrap();
    assert_eq!(one, small);
    assert!(one.next_cursor.is_empty());
}

#[test]
fn a_page_size_bounds_a_page_and_is_capped_at_500() {
    let answer = chain_answer(1, 30);
    let entries = answer.figures.len() + answer.not_evaluated.len();
    let first = page(&answer, &ask("", 3), PAGE_BUDGET).unwrap();
    assert_eq!(first.figures.len(), 3);
    assert_eq!(first.next_cursor, format!("{JOB}/3c5e7a9b1d3f5a7c/3"));
    let pages = every_page(&answer, 7, PAGE_BUDGET);
    assert_eq!(pages.len(), entries.div_ceil(7));
    // One page holds the last figures and the first not evaluated.
    assert!(pages
        .iter()
        .any(|p| !p.figures.is_empty() && !p.not_evaluated.is_empty()));
    assert_eq!(reassembled(&pages), answer);

    let big = chain_answer(12, 58);
    let capped = page(&big, &ask("", 100_000), PAGE_BUDGET).unwrap();
    assert_eq!(capped.figures.len(), 500);
    // The budget ends a page first where the size would pass it.
    let budgeted = page(&big, &ask("", 400), 16 * 1024).unwrap();
    assert!(budgeted.figures.len() < 400);
    assert!(budgeted.encoded_len() <= 16 * 1024);
}

#[test]
fn a_cursor_another_job_gave_or_none_gave_is_refused_naming_it() {
    let answer = chain_answer(1, 30);
    let other = "JOB-01JAG0A1B2C30000000000009/5d7f9a1c3e5b7d9f/2";
    for cursor in [
        other,
        "JOB-01JAG0A1B2C30000000000002/5d7f9a1c3e5b7d9f/2",
        "JOB-01JAG0A1B2C30000000000002/3c5e7a9b1d3f5a7c/100000",
        "JOB-01JAG0A1B2C30000000000002/3c5e7a9b1d3f5a7c/-1",
        "v/plugin-07/0.4.0",
        "anything",
    ] {
        let refused = page(&answer, &ask(cursor, 0), PAGE_BUDGET).unwrap_err();
        assert_eq!(
            refused_naming(refused),
            (RefusalStatus::InvalidArgument, vec!["cursor".to_string()]),
            "{cursor}"
        );
    }
    let refused = page(&answer, &ask(other, 0), PAGE_BUDGET).unwrap_err();
    assert!(
        refused.contains("JOB-01JAG0A1B2C30000000000009"),
        "{refused}"
    );
    let refused = page(&answer, &ask("", -1), PAGE_BUDGET).unwrap_err();
    assert_eq!(
        refused_naming(refused),
        (
            RefusalStatus::InvalidArgument,
            vec!["page_size".to_string()]
        )
    );
}

#[test]
fn an_entry_past_the_budget_is_a_page_of_its_own() {
    let answer = chain_answer(1, 2);
    let budget = KEPT_FOR_THE_REST + answer.provenance.as_ref().unwrap().encoded_len() + 64;
    let pages = every_page(&answer, 0, budget);
    let entries = answer.figures.len() + answer.not_evaluated.len();
    assert_eq!(pages.len(), entries);
    assert!(pages
        .iter()
        .all(|p| p.figures.len() + p.not_evaluated.len() == 1));
    assert_eq!(reassembled(&pages), answer);
}
