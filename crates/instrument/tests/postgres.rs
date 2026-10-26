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
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not create the schema");
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
        trading_currency: String::new(),
    }
}

fn record(instrument_id: &str, identifiers: Vec<Identifier>) -> Instrument {
    Instrument {
        instrument_id: instrument_id.into(),
        sources: identifiers
            .iter()
            .map(|held| Source {
                prepared_call_id: Default::default(),
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
        binary_event_contract: String::new(),
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
            valid_until_ns: 0,
            provenance: Vec::new(),
        }],
        ..Default::default()
    }
}

fn entry(instrument_id: &str, version: i64, operation: &str) -> Version {
    Version {
        prepared_call_id: Default::default(),
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
fn a_listing_identifiers_trading_currency_comes_back_with_it() {
    // Contract v25 (W3.1).
    let store = store();
    let id = unique("INS");
    let figi = unique("BBG");
    let mut listing = identifier("figi", &figi, "", 0);
    listing.trading_currency = "CAD".into();
    let key = IdentifierSet::new([asked("figi", &figi, "")]).key();
    let (held, _) = store
        .mint(record(&id, vec![listing]), &key, entry(&id, 1, "mint"))
        .unwrap();
    let back = store.by_id(&held.instrument_id).unwrap().unwrap();
    assert_eq!(back.identifiers[0].trading_currency, "CAD");
    let matched = store.matching("figi", &figi, "", 1).unwrap();
    assert_eq!(matched[0].identifiers[0].trading_currency, "CAD");
}

#[test]
fn a_currency_offer_keeps_the_provenance_it_came_with() {
    // Contract v23 (W3.1): a currency the plugin's admin supplied is offered
    // with its Provenance entry, kept encoded as it arrived and read back
    // whole; an offer with none reads back with none.
    let store = store();
    let mut held = minted(&store, "CCY");
    let entry_bytes = prost::Message::encode_to_vec(&meridian_pb::v1::Provenance {
        field: "stated_currency".into(),
        kind: meridian_pb::v1::ProvenanceKind::Supplied as i32,
        person: "local|admin".into(),
        ..Default::default()
    });
    held.offers.push(Offer {
        field: Field::Currency,
        value: "USD".into(),
        identifier: None,
        source: "supplied by local|admin on custody-1's page".into(),
        instance_id: "custody-1".into(),
        offered_at_ns: 200,
        valid_until_ns: 0,
        provenance: entry_bytes.clone(),
    });
    held.version = 2;
    let id = held.instrument_id.clone();
    assert_eq!(
        store.write(held, 1, entry(&id, 2, "resolve")).unwrap(),
        Written::Stored
    );
    let back = store.by_id(&id).unwrap().unwrap();
    let currency = back
        .offers
        .iter()
        .find(|offer| offer.field == Field::Currency)
        .expect("the currency's offer is kept");
    assert_eq!(currency.provenance, entry_bytes);
    assert_eq!(
        currency.source,
        "supplied by local|admin on custody-1's page"
    );
    let class = back
        .offers
        .iter()
        .find(|offer| offer.field == Field::AssetClass)
        .unwrap();
    assert!(class.provenance.is_empty(), "a stated value carries none");
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
        prepared_call_id: Default::default(),
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
        reason: 0,
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
             INSERT INTO instrument (instrument_id, asset_class, currency, description, version,
                                     valid_from_ns, record_time_ns)
                  VALUES ('INS-TCH4', 'ASSET_CLASS_FUND', 'USD', 'A plan option', 1, 0, 50);
             INSERT INTO instrument_identifier (instrument_id, scheme, value, source, valid_from_ns)
                  VALUES ('INS-TCH4', 'symbol', 'TCH4', 'snaptrade', 1790640000000000000);
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
    store.migrate(&meridian_clock::SystemClock).unwrap();
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("a second run finds nothing to do");

    let usd = store.by_id("INS-USD").unwrap().unwrap();
    assert_eq!(usd.version, 3, "its key and version stay");
    let class = usd.source_of(Field::AssetClass, None).unwrap();
    assert_eq!(class.source, "the platform, record version 3");
    assert!(class.person.is_empty());
    assert_eq!(store.history("INS-USD").unwrap()[0].operation, "migrate");

    // A window kept from the date the platform learnt the identifier opens
    // from the start, once, as a version of its own naming the date.
    let tch4 = store.by_id("INS-TCH4").unwrap().unwrap();
    assert_eq!(tch4.identifiers[0].valid_from_ns, 0);
    assert_eq!(tch4.version, 2);
    let history = store.history("INS-TCH4").unwrap();
    assert_eq!(
        history.len(),
        2,
        "sourced, then opened; the second run wrote nothing"
    );
    assert!(history.iter().any(|v| v
        .changes
        .iter()
        .any(|c| c.source.contains("recorded from 2026-09-29 00:00 UTC"))));
    let earlier = store
        .matching("symbol", "TCH4", "snaptrade", 1_790_000_000_000_000_000)
        .unwrap();
    assert_eq!(earlier.len(), 1, "an earlier date finds the record held");

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

    store
        .migrate(&meridian_clock::SystemClock)
        .expect("could not apply the schema");
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
    store.migrate(&meridian_clock::SystemClock).unwrap();

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

    store.migrate(&meridian_clock::SystemClock).unwrap();
    store
        .migrate(&meridian_clock::SystemClock)
        .expect("a second run finds nothing to do");

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

#[test]
fn a_closed_window_an_ended_offer_a_contract_and_a_conflicts_reason_are_kept() {
    // Contract v19: an identifier that left the record kept with its window
    // closed, matched only for dates it covered; a binary event contract's
    // attributes; the platform's end offered; a conflict's reason.
    let store = store();
    let id = unique("LCL-V19");
    let symbol = unique("ZZ");
    let set = IdentifierSet::new([asked("symbol", &symbol, "kalshi")]).key();
    let mut held = record(&id, vec![identifier("symbol", &symbol, "kalshi", 0)]);
    store
        .mint(held.clone(), &set, entry(&id, 1, "mint"))
        .unwrap();

    held.identifiers[0].valid_to_ns = Some(500);
    held.instrument_type = "INSTRUMENT_TYPE_BINARY_EVENT_CONTRACT".into();
    held.binary_event_contract = "1|USD|LCL-USD|1793548800000000000".into();
    held.offers.push(Offer {
        field: Field::Identifier,
        value: symbol.clone(),
        identifier: Some(asked("symbol", &symbol, "kalshi")),
        source: "the platform, record INS-1 version 2".into(),
        instance_id: String::new(),
        offered_at_ns: 400,
        valid_until_ns: 450,
        provenance: Vec::new(),
    });
    held.version = 2;
    assert_eq!(
        store.write(held, 1, entry(&id, 2, "complete")).unwrap(),
        Written::Stored
    );
    let read = store.by_id(&id).unwrap().unwrap();
    assert_eq!(
        read.identifiers[0].valid_to_ns,
        Some(500),
        "closed, never deleted"
    );
    assert_eq!(
        read.binary_event_contract,
        "1|USD|LCL-USD|1793548800000000000"
    );
    assert!(read.offers.iter().any(|offer| offer.valid_until_ns == 450));
    assert_eq!(
        store
            .matching("symbol", &symbol, "kalshi", 499)
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .matching("symbol", &symbol, "kalshi", 500)
        .unwrap()
        .is_empty());
    let windows = store.windows("symbol", &symbol, "kalshi").unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].1.valid_to_ns, Some(500));

    let figi = unique("BBG");
    store
        .note_conflict(Conflict {
            identifiers: vec![asked("figi", &figi, "")],
            instrument_ids: vec!["LCL-A".into(), "LCL-B".into()],
            reported_by: "custody-1".into(),
            first_seen_ns: 10,
            last_seen_ns: 10,
            reason: 2,
        })
        .unwrap();
    // Seen again with no reason: once ambiguous, it stays so.
    store
        .note_conflict(Conflict {
            identifiers: vec![asked("figi", &figi, "")],
            instrument_ids: vec!["LCL-A".into(), "LCL-B".into()],
            reported_by: String::new(),
            first_seen_ns: 20,
            last_seen_ns: 20,
            reason: 0,
        })
        .unwrap();
    let listed: Vec<Conflict> = store
        .conflicts()
        .unwrap()
        .into_iter()
        .filter(|c| c.identifiers[0].value == figi)
        .collect();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].reason, 2);
    assert_eq!(listed[0].last_seen_ns, 20);
}

