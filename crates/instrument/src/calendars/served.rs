//! The calendars on the bus (contract v22; W3.16 to W3.21): a pull kept and
//! announced, the reads answered from what is held, a miss reported by its
//! subject.

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::{Bus, MemoryBackend};
use meridian_domain::v1::{
    compute_date_request::Inputs, BusinessDayInputs, CalendarAppliedEvent, CalendarCoverage,
    CalendarDay, CalendarWeekend, ComputeDateReply, ListCalendarsReply, PullCalendarReply,
    ReadCalendarReply,
};
use prost::Message;

use super::*;
use crate::service::{
    CALENDAR_APPLIED, CALENDAR_MISSING, CALENDAR_PULLED, COMPUTE_DATE, LIST_CALENDARS,
    READ_CALENDAR,
};
use crate::{InstrumentService, MemoryStore, Store};

const USD: &str = "CAL-01JAE000000000000000USD1";

fn usd(version: i64, closed: &[&str]) -> CalendarRecord {
    CalendarRecord {
        calendar_id: USD.into(),
        name: "US dollar settlement".into(),
        weekends: vec![CalendarWeekend {
            weekdays: vec![Weekday::Saturday as i32, Weekday::Sunday as i32],
            valid_from_date: "2016-01-01".into(),
        }],
        days: closed
            .iter()
            .map(|date| CalendarDay {
                date: (*date).into(),
                kind: CalendarDayKind::Closed as i32,
                name: "a holiday".into(),
                source: "fed".into(),
            })
            .collect(),
        coverage: Some(CalendarCoverage {
            from_date: "2016-01-01".into(),
            to_date: "2028-12-31".into(),
        }),
        version,
        ..Default::default()
    }
}

async fn call<R: Message + Default>(
    bus: &Bus,
    topic: &str,
    kind: &str,
    message: impl Message,
) -> R {
    let (_, bytes) = bus
        .call(topic, kind, message.encode_to_vec(), None, None)
        .await
        .unwrap();
    R::decode(&bytes[..]).unwrap()
}

async fn next<M: Message + Default>(heard: &mut meridian_bus::Subscription) -> M {
    let delivery = tokio::time::timeout(Duration::from_secs(2), heard.recv())
        .await
        .expect("nothing was heard within two seconds")
        .unwrap();
    M::decode(&delivery.envelope.payload[..]).unwrap()
}

#[tokio::test]
async fn a_pulled_calendar_is_kept_announced_and_answered_and_a_miss_is_reported() {
    let bus = Arc::new(Bus::single(
        "instrument",
        Arc::new(MemoryBackend::new()),
        Arc::new(meridian_clock::ManualClock::at(1_760_400_000_000_000_000)),
    ));
    let store = Arc::new(MemoryStore::new());
    let mut applied = bus.subscribe(CALENDAR_APPLIED);
    let mut missing = bus.subscribe(CALENDAR_MISSING);
    let running = InstrumentService::new(
        Arc::clone(&bus),
        store.clone() as Arc<dyn Store>,
        Arc::new(meridian_clock::ManualClock::at(1_760_400_000_000_000_000)),
    )
    .start();
    tokio::spawn(running);

    // Not held yet: the date read says so and names the calendar missing.
    let business_day = |date: &str| ComputeDateRequest {
        inputs: Some(Inputs::BusinessDay(BusinessDayInputs {
            date: date.into(),
            calendars: vec![USD.into()],
        })),
        as_of_ns: 0,
    };
    let answer: ComputeDateReply = call(
        &bus,
        COMPUTE_DATE,
        "meridian.v1.ComputeDateRequest",
        business_day("2026-10-12"),
    )
    .await;
    assert_eq!(answer.outcome, DateOutcome::CalendarNotHeld as i32);
    let reported: MissingCalendarDetectedEvent = next(&mut missing).await;
    assert_eq!(reported.subject.unwrap().calendar_id, USD);

    // The conductor's pull: kept, each version once, each announced.
    for _ in 0..2 {
        bus.publish(
            CALENDAR_PULLED,
            "meridian.v1.PullCalendarReply",
            PullCalendarReply {
                calendars: vec![usd(1, &["2026-10-12"])],
                rules: Vec::new(),
            }
            .encode_to_vec(),
            None,
            None,
        )
        .unwrap();
    }
    let event: CalendarAppliedEvent = next(&mut applied).await;
    assert_eq!((event.calendar_id.as_str(), event.version), (USD, 1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(store.calendar_versions().unwrap().len(), 1);

    let answer: ComputeDateReply = call(
        &bus,
        COMPUTE_DATE,
        "meridian.v1.ComputeDateRequest",
        business_day("2026-10-12"),
    )
    .await;
    assert_eq!(answer.outcome, DateOutcome::Computed as i32);
    assert!(!answer.business_day);
    assert_eq!(answer.basis.unwrap().calendars[0].version, 1);

    let read: ReadCalendarReply = call(
        &bus,
        READ_CALENDAR,
        "meridian.v1.ReadCalendarRequest",
        ReadCalendarRequest {
            calendar_id: USD.into(),
            from_date: "2026-10-01".into(),
            to_date: "2026-10-31".into(),
            as_of_ns: 0,
        },
    )
    .await;
    assert!(read.found);
    assert_eq!(read.days.len(), 1);

    let listed: ListCalendarsReply = call(
        &bus,
        LIST_CALENDARS,
        "meridian.v1.ListCalendarsRequest",
        ListCalendarsRequest::default(),
    )
    .await;
    assert_eq!(listed.calendars.len(), 1);
    assert_eq!(listed.calendars[0].coverage_to_date, "2028-12-31");

    // A range that ends before it starts is refused, naming to_date.
    let refused = bus
        .call(
            READ_CALENDAR,
            "meridian.v1.ReadCalendarRequest",
            ReadCalendarRequest {
                calendar_id: USD.into(),
                from_date: "2026-10-31".into(),
                to_date: "2026-10-01".into(),
                as_of_ns: 0,
            }
            .encode_to_vec(),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("to_date"), "{refused}");
}
