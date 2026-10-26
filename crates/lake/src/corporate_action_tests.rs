//! Declared corporate actions in the lake (contract v28, the filings slice;
//! W10.4 to W10.6; ruling 598): a cash dividend recorded with the security
//! as its subject and its issuer beside it as a company, published every
//! version unconflated, read by subject and role, kind and ex-date, point in
//! time, and caught up after a watermark; and the refusals W10.4 names. The
//! instrument store stood in on the bus, as the lake's other tests stand it
//! in.

use std::sync::Arc;

use meridian_bus::{Bus, Envelope, MemoryBackend, MessageMeta};
use meridian_clock::ManualClock;
use meridian_domain::v1::{
    AssetClass, CorporateActionAnnounced, CorporateActionKind, CorporateActionsRecordedEvent,
    DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, FilingRef, InstrumentRecord,
    ListCorporateActionsRequest, MeasureRef, Money, ObservationMeta, PartitionSequence,
    ResolveCompanyRequest, ResolveIdentifierReply, ResolveIdentifierRequest,
    ResolveInstrumentReply, ResolveInstrumentRequest, Source, SubjectRef, SubjectRole, Watermark,
};
use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, Decimal, ObservationMode};
use prost::Message;

use crate::row::Observation;
use crate::service::{self, Lake};

const NOW: i64 = 1_791_417_600_000_000_000; // 2026-10-08T00:00Z
const HOUR: i64 = 3_600 * 1_000_000_000;
const AAPL: &str = "INS-01JC24AAPL00000000000001";
const APPLE: &str = "ENT-01JC28APPLEINC000000000001";
const DIVIDENDS: &str = "sec-edgar-1:dividends";

struct Harness {
    bus: Arc<Bus>,
    lake: Arc<Lake>,
}

