//! The filings slice in the lake (contract v28; FDP §1 items 6 to 11, ruling
//! 568; W10.4 to W10.6): a filing heard every version and read by its filer,
//! kind and filed date, and caught up; a fact restated and confirmed, read
//! as first published and as latest; a rating action read by the rated and
//! by its agency; a 13F's holdings read by the holder and by the security,
//! with their report, narrowed by its kind and date, and a 13D's amendment
//! read as published; and the refusals W10.4 names. The instrument store
//! stood in on the bus, as the lake's other tests stand it in.

use std::sync::Arc;

use meridian_bus::{Bus, Envelope, MemoryBackend, MessageMeta};
use meridian_clock::ManualClock;
use meridian_domain::v1::{
    AssetClass, DatasetEntitlement, DatasetRef, EntitlementsChangedEvent, FactDimension, Filing,
    FilingKind, FilingRef, FilingsRecordedEvent, Holding, HoldingsReport, HoldingsReportKind,
    InstrumentRecord, ListFilingsRequest, ListHoldingsRequest, ListRatingActionsRequest,
    ListReportedFactsRequest, Money, ObservationMeta, PartitionSequence, RatingAction,
    RatingActionKind, ReportedFact, ResolveCompanyRequest, ResolveIdentifierReply,
    ResolveIdentifierRequest, ResolveInstrumentReply, ResolveInstrumentRequest, Source, SourceTime,
    SourceTimeKind, SubjectRef, SubjectRole, UnitValue, ValueUnit, Watermark,
};
use meridian_pb::v1::{DatasetDeclaration, DatasetLicence, Decimal, ObservationMode};
use prost::Message;

use crate::row::Observation;
use crate::service::{self, Lake};

