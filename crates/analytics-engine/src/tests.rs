//! The engine against the SDK's reference (requirement 36), on one worker
//! and on four (requirement 25), and its inner loops counted for
//! allocations (requirement 31).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use serde_json::Value;

use crate::canonical::canonical;
use crate::case::Case;
use crate::exact::Dec;
use crate::graph::{Executor, Job, Sequential, Threads};
use crate::measures::{Part, Unit};
use crate::{compute, compute_with, historical, measures, model, pricing};

// ── Allocations, counted on the thread that asks ────────────────────────────

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static COUNTED: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|on| {
            if on.get() {
                let _ = COUNTED.try_with(|n| n.set(n.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = COUNTING.try_with(|on| {
            if on.get() {
                let _ = COUNTED.try_with(|n| n.set(n.get() + 1));
            }
        });
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// How many allocations `f` made on this thread.
fn allocations(f: impl FnOnce()) -> usize {
    COUNTED.with(|n| n.set(0));
    COUNTING.with(|on| on.set(true));
    f();
    COUNTING.with(|on| on.set(false));
    COUNTED.with(|n| n.get())
}

// ── The SDK's pinned cases ──────────────────────────────────────────────────

/// The cases, vendored from the SDK (reference_cases/SOURCE).
const CASES: [(&str, &str); 7] = [
    (
        "valuation",
        include_str!("../reference_cases/valuation.json"),
    ),
    (
        "stale-and-unconverted",
        include_str!("../reference_cases/stale-and-unconverted.json"),
    ),
    (
        "event-group",
        include_str!("../reference_cases/event-group.json"),
    ),
    (
        "event-group-not-exhaustive",
        include_str!("../reference_cases/event-group-not-exhaustive.json"),
    ),
    (
        "resolved-in-a-stablecoin",
        include_str!("../reference_cases/resolved-in-a-stablecoin.json"),
    ),
    (
        "historical-var-and-es",
        include_str!("../reference_cases/historical-var-and-es.json"),
    ),
    ("what-if", include_str!("../reference_cases/what-if.json")),
];

/// A case, held to its own canonical form as the SDK's `reference.load`
/// holds it: pinned byte for byte.
fn load(name: &str, raw: &str) -> Value {
    let case: Value = serde_json::from_str(raw).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(
        canonical(&case).unwrap(),
        raw.trim_end_matches('\n'),
        "{name} is not in RFC 8785 canonical form"
    );
    assert_eq!(case["case"], name, "{name}'s file names another case");
    case
}

#[test]
fn every_pinned_case_is_answered_as_the_reference_answers_it() {
    for (name, raw) in CASES {
        let case = load(name, raw);
        let answer = compute(&case["inputs"]).unwrap_or_else(|e| panic!("{name}: {e}"));
        let ours = canonical(&answer.to_json()).unwrap();
        let theirs = canonical(&case["answer"]).unwrap();
        if ours != theirs {
            let ours: Value = serde_json::from_str(&ours).unwrap();
            for (part, mine) in ours.as_object().unwrap() {
                assert_eq!(
                    serde_json::to_string_pretty(mine).unwrap(),
                    serde_json::to_string_pretty(&case["answer"][part]).unwrap(),
                    "{name}: the {part} differ from the reference's"
                );
            }
        }
        assert_eq!(
            answer.answer_hash(),
            case["answer"]["provenance"]["answer_hash"]
                .as_str()
                .unwrap(),
            "{name}: the hash differs"
        );
    }
}

/// An executor that sends every part through its JSON, as the bus carries
/// it between a worker and the coordinator.
struct OverTheWire;

impl Executor for OverTheWire {
    fn run(&self, job: &Job<'_>, units: &[Unit]) -> Vec<crate::case::Result<Part>> {
        Threads(4)
            .run(job, units)
            .into_iter()
            .map(|part| part.and_then(|p| Part::from_json(&p.to_json())))
            .collect()
    }
}

#[test]
fn one_worker_and_four_give_the_same_answer_and_hash() {
    for (name, raw) in CASES {
        let case = load(name, raw);
        let inputs = &case["inputs"];
        let one = compute_with(inputs, &Sequential).unwrap();
        let expected = canonical(&one.to_json()).unwrap();
        for executor in [
            &Threads(1) as &dyn Executor,
            &Threads(4),
            &Threads(16),
            &OverTheWire,
        ] {
            let other = compute_with(inputs, executor).unwrap();
            assert_eq!(canonical(&other.to_json()).unwrap(), expected, "{name}");
            assert_eq!(other.answer_hash(), one.answer_hash(), "{name}");
        }
    }
}

#[test]
fn the_hash_names_the_inputs_and_the_engine_version() {
    let case = load("valuation", CASES[0].1);
    let mut inputs = case["inputs"].clone();
    let first = compute(&inputs).unwrap();
    inputs["engine_version"] = Value::String("0.1.1".into());
    let second = compute(&inputs).unwrap();
    assert_ne!(first.answer_hash(), second.answer_hash());
    // The figures are the same; the provenance names the other version.
    assert_eq!(
        canonical(&first.to_json()["figures"]).unwrap(),
        canonical(&second.to_json()["figures"]).unwrap()
    );
    assert_eq!(second.provenance["engine_version"], "0.1.1");
}

#[test]
fn a_unit_is_named_by_its_inputs_and_cached_by_that_name() {
    let case = load("valuation", CASES[0].1);
    let inputs = &case["inputs"];
    let hash = crate::canonical::answer_hash(inputs).unwrap();
    let unit = Unit {
        measure: 0,
        account: Some("ACC-1".into()),
    };
    let name = crate::graph::unit_hash(&hash, &unit);
    assert_eq!(name, crate::graph::unit_hash(&hash, &unit.clone()));
    assert_ne!(
        name,
        crate::graph::unit_hash(
            &hash,
            &Unit {
                measure: 0,
                account: Some("ACC-2".into())
            }
        )
    );
    let parsed = Case::from_json(inputs).unwrap();
    let part = measures::compute(&parsed, &unit).unwrap();
    let cache = crate::graph::Cache::new(1);
    cache.put(name.clone(), part.clone());
    assert_eq!(
        canonical(&cache.get(&name).unwrap().to_json()).unwrap(),
        canonical(&part.to_json()).unwrap()
    );
    cache.put("another".into(), part);
    assert!(cache.get(&name).is_none(), "the oldest leaves first");
    assert_eq!(cache.len(), 1);
}

#[test]
fn a_document_naming_what_it_does_not_hold_is_refused_not_guessed() {
    let case = load("valuation", CASES[0].1);
    let mut inputs = case["inputs"].clone();
    inputs["instruments"] = Value::Array(Vec::new());
    assert!(compute(&inputs).unwrap_err().0.contains("no record"));
    let mut inputs = case["inputs"].clone();
    inputs["job"]["measures"][0]["version"] = Value::from(2);
    assert!(compute(&inputs).unwrap_err().0.contains("no definition"));
}

// ── The inner loops allocate nothing ────────────────────────────────────────

#[test]
fn the_revaluation_loop_allocates_nothing() {
    let case = load("historical-var-and-es", CASES[5].1);
    let parsed = Case::from_json(&case["inputs"]).unwrap();
    let key = &parsed.measures[0];
    let definition = measures::lookup(key).unwrap();
    let mut part = Part::default();
    let gathered = historical::gather(
        &parsed,
        key,
        definition,
        "ACC-1",
        crate::answer::BASE,
        &mut part,
    )
    .unwrap();
    let window = historical::window(definition);
    let bucket = gathered.buckets.values().next().unwrap();
    assert!(!bucket.holdings.is_empty());
    let mut work = historical::Workspace::new(window);
    let mut finite = false;
    let counted = allocations(|| {
        finite = historical::approximate(bucket, &gathered.table, window, &mut work);
    });
    assert!(finite);
    assert_eq!(counted, 0, "an iteration over 500 days allocated");
    // And the exact days the figure is taken from are the reference's.
    let worst = historical::worst_days(bucket, &gathered.table, window, 5, &mut work);
    assert_eq!(worst.len(), 5);
    assert_eq!(worst[4].negated().round_half_even(2).text(), "571.39");
}

#[test]
fn a_batch_of_pricings_allocates_nothing_in_its_loop() {
    let mut columns = pricing::Columns::default();
    let fx = [Dec::parse("1.0850").unwrap()];
    for i in 0..500 {
        let price = Dec::parse(&format!("{}.{:02}", 100 + i, i % 100)).unwrap();
        columns.push(
            &format!("INS-{i}"),
            price,
            if i % 2 == 0 { &fx } else { &[] },
        );
    }
    let mut out = vec![Dec::zero(); columns.len()];
    let counted = allocations(|| columns.price(&mut out));
    assert_eq!(counted, 0, "a batch iteration allocated");
    assert_eq!(out[0].text(), "108.500000");
    assert_eq!(out[1].text(), "101.01");
}

// ── The pricing seam equals the built-in models ─────────────────────────────

#[test]
fn a_units_market_objects_price_to_the_built_in_value() {
    for (name, raw) in CASES {
        let case = load(name, raw);
        let parsed = Case::from_json(&case["inputs"]).unwrap();
        for (account, base) in &parsed.bases {
            for position in parsed.positions.iter().filter(|p| &p.account_id == account) {
                let mut reads = crate::answer::Reads::default();
                let Ok(Ok(valued)) =
                    model::value(&parsed, &mut reads, &position.instrument_id, base)
                else {
                    continue;
                };
                let record = &parsed.instruments[&position.instrument_id];
                let inputs: Vec<_> = pricing::unit_inputs(&parsed, &position.instrument_id, base)
                    .unwrap()
                    .into_iter()
                    .map(|p| (p.subject_id, p.kind, p.amount, p.instrument_id))
                    .collect();
                let (value, held) =
                    model::value_per_unit(&record.model_id, &position.instrument_id, &inputs)
                        .unwrap();
                assert_eq!(&held, base, "{name}: {}", position.instrument_id);
                assert_eq!(value, valued.per_unit, "{name}: {}", position.instrument_id);
            }
        }
    }
}

/// Generated documents the SDK's reference answered, in the directory
/// `MERIDIAN_REFERENCE_FUZZ` names, each `{"inputs", "answer"}` (`answer`
/// null where the reference refuses the document): run by hand when the
/// engine changes, beside the pinned cases the gate runs.
#[test]
#[ignore = "reads generated documents from MERIDIAN_REFERENCE_FUZZ"]
fn generated_documents_are_answered_as_the_reference_answers_them() {
    let dir = std::env::var("MERIDIAN_REFERENCE_FUZZ")
        .expect("MERIDIAN_REFERENCE_FUZZ names a directory");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    let mut checked = 0;
    for path in paths {
        let held: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let ours = compute_with(&held["inputs"], &Threads(4));
        match (&held["answer"], ours) {
            (Value::Null, Err(_)) => {}
            (Value::Null, Ok(_)) => panic!(
                "{}: the reference refuses it, and the engine answered",
                path.display()
            ),
            (_, Err(e)) => panic!("{}: {e}", path.display()),
            (theirs, Ok(ours)) => {
                let mine = ours.to_json();
                for part in ["figures", "not_evaluated", "provenance"] {
                    assert_eq!(
                        serde_json::to_string_pretty(&mine[part]).unwrap(),
                        serde_json::to_string_pretty(&theirs[part]).unwrap(),
                        "{}: the {part} differ",
                        path.display()
                    );
                }
            }
        }
        checked += 1;
    }
    assert!(checked > 0, "no documents in {dir}");
}