fn harness() -> Harness {
    let clock = Arc::new(ManualClock::at(NOW));
    let bus = Arc::new(Bus::single(
        "lake-1",
        Arc::new(MemoryBackend::new()),
        clock.clone(),
    ));
    bus.serve(service::RESOLVE_INSTRUMENT, |envelope| {
        let asked = ResolveInstrumentRequest::decode(&envelope.payload[..]).unwrap();
        let instrument = (asked.instrument_id == AAPL).then(|| InstrumentRecord {
            instrument_id: AAPL.into(),
            asset_class: AssetClass::Equity as i32,
            ..Default::default()
        });
        Ok((
            "meridian.v1.ResolveInstrumentReply".into(),
            ResolveInstrumentReply {
                found: instrument.is_some(),
                instrument,
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(service::RESOLVE_IDENTIFIER, |envelope| {
        let asked = ResolveIdentifierRequest::decode(&envelope.payload[..]).unwrap();
        let usd = asked
            .identifiers
            .first()
            .is_some_and(|first| first.scheme == "iso4217");
        Ok((
            "meridian.v1.ResolveIdentifierReply".into(),
            ResolveIdentifierReply {
                found: usd,
                instrument_id: if usd { "INS-USD".into() } else { String::new() },
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    // The store holds Apple's company record and no other; a company it
    // does not hold is refused, as W3.32's resolve by company_id is.
    bus.serve(service::RESOLVE_COMPANY, |envelope| {
        let asked = ResolveCompanyRequest::decode(&envelope.payload[..]).unwrap();
        if asked.company_id != APPLE {
            return Err(format!(
                "company_id: {:?} is no company this deployment holds",
                asked.company_id
            ));
        }
        Ok((
            "meridian.v1.CompanyRecord".into(),
            meridian_domain::v1::CompanyRecord {
                company_id: APPLE.into(),
                name: "Apple Inc.".into(),
                version: 1,
                ..Default::default()
            }
            .encode_to_vec(),
        ))
    });
    bus.serve(service::PLUGIN_CATALOGUE, |_| {
        Ok((
            "meridian.v1.PluginCatalogue".into(),
            meridian_domain::v1::PluginCatalogue::default().encode_to_vec(),
        ))
    });
    let store = Arc::new(crate::MemoryStore::new());
    let lake = service::serve(bus.clone(), store, clock);
    lake.configured(&configuration());
    Harness { bus, lake }
}

/// One dataset of declared dividends, signal-1 entitled to every field.
fn configuration() -> EntitlementsChangedEvent {
    let (instance, key) = DIVIDENDS.split_once(':').unwrap();
    EntitlementsChangedEvent {
        datasets: vec![DatasetRef {
            dataset: DIVIDENDS.into(),
            instance: instance.into(),
            vendor: "SEC EDGAR".into(),
            declaration: Some(DatasetDeclaration {
                key: key.into(),
                vendor: "SEC EDGAR".into(),
                data_types: vec!["meridian.v1.CorporateActionAnnounced".into()],
                modes: vec![ObservationMode::Push as i32],
                licence_default: Some(DatasetLicence {
                    kept: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }],
        licences: vec![],
        entitlements: vec![DatasetEntitlement {
            dataset: DIVIDENDS.into(),
            instance: "signal-1".into(),
            allowed: true,
            ..Default::default()
        }],
        changed_at_ns: NOW,
    }
}

fn envelope(instance: &str) -> Envelope {
    Envelope {
        meta: Some(MessageMeta {
            publisher_instance_id: instance.into(),
            account_scope_applies: true,
            published_at_ns: NOW,
            ..Default::default()
        }),
        payload_type: String::new(),
        payload: Vec::new(),
    }
}

fn decimal(text: &str) -> Option<Decimal> {
    Some(
        text.parse::<meridian_domain::exact::Exact>()
            .unwrap()
            .to_wire(),
    )
}

fn usd(amount: &str) -> Option<Money> {
    Some(Money {
        amount: decimal(amount),
        currency_code: "USD".into(),
        instrument_id: String::new(),
    })
}

fn subject(entity: &str, role: SubjectRole) -> SubjectRef {
    SubjectRef {
        entity_id: entity.into(),
        role: role as i32,
    }
}

/// A cash dividend as the fixture records it: the rule found every date in
/// an 8-K's exhibit.
fn dividend(row_key: &str, amount: &str, ex_date: &str) -> CorporateActionAnnounced {
    CorporateActionAnnounced {
        meta: Some(ObservationMeta {
            row_key: row_key.into(),
            subjects: vec![
                subject(AAPL, SubjectRole::Subject),
                subject(APPLE, SubjectRole::Issuer),
            ],
            source: Some(Source {
                instance: "sec-edgar-1".into(),
                dataset: DIVIDENDS.into(),
                ..Default::default()
            }),
            valid_from_ns: NOW - HOUR,
            ..Default::default()
        }),
        kind: CorporateActionKind::CashDividend as i32,
        amount_per_share: usd(amount),
        declared_date: "2026-10-01".into(),
        ex_date: ex_date.into(),
        record_date: ex_date.into(),
        payment_date: "2026-11-13".into(),
        rule: Some(MeasureRef {
            definition: "edgar_8k_dividend".into(),
            version: 1,
            ..Default::default()
        }),
        filing: Some(FilingRef {
            accession_number: "0000320193-26-000105".into(),
            form: "8-K".into(),
        }),
    }
}

fn record(h: &Harness, rows: Vec<CorporateActionAnnounced>) -> Result<(u32, u32, u32), String> {
    tokio::task::block_in_place(|| {
        h.lake.record(
            "actions",
            rows.into_iter().map(Observation::CorporateAction).collect(),
            "",
            &envelope("sec-edgar-1"),
        )
    })
    .map(|reply| (reply.recorded, reply.restated, reply.unchanged))
}

fn list(
    h: &Harness,
    request: ListCorporateActionsRequest,
) -> Result<meridian_domain::v1::ListCorporateActionsReply, String> {
    tokio::task::block_in_place(|| {
        h.lake
            .list_corporate_actions(request, &envelope("signal-1"))
    })
}

fn amounts(reply: &meridian_domain::v1::ListCorporateActionsReply) -> Vec<String> {
    reply
        .actions
        .iter()
        .map(|action| {
            meridian_domain::exact::Exact::from_wire(
                action
                    .amount_per_share
                    .as_ref()
                    .unwrap()
                    .amount
                    .as_ref()
                    .unwrap(),
            )
            .unwrap()
            .to_string()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dividend_is_recorded_heard_every_version_and_read_by_subject_role_and_ex_date() {
    let h = harness();
    let mut heard = h.bus.subscribe_every(&format!(
        "platform.lake.{DIVIDENDS}.event.corporate-actions-recorded"
    ));
    let rows = vec![
        dividend("0000320193-26-000105:dividend", "0.26", "2026-11-10"),
        dividend("0000320193-26-000071:dividend", "0.25", "2026-08-11"),
    ];
    assert_eq!(record(&h, rows).unwrap(), (2, 0, 0));
    let first =
        CorporateActionsRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
            .unwrap()
            .action
            .unwrap();
    assert_eq!(first.kind, CorporateActionKind::CashDividend as i32);
    assert_eq!(
        first.amount_per_share.as_ref().unwrap().instrument_id,
        "INS-USD",
        "a Money named by its code resolves to its cash instrument"
    );
    heard.recv().await.unwrap();

    // The dividends going ex from the as-of: the one ahead alone.
    let ahead = list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(AAPL, SubjectRole::Unspecified)],
            from_date: "2026-10-08".into(),
            kinds: vec![CorporateActionKind::CashDividend as i32],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(amounts(&ahead), vec!["0.26"]);
    assert_eq!(ahead.datasets[0].dataset, DIVIDENDS);
    // By the issuer, in its role; never in another.
    let by_issuer = list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(APPLE, SubjectRole::Issuer)],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_issuer.actions.len(), 2);
    let as_holder = list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(APPLE, SubjectRole::Holder)],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(as_holder.actions.is_empty());
    let watermark = ahead.watermark.clone().unwrap();

    // A correction is the next version of its row key: the read answers it,
    // and both versions are heard, never conflated.
    assert_eq!(
        record(
            &h,
            vec![dividend(
                "0000320193-26-000105:dividend",
                "0.27",
                "2026-11-10"
            )]
        )
        .unwrap(),
        (0, 1, 0)
    );
    let corrected =
        CorporateActionsRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
            .unwrap()
            .action
            .unwrap();
    assert_eq!(corrected.meta.as_ref().unwrap().version, 2);
    let now = list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(AAPL, SubjectRole::Unspecified)],
            from_date: "2026-10-08".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(amounts(&now), vec!["0.27"]);
    // Caught up after the watermark the reader last saw: the correction,
    // whatever its date; dates beside it are refused.
    let caught_up = list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(AAPL, SubjectRole::Unspecified)],
            after_watermark: Some(watermark.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(amounts(&caught_up), vec!["0.27"]);
    let from_the_start = list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(AAPL, SubjectRole::Unspecified)],
            after_watermark: Some(Watermark {
                partitions: vec![PartitionSequence {
                    partition: DIVIDENDS.into(),
                    sequence: 0,
                }],
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        from_the_start.actions.len(),
        3,
        "every version recorded after the watermark"
    );
    assert!(list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(AAPL, SubjectRole::Unspecified)],
            after_watermark: Some(watermark),
            from_date: "2026-10-08".into(),
            ..Default::default()
        },
    )
    .unwrap_err()
    .contains("after_watermark"));
}

#[tokio::test(flavor = "multi_thread")]
async fn what_w10_4_refuses_of_a_corporate_action_is_refused_naming_the_field() {
    let h = harness();
    let refused = |mut row: CorporateActionAnnounced,
                   change: &dyn Fn(&mut CorporateActionAnnounced)| {
        change(&mut row);
        record(&h, vec![row]).unwrap_err()
    };
    let base = || dividend("0000320193-26-000105:dividend", "0.26", "2026-11-10");
    // No ex-date: the plugin never infers one (FDP Q12).
    let words = refused(base(), &|row| row.ex_date.clear());
    assert!(words.starts_with("actions[0].ex_date"), "{words}");
    let words = refused(base(), &|row| row.ex_date = "2026-11-31".into());
    assert!(words.contains("actions[0].ex_date"), "{words}");
    let words = refused(base(), &|row| row.kind = 0);
    assert!(words.starts_with("actions[0].kind"), "{words}");
    let words = refused(base(), &|row| row.amount_per_share = None);
    assert!(words.starts_with("actions[0].amount_per_share"), "{words}");
    let words = refused(base(), &|row| row.rule = None);
    assert!(words.starts_with("actions[0].rule"), "{words}");
    // A company in a role other than issuer; a company the deployment does
    // not hold; a company alone, with no security.
    let words = refused(base(), &|row| {
        row.meta.as_mut().unwrap().subjects[1].role = SubjectRole::Holder as i32;
    });
    assert!(
        words.contains("actions[0].meta.subjects[1].role"),
        "{words}"
    );
    let words = refused(base(), &|row| {
        row.meta.as_mut().unwrap().subjects[1].entity_id = "ENT-01JC28NOTHELD0000000000001".into();
    });
    assert!(words.contains("no company the deployment holds"), "{words}");
    let words = refused(base(), &|row| {
        row.meta.as_mut().unwrap().subjects.remove(0);
    });
    assert!(words.contains("names no security"), "{words}");
    // Dated by its own fields, never a business date.
    let words = refused(base(), &|row| {
        row.meta.as_mut().unwrap().business_date = "2026-10-01".into();
    });
    assert!(words.contains("actions[0].meta.business_date"), "{words}");
    // A read naming an unspecified kind.
    assert!(list(
        &h,
        ListCorporateActionsRequest {
            subjects: vec![subject(AAPL, SubjectRole::Unspecified)],
            kinds: vec![0],
            ..Default::default()
        },
    )
    .unwrap_err()
    .starts_with("kinds[0]"));
}
