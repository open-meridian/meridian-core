//! The Postgres store, against Postgres.
//!
//! Not against a fake. A store implementation tested against a stand-in tests
//! the stand-in, and every property worth having here -- the version gate under
//! concurrency, one record per minted set, a dated window evaluated in SQL,
//! what a release before contract v10 held sourced once -- is a property of
//! the database rather than of the Rust around it.
//!
//! These do not run under `make test`, which has no database. `make test-store`
//! runs them through compose. They fail loudly when the database is missing
//! rather than skipping, because a suite that quietly runs nothing is the
//! failure this project keeps finding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use meridian_instrument::store::{
    Asked, Change, Conflict, Field, Identifier, IdentifierSet, Instrument, Offer, Replaced, Source,
    Stood, Store, Version, Written,
};
use meridian_instrument::PostgresStore;

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

fn asked(scheme: &str, value: &str, source: &str) -> Asked {
    Asked {
        scheme: scheme.into(),
        value: value.into(),
        source: source.into(),
    }
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

fn record(instrument_id: &str, identifiers: Vec<Identifier>) -> Instrument {
    Instrument {
        instrument_id: instrument_id.into(),
        sources: identifiers
            .iter()
            .map(|held| Source {
                acting_through_delegation: String::new(),
                client_name: String::new(),
                field: Field::Identifier,
                identifier: Some(held.asked()),
                source: "reported by custody-1".into(),
                person: String::new(),
                instance_id: "custody-1".into(),
                recorded_at_ns: 100,
                note: String::new(),
            })
            .collect(),
        identifiers,
        asset_class: String::new(),
        currency: String::new(),
        exchange_mic: String::new(),
        description: String::new(),
        instrument_type: String::new(),
        money_market_fund: String::new(),
        listing_venue_id: String::new(),
        lifecycle_state: "INSTRUMENT_LIFECYCLE_STATE_ACTIVE".into(),
        version: 1,
        valid_from_ns: 0,
        record_time_ns: 100,
        offers: vec![Offer {
            field: Field::AssetClass,
            value: "ASSET_CLASS_EQUITY".into(),
            identifier: None,
            source: "stated by custody-1".into(),
            instance_id: "custody-1".into(),
            offered_at_ns: 100,
        }],
    }
}

fn entry(instrument_id: &str, version: i64, operation: &str) -> Version {
    Version {
        acting_through_delegation: String::new(),
        client_name: String::new(),
        instrument_id: instrument_id.into(),
        version,
        operation: operation.into(),
        changes: vec![Change {
            field: "identifier".into(),
            scheme: "symbol".into(),
            namespace: "snaptrade".into(),
            before: String::new(),
            after: "SNAP1".into(),
            source: "reported by custody-1".into(),
        }],
        person: String::new(),
        instance_id: "custody-1".into(),
        note: String::new(),
        merged_instrument_id: String::new(),
        record_time_ns: 100 + version,
    }
}

fn minted(store: &PostgresStore, tag: &str) -> Instrument {
    let id = unique("LCL");
    let symbol = unique(tag);
    let key = IdentifierSet::new([asked("symbol", &symbol, "snaptrade")]).key();
    let (held, stood) = store
        .mint(
            record(&id, vec![identifier("symbol", &symbol, "snaptrade", 0)]),
            &key,
            entry(&id, 1, "mint"),
        )
        .unwrap();
    assert_eq!(stood, Stood::Minted);
    held
}

#[test]
fn a_minted_record_comes_back_whole_with_its_sources_offers_and_history() {
    let store = store();
    let held = minted(&store, "SNAP");
    let back = store.by_id(&held.instrument_id).unwrap().unwrap();
    assert_eq!(back, held);
    let history = store.history(&held.instrument_id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].operation, "mint");
    assert_eq!(history[0].changes[0].after, "SNAP1");
}