const NOW: i64 = 1_793_404_800_000_000_000; // 2026-10-31T00:00Z
const DAY: i64 = 86_400 * 1_000_000_000;
const AAPL: &str = "INS-01JC24AAPL00000000000001";
const KO: &str = "INS-01JC24KO0000000000000001";
const APPLE: &str = "ENT-01JC28APPLEINC000000000001";
const BERKSHIRE: &str = "ENT-01JC28BERKSHIRE00000000001";
const MOODYS: &str = "ENT-01JC28MOODYS00000000000001";
const FILINGS: &str = "sec-edgar-1:filings";
const FACTS: &str = "sec-edgar-1:facts";
const THIRTEEN_F: &str = "sec-edgar-1:13f";
const RATINGS: &str = "credit-ratings-1:ratings_moodys";

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
        let instrument =
            [AAPL, KO]
                .contains(&asked.instrument_id.as_str())
                .then(|| InstrumentRecord {
                    instrument_id: asked.instrument_id.clone(),
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
    // The store holds three companies and no other, refusing one it does
    // not hold as W3.32's resolve by company_id does.
    bus.serve(service::RESOLVE_COMPANY, |envelope| {
        let asked = ResolveCompanyRequest::decode(&envelope.payload[..]).unwrap();
        if ![APPLE, BERKSHIRE, MOODYS].contains(&asked.company_id.as_str()) {
            return Err(format!(
                "company_id: {:?} is no company this deployment holds",
                asked.company_id
            ));
        }
        Ok((
            "meridian.v1.CompanyRecord".into(),
            meridian_domain::v1::CompanyRecord {
                company_id: asked.company_id,
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

/// EDGAR's three datasets and one agency's history, signal-1 entitled to
/// every field of each.
fn configuration() -> EntitlementsChangedEvent {
    let dataset = |id: &str, vendor: &str, data_types: &[&str]| {
        let (instance, key) = id.split_once(':').unwrap();
        DatasetRef {
            dataset: id.into(),
            instance: instance.into(),
            vendor: vendor.into(),
            declaration: Some(DatasetDeclaration {
                key: key.into(),
                vendor: vendor.into(),
                data_types: data_types.iter().map(|t| t.to_string()).collect(),
                modes: vec![ObservationMode::Push as i32],
                licence_default: Some(DatasetLicence {
                    kept: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    };
    let datasets = vec![
        dataset(FILINGS, "SEC EDGAR", &["meridian.v1.Filing"]),
        dataset(FACTS, "SEC EDGAR", &["meridian.v1.ReportedFact"]),
        dataset(
            THIRTEEN_F,
            "SEC EDGAR",
            &["meridian.v1.HoldingsReport", "meridian.v1.Holding"],
        ),
        dataset(RATINGS, "Moody's", &["meridian.v1.RatingAction"]),
    ];
    EntitlementsChangedEvent {
        entitlements: datasets
            .iter()
            .map(|d| DatasetEntitlement {
                dataset: d.dataset.clone(),
                instance: "signal-1".into(),
                allowed: true,
                ..Default::default()
            })
            .collect(),
        datasets,
        licences: vec![],
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

fn number(value: &Option<Decimal>) -> String {
    meridian_domain::exact::Exact::from_wire(value.as_ref().unwrap())
        .unwrap()
        .to_string()
}

fn subject(entity: &str, role: SubjectRole) -> SubjectRef {
    SubjectRef {
        entity_id: entity.into(),
        role: role as i32,
    }
}

fn accession(number: &str, form: &str) -> Option<FilingRef> {
    Some(FilingRef {
        accession_number: number.into(),
        form: form.into(),
    })
}

/// An envelope for one of the slice's rows, published by the source at
/// `published`.
fn meta(
    row_key: &str,
    dataset: &str,
    subjects: Vec<SubjectRef>,
    valid: i64,
    published: i64,
) -> Option<ObservationMeta> {
    Some(ObservationMeta {
        row_key: row_key.into(),
        subjects,
        source: Some(Source {
            instance: dataset.split_once(':').unwrap().0.into(),
            dataset: dataset.into(),
            ..Default::default()
        }),
        valid_from_ns: valid,
        source_times: vec![SourceTime {
            kind: SourceTimeKind::Published as i32,
            at_ns: published,
        }],
        ..Default::default()
    })
}

fn filing(number: &str, form: &str, kind: FilingKind, items: &[&str], filed: &str) -> Filing {
    Filing {
        meta: meta(
            number,
            FILINGS,
            vec![subject(APPLE, SubjectRole::Filer)],
            NOW - 2 * DAY,
            NOW - 2 * DAY + 600_000_000_000,
        ),
        filing: accession(number, form),
        kind: kind as i32,
        items: items.iter().map(|i| i.to_string()).collect(),
        period_of_report: String::new(),
        filed_date: filed.into(),
    }
}

const EPS_KEY: &str = "320193:us-gaap:EarningsPerShareBasic:USD/shares:2026-06-28:2026-09-27";

fn eps(amount: &str, filed_by: &str, published: i64) -> ReportedFact {
    ReportedFact {
        meta: meta(
            EPS_KEY,
            FACTS,
            vec![subject(APPLE, SubjectRole::Filer)],
            published,
            published,
        ),
        concept: "us-gaap:EarningsPerShareBasic".into(),
        amount: Some(Money {
            amount: decimal(amount),
            currency_code: "USD".into(),
            instrument_id: String::new(),
        }),
        value: None,
        period_start: "2026-06-28".into(),
        period_end: "2026-09-27".into(),
        dimensions: vec![],
        filing: accession(filed_by, "10-K"),
    }
}

fn rating(rating: &str, action: RatingActionKind, valid: i64) -> RatingAction {
    RatingAction {
        meta: meta(
            &format!("moodys:issuer:{APPLE}:{valid}"),
            RATINGS,
            vec![
                subject(APPLE, SubjectRole::Subject),
                subject(MOODYS, SubjectRole::Rater),
            ],
            valid,
            valid + 150 * DAY,
        ),
        rating: rating.into(),
        rating_class: "long-term issuer".into(),
        action: action as i32,
    }
}

fn report(
    row_key: &str,
    kind: HoldingsReportKind,
    report_date: &str,
    form: &str,
    published: i64,
) -> HoldingsReport {
    HoldingsReport {
        meta: meta(
            row_key,
            THIRTEEN_F,
            vec![subject(BERKSHIRE, SubjectRole::Holder)],
            NOW - 30 * DAY,
            published,
        ),
        kind: kind as i32,
        report_date: report_date.into(),
        complete: kind == HoldingsReportKind::Form13f,
        filing: accession(row_key, form),
    }
}

fn holding(report_row_key: &str, held: &str, shares: &str, published: i64) -> Holding {
    Holding {
        meta: meta(
            &format!("{report_row_key}:{held}"),
            THIRTEEN_F,
            vec![
                subject(BERKSHIRE, SubjectRole::Holder),
                subject(held, SubjectRole::Held),
            ],
            NOW - 30 * DAY,
            published,
        ),
        report_row_key: report_row_key.into(),
        quantity: Some(UnitValue {
            value: decimal(shares),
            unit: ValueUnit::Shares as i32,
            ..Default::default()
        }),
        market_value: None,
        percent_of_class: None,
    }
}

fn record(h: &Harness, field: &str, rows: Vec<Observation>) -> Result<(u32, u32, u32), String> {
    let instance = rows[0].meta().source.clone().unwrap().instance;
    tokio::task::block_in_place(|| h.lake.record(field, rows, "", &envelope(&instance)))
        .map(|reply| (reply.recorded, reply.restated, reply.unchanged))
}

fn reader() -> Envelope {
    envelope("signal-1")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filing_is_heard_every_version_and_read_by_its_filer_kind_and_filed_date() {
    let h = harness();
    let mut heard = h
        .bus
        .subscribe_every(&format!("platform.lake.{FILINGS}.event.filings-recorded"));
    let eight_k = filing(
        "0000320193-26-000105",
        "8-K",
        FilingKind::CurrentReport,
        &["2.02", "9.01"],
        "2026-10-30",
    );
    let ten_k = filing(
        "0000320193-25-000079",
        "10-K",
        FilingKind::AnnualReport,
        &[],
        "2025-10-31",
    );
    let rows = vec![
        Observation::Filing(eight_k.clone()),
        Observation::Filing(ten_k),
    ];
    assert_eq!(record(&h, "filings", rows).unwrap(), (2, 0, 0));
    let first = FilingsRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
        .unwrap()
        .filing
        .unwrap();
    assert_eq!(
        first.items,
        vec!["2.02", "9.01"],
        "Item 2.02 dates the report"
    );
    heard.recv().await.unwrap();

    // The 8-K report dates since a date, by the filer in its role.
    let read = |request: ListFilingsRequest| {
        tokio::task::block_in_place(|| h.lake.list_filings(request, &reader()))
    };
    let earnings = read(ListFilingsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Filer)],
        kinds: vec![FilingKind::CurrentReport as i32],
        from_date: "2024-10-01".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(earnings.filings.len(), 1);
    assert_eq!(earnings.filings[0].filed_date, "2026-10-30");
    assert_eq!(earnings.datasets[0].dataset, FILINGS);
    // By its filed date: the 10-K alone before 2026.
    let last_year = read(ListFilingsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Unspecified)],
        to_date: "2025-12-31".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(last_year.filings.len(), 1);
    assert_eq!(last_year.filings[0].kind, FilingKind::AnnualReport as i32);
    // Never in a role the row does not give it.
    let as_holder = read(ListFilingsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Holder)],
        ..Default::default()
    })
    .unwrap();
    assert!(as_holder.filings.is_empty());
    let watermark = earnings.watermark.clone().unwrap();

    // An amendment of its items is the next version, heard and caught up.
    let mut amended = eight_k;
    amended.items.push("7.01".into());
    assert_eq!(
        record(&h, "filings", vec![Observation::Filing(amended)]).unwrap(),
        (0, 1, 0)
    );
    let again = FilingsRecordedEvent::decode(&heard.recv().await.unwrap().envelope.payload[..])
        .unwrap()
        .filing
        .unwrap();
    assert_eq!(again.meta.unwrap().version, 2);
    let caught_up = read(ListFilingsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Filer)],
        after_watermark: Some(watermark.clone()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(caught_up.filings.len(), 1);
    assert_eq!(caught_up.filings[0].items.len(), 3);
    let every = read(ListFilingsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Filer)],
        after_watermark: Some(Watermark {
            partitions: vec![PartitionSequence {
                partition: FILINGS.into(),
                sequence: 0,
            }],
        }),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(every.filings.len(), 3, "every version recorded after it");
    assert!(read(ListFilingsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Filer)],
        after_watermark: Some(watermark),
        from_date: "2026-01-01".into(),
        ..Default::default()
    })
    .unwrap_err()
    .contains("after_watermark"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fact_restated_and_confirmed_reads_as_first_published_and_as_latest() {
    let h = harness();
    let first = NOW - 20 * DAY;
    let confirmed = NOW - 10 * DAY;
    let restated = NOW - DAY;
    let fact = |row: ReportedFact| vec![Observation::ReportedFact(row)];
    assert_eq!(
        record(
            &h,
            "facts",
            fact(eps("1.85", "0000320193-26-000107", first))
        )
        .unwrap(),
        (1, 0, 0)
    );
    // A later filing repeating it: a confirmation, not a restatement.
    assert_eq!(
        record(
            &h,
            "facts",
            fact(eps("1.850", "0000320193-26-000120", confirmed))
        )
        .unwrap(),
        (0, 0, 1)
    );
    // The same delivery again changes nothing.
    assert_eq!(
        record(
            &h,
            "facts",
            fact(eps("1.850", "0000320193-26-000120", confirmed))
        )
        .unwrap(),
        (0, 0, 1)
    );
    // A later filing's different value: restated.
    assert_eq!(
        record(
            &h,
            "facts",
            fact(eps("1.86", "0000320193-27-000011", restated))
        )
        .unwrap(),
        (0, 1, 0)
    );
    let read = |published_as_of_ns: i64, concepts: Vec<String>| {
        tokio::task::block_in_place(|| {
            h.lake.list_reported_facts(
                ListReportedFactsRequest {
                    subjects: vec![subject(APPLE, SubjectRole::Filer)],
                    concepts,
                    published_as_of_ns,
                    ..Default::default()
                },
                &reader(),
            )
        })
        .unwrap()
    };
    let eps_concept = || vec!["us-gaap:EarningsPerShareBasic".to_string()];
    let latest = read(0, eps_concept());
    assert_eq!(latest.facts.len(), 1);
    assert_eq!(
        number(&latest.facts[0].amount.as_ref().unwrap().amount),
        "1.86"
    );
    assert_eq!(latest.facts[0].meta.as_ref().unwrap().version, 3);
    let as_first_published = read(first, eps_concept());
    assert_eq!(
        number(&as_first_published.facts[0].amount.as_ref().unwrap().amount),
        "1.85"
    );
    assert_eq!(
        as_first_published.facts[0].meta.as_ref().unwrap().version,
        1
    );
    let as_confirmed = read(confirmed, eps_concept());
    assert_eq!(
        as_confirmed.facts[0]
            .filing
            .as_ref()
            .unwrap()
            .accession_number,
        "0000320193-26-000120",
        "the confirmation names the filing that repeated it"
    );
    // Another concept answers nothing; a period ending before the range
    // nothing either.
    assert!(read(0, vec!["us-gaap:Revenues".into()]).facts.is_empty());
    let before = tokio::task::block_in_place(|| {
        h.lake.list_reported_facts(
            ListReportedFactsRequest {
                subjects: vec![subject(APPLE, SubjectRole::Unspecified)],
                to_date: "2026-09-26".into(),
                ..Default::default()
            },
            &reader(),
        )
    })
    .unwrap();
    assert!(before.facts.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rating_action_is_read_by_the_rated_and_by_its_agency_on_its_date() {
    let h = harness();
    let affirmed = NOW - 180 * DAY; // 2026-05-04
    let rows = vec![
        Observation::RatingAction(rating("Aaa", RatingActionKind::New, NOW - 900 * DAY)),
        Observation::RatingAction(rating("Aaa", RatingActionKind::Affirmed, affirmed)),
    ];
    assert_eq!(record(&h, "actions", rows).unwrap(), (2, 0, 0));
    let read = |request: ListRatingActionsRequest| {
        tokio::task::block_in_place(|| h.lake.list_rating_actions(request, &reader())).unwrap()
    };
    let history = read(ListRatingActionsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Subject)],
        ..Default::default()
    });
    assert_eq!(history.actions.len(), 2);
    let by_agency = read(ListRatingActionsRequest {
        subjects: vec![subject(MOODYS, SubjectRole::Rater)],
        kinds: vec![RatingActionKind::Affirmed as i32],
        ..Default::default()
    });
    assert_eq!(by_agency.actions.len(), 1);
    // Dated by the action, its valid time's day, never the published time.
    let this_year = read(ListRatingActionsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Unspecified)],
        from_date: "2026-01-01".into(),
        ..Default::default()
    });
    assert_eq!(this_year.actions.len(), 1);
    assert_eq!(
        this_year.actions[0].action,
        RatingActionKind::Affirmed as i32
    );
    let not_a_rater = read(ListRatingActionsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Rater)],
        ..Default::default()
    });
    assert!(not_a_rater.actions.is_empty());
    // As published before the history carried the affirmation.
    let as_published = read(ListRatingActionsRequest {
        subjects: vec![subject(APPLE, SubjectRole::Subject)],
        published_as_of_ns: affirmed + DAY,
        ..Default::default()
    });
    assert_eq!(as_published.actions.len(), 1);
    assert_eq!(as_published.actions[0].action, RatingActionKind::New as i32);
}

#[tokio::test(flavor = "multi_thread")]
async fn holdings_are_read_by_holder_and_by_security_with_their_report_once() {
    let h = harness();
    let filed = NOW - 15 * DAY;
    let q3 = "0001067983-26-000045";
    let q2 = "0001067983-26-000031";
    let reports = vec![
        Observation::HoldingsReport(report(
            q3,
            HoldingsReportKind::Form13f,
            "2026-09-30",
            "13F-HR",
            filed,
        )),
        Observation::HoldingsReport(report(
            q2,
            HoldingsReportKind::Form13f,
            "2026-06-30",
            "13F-HR",
            filed - 90 * DAY,
        )),
    ];
    assert_eq!(record(&h, "reports", reports).unwrap(), (2, 0, 0));
    let holdings = vec![
        Observation::Holding(holding(q3, AAPL, "300000000", filed)),
        Observation::Holding(holding(q3, KO, "400000000", filed)),
        Observation::Holding(holding(q2, AAPL, "280000000", filed - 90 * DAY)),
    ];
    assert_eq!(record(&h, "holdings", holdings).unwrap(), (3, 0, 0));
    let read = |request: ListHoldingsRequest| {
        tokio::task::block_in_place(|| h.lake.list_holdings(request, &reader())).unwrap()
    };
    // A security's holders as of a quarter: Berkshire's Q3 holding of
    // Apple, with its report.
    let holders = read(ListHoldingsRequest {
        subjects: vec![subject(AAPL, SubjectRole::Held)],
        kinds: vec![HoldingsReportKind::Form13f as i32],
        from_date: "2026-09-30".into(),
        ..Default::default()
    });
    assert_eq!(holders.holdings.len(), 1);
    assert_eq!(
        number(&holders.holdings[0].quantity.as_ref().unwrap().value),
        "300000000"
    );
    assert_eq!(holders.reports.len(), 1);
    assert_eq!(holders.reports[0].report_date, "2026-09-30");
    assert!(holders.reports[0].complete);
    // A report's holdings by its holder: three holdings, two reports, each
    // report once.
    let book = read(ListHoldingsRequest {
        subjects: vec![subject(BERKSHIRE, SubjectRole::Holder)],
        ..Default::default()
    });
    assert_eq!(book.holdings.len(), 3);
    assert_eq!(book.reports.len(), 2);
    // Narrowed by the report's date: Q2 alone.
    let q2_only = read(ListHoldingsRequest {
        subjects: vec![subject(BERKSHIRE, SubjectRole::Holder)],
        to_date: "2026-07-01".into(),
        ..Default::default()
    });
    assert_eq!(q2_only.holdings.len(), 1);
    assert_eq!(q2_only.reports[0].meta.as_ref().unwrap().row_key, q2);
    // The security asked in a role it does not hold there: nothing.
    assert!(read(ListHoldingsRequest {
        subjects: vec![subject(AAPL, SubjectRole::Holder)],
        ..Default::default()
    })
    .holdings
    .is_empty());
    // A 13G kind asked: nothing of the 13Fs.
    assert!(read(ListHoldingsRequest {
        subjects: vec![subject(BERKSHIRE, SubjectRole::Holder)],
        kinds: vec![HoldingsReportKind::Schedule13g as i32],
        ..Default::default()
    })
    .holdings
    .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stakes_amendment_is_its_next_version_read_as_published() {
    let h = harness();
    let crossed = NOW - 20 * DAY;
    let amended = NOW - 5 * DAY;
    let stake = "0001067983-26-000050";
    let thirteen_d = |published: i64, date: &str, shares: &str, percent: &str| {
        let mut row = holding(stake, KO, shares, published);
        row.percent_of_class = Some(UnitValue {
            value: decimal(percent),
            unit: ValueUnit::Percent as i32,
            ..Default::default()
        });
        (
            Observation::HoldingsReport(report(
                stake,
                HoldingsReportKind::Schedule13d,
                date,
                "SC 13D",
                published,
            )),
            Observation::Holding(row),
        )
    };
    let (report_v1, holding_v1) = thirteen_d(crossed, "2026-10-09", "460000000", "10.6");
    record(&h, "reports", vec![report_v1]).unwrap();
    record(&h, "holdings", vec![holding_v1]).unwrap();
    let (report_v2, holding_v2) = thirteen_d(amended, "2026-10-24", "520000000", "12.0");
    assert_eq!(record(&h, "reports", vec![report_v2]).unwrap(), (0, 1, 0));
    assert_eq!(record(&h, "holdings", vec![holding_v2]).unwrap(), (0, 1, 0));
    let read = |published_as_of_ns: i64| {
        tokio::task::block_in_place(|| {
            h.lake.list_holdings(
                ListHoldingsRequest {
                    subjects: vec![subject(KO, SubjectRole::Held)],
                    kinds: vec![HoldingsReportKind::Schedule13d as i32],
                    published_as_of_ns,
                    ..Default::default()
                },
                &reader(),
            )
        })
        .unwrap()
    };
    let now = read(0);
    assert_eq!(
        number(&now.holdings[0].percent_of_class.as_ref().unwrap().value),
        "12.0"
    );
    assert_eq!(now.reports[0].report_date, "2026-10-24");
    let then = read(crossed + DAY);
    assert_eq!(
        number(&then.holdings[0].percent_of_class.as_ref().unwrap().value),
        "10.6"
    );
    assert_eq!(then.reports[0].report_date, "2026-10-09");
    assert_eq!(then.reports[0].meta.as_ref().unwrap().version, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn what_w10_4_refuses_of_the_filings_slice_is_refused_naming_the_field() {
    let h = harness();
    let refused = |field: &str, row: Observation| record(&h, field, vec![row]).unwrap_err();
    let base_filing = || {
        filing(
            "0000320193-26-000105",
            "8-K",
            FilingKind::CurrentReport,
            &["2.02"],
            "2026-10-30",
        )
    };
    // A filing with no accession; an unspecified kind; no filed date.
    let mut row = base_filing();
    row.filing.as_mut().unwrap().accession_number.clear();
    let words = refused("filings", Observation::Filing(row));
    assert!(
        words.starts_with("filings[0].filing.accession_number"),
        "{words}"
    );
    let mut row = base_filing();
    row.kind = 0;
    let words = refused("filings", Observation::Filing(row));
    assert!(words.starts_with("filings[0].kind"), "{words}");
    let mut row = base_filing();
    row.filed_date.clear();
    let words = refused("filings", Observation::Filing(row));
    assert!(words.starts_with("filings[0].filed_date"), "{words}");
    // A company in a role a filing never gives it; one the deployment does
    // not hold; a business date; no valid time.
    let mut row = base_filing();
    row.meta.as_mut().unwrap().subjects[0].role = SubjectRole::Rater as i32;
    let words = refused("filings", Observation::Filing(row));
    assert!(
        words.starts_with("filings[0].meta.subjects[0].role"),
        "{words}"
    );
    let mut row = base_filing();
    row.meta.as_mut().unwrap().subjects[0].entity_id = "ENT-01JC28NOTHELD0000000000001".into();
    let words = refused("filings", Observation::Filing(row));
    assert!(words.contains("no company the deployment holds"), "{words}");
    let mut row = base_filing();
    row.meta.as_mut().unwrap().business_date = "2026-10-30".into();
    let words = refused("filings", Observation::Filing(row));
    assert!(
        words.starts_with("filings[0].meta.business_date"),
        "{words}"
    );
    let mut row = base_filing();
    row.meta.as_mut().unwrap().valid_from_ns = 0;
    let words = refused("filings", Observation::Filing(row));
    assert!(
        words.starts_with("filings[0].meta.valid_from_ns"),
        "{words}"
    );
    // A fact with neither amount nor value, and with both.
    let mut row = eps("1.85", "0000320193-26-000107", NOW);
    row.amount = None;
    let words = refused("facts", Observation::ReportedFact(row));
    assert!(words.starts_with("facts[0].amount"), "{words}");
    let mut row = eps("1.85", "0000320193-26-000107", NOW);
    row.value = Some(UnitValue {
        value: decimal("1.85"),
        unit: ValueUnit::Ratio as i32,
        ..Default::default()
    });
    let words = refused("facts", Observation::ReportedFact(row));
    assert!(words.starts_with("facts[0].value"), "{words}");
    let mut row = eps("1.85", "0000320193-26-000107", NOW);
    row.dimensions.push(FactDimension {
        axis: "us-gaap:StatementBusinessSegmentsAxis".into(),
        member: String::new(),
    });
    let words = refused("facts", Observation::ReportedFact(row));
    assert!(
        words.starts_with("facts[0].dimensions[0].member"),
        "{words}"
    );
    // A rating action with no rater.
    let mut row = rating("Aaa", RatingActionKind::Affirmed, NOW - DAY);
    row.meta.as_mut().unwrap().subjects.pop();
    let words = refused("actions", Observation::RatingAction(row));
    assert!(
        words.starts_with("actions[0].meta.subjects names no rater"),
        "{words}"
    );
    let mut row = rating("", RatingActionKind::Downgrade, NOW - DAY);
    row.rating_class = "long-term issuer".into();
    let words = refused("actions", Observation::RatingAction(row));
    assert!(words.starts_with("actions[0].rating"), "{words}");
    // A withdrawal states no rating.
    let row = rating("", RatingActionKind::Withdrawn, NOW - DAY);
    assert_eq!(
        record(&h, "actions", vec![Observation::RatingAction(row)]).unwrap(),
        (1, 0, 0)
    );
    // A report of an unspecified kind; a holding naming no report; a
    // holding naming nothing held.
    let mut row = report(
        "0001067983-26-000045",
        HoldingsReportKind::Form13f,
        "2026-09-30",
        "13F-HR",
        NOW,
    );
    row.kind = 0;
    let words = refused("reports", Observation::HoldingsReport(row));
    assert!(words.starts_with("reports[0].kind"), "{words}");
    let mut row = holding("0001067983-26-000045", AAPL, "1", NOW);
    row.report_row_key.clear();
    let words = refused("holdings", Observation::Holding(row));
    assert!(words.starts_with("holdings[0].report_row_key"), "{words}");
    let mut row = holding("0001067983-26-000045", AAPL, "1", NOW);
    row.meta.as_mut().unwrap().subjects.pop();
    let words = refused("holdings", Observation::Holding(row));
    assert!(words.contains("names nothing held"), "{words}");
    // A read naming an unspecified kind.
    let reads = tokio::task::block_in_place(|| {
        h.lake.list_holdings(
            ListHoldingsRequest {
                subjects: vec![subject(AAPL, SubjectRole::Held)],
                kinds: vec![0],
                ..Default::default()
            },
            &reader(),
        )
    })
    .unwrap_err();
    assert!(reads.starts_with("kinds[0]"), "{reads}");
}