#[test]
fn every_calendar_and_rule_version_is_kept_once_with_when_it_was_kept() {
    // Contract v22 (W3.17): every version kept, the earlier ones readable.
    use meridian_domain::v1::{
        date_rule_version, CalendarRecord, DateRuleVersion, SettlementDateRule,
    };
    let store = store();
    let id = format!("CAL-{}", unique("USSS"));
    let calendar = |version: i64| CalendarRecord {
        calendar_id: id.clone(),
        name: "US securities settlement".into(),
        version,
        ..Default::default()
    };
    assert!(store.keep_calendar(&calendar(1), 10).unwrap());
    assert!(
        !store.keep_calendar(&calendar(1), 11).unwrap(),
        "a version held changes nothing"
    );
    assert!(store.keep_calendar(&calendar(2), 12).unwrap());
    let kept: Vec<(i64, i64)> = store
        .calendar_versions()
        .unwrap()
        .into_iter()
        .filter(|kept| kept.record.calendar_id == id)
        .map(|kept| (kept.record.version, kept.kept_at_ns))
        .collect();
    assert_eq!(kept, vec![(1, 10), (2, 12)]);

    let subject = unique("US/equity");
    let rule = |version: i64, lag: i32| DateRuleVersion {
        kind: meridian_pb::v1::DateRuleKind::Settlement as i32,
        subject: subject.clone(),
        version,
        parameters: Some(date_rule_version::Parameters::Settlement(
            SettlementDateRule {
                country_code: "US".into(),
                lag_days: lag,
                ..Default::default()
            },
        )),
        ..Default::default()
    };
    assert!(store.keep_date_rule(&rule(1, 2), 20).unwrap());
    assert!(store.keep_date_rule(&rule(2, 1), 21).unwrap());
    assert!(!store.keep_date_rule(&rule(2, 1), 22).unwrap());
    let rules: Vec<i64> = store
        .date_rule_versions()
        .unwrap()
        .into_iter()
        .filter(|kept| kept.rule.subject == subject)
        .map(|kept| kept.rule.version)
        .collect();
    assert_eq!(rules, vec![1, 2]);
}