#[test]
fn concurrent_resolves_of_one_set_meet_one_record() {
    let store = Arc::new(store());
    let symbol = unique("RACE");
    let key = IdentifierSet::new([asked("symbol", &symbol, "snaptrade")]).key();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let (store, symbol, key) = (store.clone(), symbol.clone(), key.clone());
            std::thread::spawn(move || {
                let id = unique("LCL");
                store
                    .mint(
                        record(&id, vec![identifier("symbol", &symbol, "snaptrade", 0)]),
                        &key,
                        entry(&id, 1, "mint"),
                    )
                    .unwrap()
                    .0
                    .instrument_id
            })
        })
        .collect();
    let ids: std::collections::HashSet<String> =
        threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(ids.len(), 1, "one set, one record");
}

#[test]
fn a_write_against_a_version_since_moved_on_is_refused_and_writes_nothing() {
    let store = store();
    let held = minted(&store, "GATE");
    let mut next = held.clone();
    next.asset_class = "ASSET_CLASS_EQUITY".into();
    next.currency = "USD".into();
    next.version = 2;
    next.offers.clear();
    next.sources.push(Source {
        acting_through_delegation: String::new(),
        client_name: String::new(),
        field: Field::AssetClass,
        identifier: None,
        source: "a statement".into(),
        person: "local|ada".into(),
        instance_id: String::new(),
        recorded_at_ns: 200,
        note: String::new(),
    });
    assert_eq!(
        store
            .write(next.clone(), 1, entry(&held.instrument_id, 2, "complete"))
            .unwrap(),
        Written::Stored
    );
    // The store answers sources by field; what they say is what was written.
    let mut back = store.by_id(&held.instrument_id).unwrap().unwrap();
    let order = |source: &Source| (source.field.name(), source.identifier.clone());
    back.sources.sort_by_key(order);
    let mut written = next.clone();
    written.sources.sort_by_key(order);
    assert_eq!(back, written);
    assert_eq!(
        store
            .write(next.clone(), 1, entry(&held.instrument_id, 2, "complete"))
            .unwrap(),
        Written::Stale { held: 2 }
    );
    let mut missing = next;
    missing.instrument_id = unique("LCL-NONE");
    assert_eq!(
        store
            .write(
                missing.clone(),
                1,
                entry(&missing.instrument_id, 2, "complete")
            )
            .unwrap(),
        Written::Missing
    );
    assert_eq!(
        store
            .history(&held.instrument_id)
            .unwrap()
            .iter()
            .map(|v| v.version)
            .collect::<Vec<_>>(),
        vec![2, 1]
    );
}

#[test]
fn an_identifier_matches_only_while_its_window_covers_the_moment_and_in_its_namespace() {
    let store = store();
    let id = unique("INS");
    let figi = unique("BBG");
    let key = unique("held");
    store
        .mint(
            record(&id, vec![identifier("figi", &figi, "", 1_000)]),
            &key,
            entry(&id, 1, "migrate"),
        )
        .unwrap();
    assert!(store.matching("figi", &figi, "", 999).unwrap().is_empty());
    assert_eq!(store.matching("figi", &figi, "", 1_000).unwrap().len(), 1);
    assert!(
        store
            .matching("figi", &figi, "snaptrade", 1_000)
            .unwrap()
            .is_empty(),
        "a scoped identifier does not answer for a global one"
    );
}

#[test]
fn a_conflict_is_listed_once_and_brought_up_to_date() {
    let store = store();
    let figi = unique("BBG");
    let conflict = Conflict {
        identifiers: vec![asked("figi", &figi, "")],
        instrument_ids: vec!["LCL-A".into(), "LCL-B".into()],
        reported_by: String::new(),
        first_seen_ns: 10,
        last_seen_ns: 10,
    };
    store.note_conflict(conflict.clone()).unwrap();
    store
        .note_conflict(Conflict {
            reported_by: "custody-1".into(),
            first_seen_ns: 20,
            last_seen_ns: 20,
            ..conflict
        })
        .unwrap();
    let mine: Vec<Conflict> = store
        .conflicts()
        .unwrap()
        .into_iter()
        .filter(|c| c.identifiers[0].value == figi)
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].first_seen_ns, 10);
    assert_eq!(mine[0].last_seen_ns, 20);
    assert_eq!(mine[0].reported_by, "custody-1");
}

