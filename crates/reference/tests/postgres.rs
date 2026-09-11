//! The Postgres store, against Postgres.
//!
//! Not against a fake. A store implementation tested against a stand-in tests
//! the stand-in, and every property worth having here -- the version gate under
//! concurrency, an identifier set replaced rather than merged, a dated window
//! evaluated in SQL -- is a property of the database rather than of the Rust
//! around it.
//!
//! These do not run under `make test`, which has no database. `make test-store`
//! runs them through compose. They fail loudly when the database is missing
//! rather than skipping, because a suite that quietly runs nothing is the
//! failure this project keeps finding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_reference::store::{Applied, Identifier, Instrument, Store};
use meridian_reference::PostgresStore;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// An identifier no other test uses, so these can run in parallel against one
/// database without a truncate between them.
fn unique(tag: &str) -> String {
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{tag}-{now}-{seq}")
}

fn store() -> PostgresStore {
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").expect(
        "MERIDIAN_TEST_DATABASE_URL is not set. These tests need a real Postgres; \
         run them with `make test-store`.",
    );

    let store = PostgresStore::connect(&url, 4).expect("could not reach the test database");
    store.migrate().expect("could not create the schema");
    store
}

fn identifier(scheme: &str, value: &str, source: &str, valid_from_ns: i64) -> Identifier {
    Identifier {
        scheme: scheme.into(),
        value: value.into(),
        source: source.into(),
        valid_from_ns,
        valid_to_ns: None,
    }
}

fn instrument(instrument_id: &str, identifiers: Vec<Identifier>, version: i64) -> Instrument {
    Instrument {
        instrument_id: instrument_id.into(),
        identifiers,
        asset_class: "EQUITY".into(),
        currency: "USD".into(),
        exchange_mic: "XNAS".into(),
        description: "Apple Inc. common stock".into(),
        lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
        version,
        valid_from_ns: 100,
        record_time_ns: 100,
    }
}

#[test]
fn an_applied_record_comes_back_whole() {
    let store = store();
    let id = unique("INS");
    let figi = unique("BBG");

    assert_eq!(
        store
            .apply(instrument(&id, vec![identifier("figi", &figi, "", 100)], 1))
            .unwrap(),
        Applied::Stored
    );

    let held = store.by_id(&id).unwrap().unwrap();
    assert_eq!(held.version, 1);
    assert_eq!(held.description, "Apple Inc. common stock");
    assert_eq!(held.identifiers.len(), 1);
    assert_eq!(held.identifiers[0].value, figi);
    assert_eq!(held.identifiers[0].valid_to_ns, None);
}

#[test]
fn the_version_gate_behaves_the_way_the_in_memory_store_does() {
    let store = store();
    let id = unique("INS");
    let figi = unique("BBG");
    let record = |version| instrument(&id, vec![identifier("figi", &figi, "", 100)], version);

    assert_eq!(store.apply(record(4)).unwrap(), Applied::Stored);
    assert_eq!(store.apply(record(4)).unwrap(), Applied::AlreadyCurrent);
    assert_eq!(store.apply(record(2)).unwrap(), Applied::AlreadyCurrent);
    assert_eq!(store.apply(record(9)).unwrap(), Applied::Stored);
    assert_eq!(store.version_of(&id).unwrap(), Some(9));
}

#[test]
fn concurrent_applies_leave_the_highest_version_and_one_row() {
    // The reason the gate is one statement rather than a read and a write. A
    // row that does not exist yet locks nothing, so `SELECT ... FOR UPDATE`
    // would let two inserts of a new instrument both through.
    let store = Arc::new(store());
    let id = unique("INS");
    let figi = unique("BBG");

    let mut racing = Vec::new();
    for version in 1..=16 {
        let store = store.clone();
        let id = id.clone();
        let figi = figi.clone();
        racing.push(std::thread::spawn(move || {
            store
                .apply(instrument(
                    &id,
                    vec![identifier("figi", &figi, "", 100)],
                    version,
                ))
                .unwrap()
        }));
    }
    for thread in racing {
        thread.join().unwrap();
    }

    assert_eq!(store.version_of(&id).unwrap(), Some(16));
    assert_eq!(store.matching("figi", &figi, "", 300).unwrap().len(), 1);
}

#[test]
fn an_identifier_resolves_only_while_its_window_covers_the_moment() {
    let store = store();
    let id = unique("INS");
    let figi = unique("BBG");

    let mut retired = instrument(&id, vec![identifier("figi", &figi, "", 100)], 2);
    retired.identifiers[0].valid_to_ns = Some(500);
    store.apply(retired).unwrap();

    assert_eq!(store.matching("figi", &figi, "", 300).unwrap().len(), 1);
    assert!(store.matching("figi", &figi, "", 900).unwrap().is_empty());
    assert!(store.matching("figi", &figi, "", 50).unwrap().is_empty());
}

#[test]
fn a_scoped_identifier_does_not_answer_for_a_global_one() {
    let store = store();
    let id = unique("INS");
    let symbol = unique("SYM");

    store
        .apply(instrument(
            &id,
            vec![identifier("symbol", &symbol, "snaptrade", 100)],
            1,
        ))
        .unwrap();

    assert!(store
        .matching("symbol", &symbol, "", 300)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .matching("symbol", &symbol, "snaptrade", 300)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn two_instruments_on_one_identifier_both_come_back_in_a_stable_order() {
    // Ambiguity has to be visible to the step that refuses it, and it has to
    // name the same pair twice or nobody can reproduce the incident.
    let store = store();
    let symbol = unique("SYM");
    let first = format!("INS-A-{symbol}");
    let second = format!("INS-B-{symbol}");

    for id in [&second, &first] {
        store
            .apply(instrument(
                id,
                vec![identifier("symbol", &symbol, "snaptrade", 100)],
                1,
            ))
            .unwrap();
    }

    let found = store.matching("symbol", &symbol, "snaptrade", 300).unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].instrument_id, first);
    assert_eq!(found[1].instrument_id, second);
}

#[test]
fn a_later_version_replaces_the_identifier_set_rather_than_adding_to_it() {
    // An amend is authoritative. An identifier absent from the new version is
    // absent, which is the whole reason a delta was not the verb.
    let store = store();
    let id = unique("INS");
    let figi = unique("BBG");
    let dropped = unique("SYM");

    store
        .apply(instrument(
            &id,
            vec![
                identifier("figi", &figi, "", 100),
                identifier("symbol", &dropped, "snaptrade", 100),
            ],
            1,
        ))
        .unwrap();

    store
        .apply(instrument(&id, vec![identifier("figi", &figi, "", 100)], 2))
        .unwrap();

    assert_eq!(store.by_id(&id).unwrap().unwrap().identifiers.len(), 1);
    assert!(store
        .matching("symbol", &dropped, "snaptrade", 300)
        .unwrap()
        .is_empty());
}

#[test]
fn an_instrument_nobody_applied_is_absent_rather_than_an_error() {
    let store = store();
    assert!(store.by_id(&unique("INS")).unwrap().is_none());
    assert_eq!(store.version_of(&unique("INS")).unwrap(), None);
}

#[test]
fn the_replica_can_say_how_much_it_holds() {
    let store = store();
    store
        .apply(instrument(
            &unique("INS"),
            vec![identifier("figi", &unique("BBG"), "", 100)],
            1,
        ))
        .unwrap();

    assert!(store.count().unwrap() >= 1);
}