/// Contract v24 (W3.7, W3.8, W3.22, W3.24, W3.25, W3.26): a record's
/// provisional state comes back whole; each replacement says why and takes
/// the next number in one sequence; a template version and a licence list
/// are each kept once; the runs' own state is kept by key.
#[test]
fn the_one_id_state_is_kept_whole_and_each_replacement_numbered() {
    use meridian_instrument::store::Local;
    let store = store();
    let mut held = minted(&store, "PROV");
    held.local = Local {
        provisional: true,
        reason: meridian_domain::v1::ProvisionalReason::AwaitingStaff as i32,
        template_id: "TPL-equity".into(),
        template_version: "1".into(),
        missing_fields: vec!["currency".into()],
        asked_at_ns: 77,
        migration: "awaiting_staff".into(),
        ..Default::default()
    };
    held.version = 2;
    let id = held.instrument_id.clone();
    assert_eq!(
        store
            .write(held.clone(), 1, entry(&id, 2, "provisional"))
            .unwrap(),
        Written::Stored
    );
    assert_eq!(store.by_id(&id).unwrap().unwrap().local, held.local);

    let first = unique("LCL");
    let second = unique("LCL");
    store
        .replace_because(
            &first,
            "INS-ONE",
            meridian_domain::v1::ReplacementCause::Resolved as i32,
            10,
        )
        .unwrap();
    store.replace(&second, "INS-ONE", 20).unwrap();
    let a = store.replacement_record(&first).unwrap().unwrap();
    let b = store.replacement_record(&second).unwrap().unwrap();
    assert_eq!(
        a.cause,
        meridian_domain::v1::ReplacementCause::Resolved as i32
    );
    assert_eq!(
        b.cause,
        meridian_domain::v1::ReplacementCause::Merged as i32
    );
    assert_eq!(
        b.sequence,
        a.sequence + 1,
        "one sequence, numbered in order"
    );
    let after = store.replacements_after(a.sequence - 1, 1000).unwrap();
    assert!(after.iter().any(|kept| kept.replaced_id == first));
    assert!(after
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));

    let template = meridian_domain::v1::SecurityTemplate {
        template_id: unique("TPL-test"),
        template_version: "2".into(),
        name: "a test template".into(),
        asset_class: meridian_domain::v1::AssetClass::Equity as i32,
        ..Default::default()
    };
    assert!(store.keep_template(&template, 5).unwrap());
    assert!(
        !store.keep_template(&template, 6).unwrap(),
        "a version is kept once"
    );
    assert!(store.templates().unwrap().contains(&template));
    // A held copy that differs was decoded by an older build (found on dev
    // 2026-10-26: a pod of the release before dropped a package's model
    // bindings); the whole copy replaces it, and is then kept once.
    let whole = meridian_domain::v1::SecurityTemplate {
        calculation_package: Some(meridian_domain::v1::CalculationPackage {
            model_bindings: vec![meridian_domain::v1::ModelBinding {
                model_id: "black_scholes".into(),
                model_version: 1,
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..template.clone()
    };
    assert!(
        store.keep_template(&whole, 7).unwrap(),
        "a differing copy replaces the held one"
    );
    assert!(!store.keep_template(&whole, 8).unwrap());
    let held = store.templates().unwrap();
    assert!(held.contains(&whole));
    assert!(!held.contains(&template));

    // A version no earlier run kept: the database outlives a run (test-store
    // runs again against the same one), and a counter alone starts again at
    // zero, so a second run's version was the first's and was not kept.
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    let list = meridian_domain::v1::LicenceList {
        version: now_ns + (COUNTER.fetch_add(1, Ordering::SeqCst) as i64),
        schemes: vec!["figi".into(), "cusip".into()],
        pace: Some(meridian_domain::v1::LicencePace {
            batch_size: 100,
            batches_per_minute: 6,
        }),
        administrators: vec!["nyfed".into()],
    };
    assert!(store.keep_licence_list(&list, 7).unwrap());
    assert!(!store.keep_licence_list(&list, 8).unwrap());
    let kept = store.licence_list().unwrap().unwrap();
    assert!(kept.version >= list.version);
    if kept.version == list.version {
        // The administrators kept with the list (ruling 545).
        assert_eq!(kept.administrators, ["nyfed"]);
    }

    let key = unique("state");
    assert_eq!(store.state(&key).unwrap(), None);
    store.set_state(&key, "1043").unwrap();
    store.set_state(&key, "1044").unwrap();
    assert_eq!(store.state(&key).unwrap().as_deref(), Some("1044"));
}

#[test]
fn every_company_version_is_kept_once_and_an_identifier_it_carried_finds_it() {
    // Contract v28 (W3.32; ruling 612): the platform's ENT- record, each
    // version its own row; an identifier a former version carried still
    // finds the company, its window saying whether it holds it.
    use meridian_domain::v1::{CompanyRecord, Identifier as PbIdentifier};
    let store = store();
    let company_id = format!("ENT-{}", unique("company"));
    let cik = unique("cik");
    let former = unique("cik");
    let pb = |value: &str, until: i64| PbIdentifier {
        scheme: "cik".into(),
        value: value.into(),
        valid_until_ns: until,
        ..Default::default()
    };
    let first = CompanyRecord {
        company_id: company_id.clone(),
        name: "First Name".into(),
        identifiers: vec![pb(&former, 0)],
        former_names: Vec::new(),
        version: 1,
    };
    let second = CompanyRecord {
        name: "Second Name".into(),
        identifiers: vec![pb(&former, 5), pb(&cik, 0)],
        version: 2,
        ..first.clone()
    };
    assert!(store.keep_company(&first, 1).unwrap());
    assert!(!store.keep_company(&first, 2).unwrap());
    assert!(store.keep_company(&second, 3).unwrap());
    assert_eq!(store.company(&company_id).unwrap(), Some(second.clone()));
    assert_eq!(
        store.companies_with("cik", &cik).unwrap(),
        vec![second.clone()]
    );
    assert_eq!(store.companies_with("cik", &former).unwrap(), vec![second]);
    assert!(store
        .companies_with("cik", &unique("cik"))
        .unwrap()
        .is_empty());
    assert_eq!(
        store.company(&format!("ENT-{}", unique("none"))).unwrap(),
        None
    );
}