#[test]
fn a_replacement_is_kept_and_the_first_pairing_stands() {
    let store = store();
    let merged = unique("LCL");
    assert_eq!(
        store.replace(&merged, "LCL-KEPT", 10).unwrap(),
        Replaced::Recorded
    );
    assert_eq!(
        store.replace(&merged, "LCL-OTHER", 20).unwrap(),
        Replaced::AlreadyRecorded {
            replaced_by: "LCL-KEPT".into()
        }
    );
    assert_eq!(
        store.replacement_of(&merged).unwrap(),
        Some("LCL-KEPT".into())
    );
}

#[test]
fn what_a_release_before_v10_held_is_sourced_once() {
    // Q8: a placeholder not replaced becomes a record under its own ID, with
    // its identifiers and no values, the class it kept an offer; one replaced
    // stays replaced; a record applied from the platform keeps its values,
    // each sourced "the platform, record version N" with no person.
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let scratch = format!("before_v10_{}", unique("t").replace('-', "_"));
    admin
        .batch_execute(&format!("CREATE SCHEMA {scratch}"))
        .unwrap();
    let scoped = format!("{url}?options=-csearch_path%3D{scratch}");
    // What a release before v10 made, and held.
    admin
        .batch_execute(&format!(
            "SET search_path TO {scratch};
             {first}
             {second}
             INSERT INTO instrument (instrument_id, asset_class, currency, description, version,
                                     valid_from_ns, record_time_ns)
                  VALUES ('INS-USD', 'ASSET_CLASS_CASH', 'USD', 'US dollar', 3, 0, 50);
             INSERT INTO instrument_identifier (instrument_id, scheme, value, source, valid_from_ns)
                  VALUES ('INS-USD', 'iso4217', 'USD', '', 0);
             INSERT INTO instrument_placeholder (placeholder_id, identifier_key, source, asset_class,
                                                 as_of_ns, minted_at_ns)
                  VALUES ('LCL-OPEN', 'k-open', 'snaptrade', 'ASSET_CLASS_FUND', 1, 60),
                         ('LCL-GONE', 'k-gone', 'snaptrade', '', 1, 70);
             INSERT INTO instrument_placeholder_identifier (placeholder_id, scheme, value, source)
                  VALUES ('LCL-OPEN', 'symbol', 'SPAXX', 'snaptrade'),
                         ('LCL-GONE', 'symbol', 'GONE', 'snaptrade');
             INSERT INTO instrument_replacement (replaced_id, replaced_by, replaced_at_ns)
                  VALUES ('LCL-GONE', 'INS-USD', 80);",
            first = include_str!("../migrations/0001_instrument.sql"),
            second = include_str!("../migrations/0002_placeholder.sql"),
        ))
        .unwrap();

    let store = PostgresStore::connect(&scoped, 1).unwrap();
    store.migrate().unwrap();
    store.migrate().expect("a second run finds nothing to do");

    let usd = store.by_id("INS-USD").unwrap().unwrap();
    assert_eq!(usd.version, 3, "its key and version stay");
    let class = usd.source_of(Field::AssetClass, None).unwrap();
    assert_eq!(class.source, "the platform, record version 3");
    assert!(class.person.is_empty());
    assert_eq!(store.history("INS-USD").unwrap()[0].operation, "migrate");

    let open = store
        .by_id("LCL-OPEN")
        .unwrap()
        .expect("a placeholder became a record");
    assert!(open.asset_class.is_empty(), "no value in force");
    assert_eq!(open.identifiers[0].value, "SPAXX");
    assert_eq!(open.offers.len(), 1, "the class it kept is offered");
    assert_eq!(open.offers[0].value, "ASSET_CLASS_FUND");
    assert_eq!(store.history("LCL-OPEN").unwrap().len(), 1);

    assert!(
        store.by_id("LCL-GONE").unwrap().is_none(),
        "a replaced one stays replaced"
    );
    assert_eq!(
        store.replacement_of("LCL-GONE").unwrap(),
        Some("INS-USD".into())
    );
    store.verify().unwrap();

    admin
        .batch_execute(&format!("DROP SCHEMA {scratch} CASCADE"))
        .unwrap();
}

#[test]
fn a_start_against_a_database_with_no_schema_refuses_and_names_the_fix() {
    // Found installing the chart for the first time: the serving credential
    // has no right to create a table, by design, so a start that tried met
    // `permission denied for schema public` and said nothing about migrating.
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let scratch = format!("verify_{}", unique("t").replace('-', "_"));
    admin
        .batch_execute(&format!("CREATE SCHEMA {scratch}"))
        .unwrap();

    let scoped = format!("{url}?options=-csearch_path%3D{scratch}");
    let store = PostgresStore::connect(&scoped, 1).unwrap();

    let refused = store
        .verify()
        .expect_err("an empty database must not verify");
    let said = refused.to_string();
    assert!(said.contains("no schema"), "{said}");
    assert!(
        said.contains("migrate"),
        "the refusal has to name the fix: {said}"
    );

    store.migrate().expect("could not apply the schema");
    store.verify().expect("a migrated database must verify");

    admin
        .batch_execute(&format!("DROP SCHEMA {scratch} CASCADE"))
        .unwrap();
}

#[test]
fn a_free_text_class_becomes_the_enums_and_one_it_did_not_plainly_mean_is_cleared() {
    // sdk-contract/asset-class-is-an-enum. The column held whatever the
    // platform sent; the platform's own migration maps its master the same
    // way, so what is held here agrees with the authority afterwards.
    let url = std::env::var("MERIDIAN_TEST_DATABASE_URL").unwrap();
    let mut admin = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let scratch = format!("classes_{}", unique("t").replace('-', "_"));
    admin
        .batch_execute(&format!("CREATE SCHEMA {scratch}"))
        .unwrap();
    let scoped = format!("{url}?options=-csearch_path%3D{scratch}");
    let store = PostgresStore::connect(&scoped, 1).unwrap();
    store.migrate().unwrap();

    for (id, class) in [
        ("INS-A", "EQUITY"),
        ("INS-B", "ETF"),
        ("INS-C", "REIT"),
        ("INS-D", ""),
        ("INS-E", "ASSET_CLASS_DEBT"),
    ] {
        admin
            .execute(
                &format!(
                    "INSERT INTO {scratch}.instrument (instrument_id, asset_class, version, \
                     valid_from_ns, record_time_ns) VALUES ($1, $2, 1, 0, 0)"
                ),
                &[&id, &class],
            )
            .unwrap();
    }

    store.migrate().unwrap();
    store.migrate().expect("a second run finds nothing to do");

    let class = |id: &str| store.by_id(id).unwrap().unwrap().asset_class;
    assert_eq!(class("INS-A"), "ASSET_CLASS_EQUITY");
    assert_eq!(class("INS-B"), "ASSET_CLASS_FUND", "an ETF is a fund");
    assert_eq!(class("INS-C"), "", "not guessed: cleared, and reported");
    assert_eq!(class("INS-D"), "");
    assert_eq!(class("INS-E"), "ASSET_CLASS_DEBT");

    admin
        .batch_execute(&format!("DROP SCHEMA {scratch} CASCADE"))
        .unwrap();
}

#[test]
fn a_venue_is_kept_at_its_newest_version_and_a_record_keeps_its_listing_venue() {
    use meridian_domain::v1::VenueRecord;
    let store = store();
    let id = format!("VEN-{}", unique("xnys"));
    let venue = |version: i64, name: &str| VenueRecord {
        venue_id: id.clone(),
        name: name.into(),
        version,
        ..Default::default()
    };
    assert!(store.keep_venue(&venue(2, "NYSE"), 1).unwrap());
    assert!(!store.keep_venue(&venue(1, "Old"), 2).unwrap());
    assert!(store
        .keep_venue(&venue(3, "New York Stock Exchange"), 3)
        .unwrap());
    let held: Vec<_> = store
        .venues()
        .unwrap()
        .into_iter()
        .filter(|v| v.venue_id == id)
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].name, "New York Stock Exchange");

    let mut listed = minted(&store, "listed");
    let before = listed.version;
    listed.listing_venue_id = id.clone();
    listed.version += 1;
    store
        .write(
            listed.clone(),
            before,
            entry(&listed.instrument_id, listed.version, "platform"),
        )
        .unwrap();
    let back = store.by_id(&listed.instrument_id).unwrap().unwrap();
    assert_eq!(back.listing_venue_id, id);
}
