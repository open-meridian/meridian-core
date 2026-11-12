use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use meridian_bus::read_after::Position;
use meridian_bus::{Bus, MemoryBackend};
use meridian_clock::ManualClock;
use meridian_pb::v1::PartitionSequence;

/// A member whose record catches up when told: a replica behind its writer.
struct Lagging(Mutex<HashMap<String, u64>>);

impl Position for Lagging {
    fn heads(&self, _: &[String]) -> Result<HashMap<String, u64>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
}

fn bus() -> Arc<Bus> {
    Arc::new(Bus::single(
        "plugin-1",
        Arc::new(MemoryBackend::new()),
        Arc::new(ManualClock::at(1_790_553_600_000_000_000)),
    ))
}

/// Each read a member answered: what it was asked to read after, and how
/// far the member had reached.
type Answered = Arc<Mutex<Vec<(Option<Watermark>, u64)>>>;

/// A read served by a member at `lagging`: what it was asked to read after,
/// and how far the member had reached when it answered.
fn serve_read(bus: &Bus, topic: &str, lagging: Arc<Lagging>, partition: &'static str) -> Answered {
    let answered = Arc::new(Mutex::new(Vec::new()));
    let noting = Arc::clone(&answered);
    bus.serve(topic, move |envelope| {
        let at = lagging
            .0
            .lock()
            .unwrap()
            .get(partition)
            .copied()
            .unwrap_or(0);
        noting
            .lock()
            .unwrap()
            .push((envelope.meta.and_then(|m| m.read_after), at));
        Ok(("meridian.v1.ListQuotesReply".into(), Vec::new()))
    });
    answered
}

fn mark(partition: &str, sequence: u64) -> Watermark {
    Watermark {
        partitions: vec![PartitionSequence {
            partition: partition.into(),
            sequence,
        }],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_after_an_acknowledged_write_sees_it_on_a_lagging_member() {
    let bus = bus();
    Seen::keep_on(&bus);
    let lagging = Arc::new(Lagging(Mutex::new([("fx".to_string(), 3)].into())));
    bus.reads_after().keep("lake", lagging.clone());
    bus.reads_after().set_bound(Duration::from_secs(5));
    bus.serve("platform.lake.command.record-prices", |_| {
        let reply = domain::RecordObservationsReply {
            watermark: Some(mark("fx", 5)),
            ..Default::default()
        };
        Ok((
            "meridian.v1.RecordObservationsReply".into(),
            reply.encode_to_vec(),
        ))
    });
    let answered = serve_read(
        &bus,
        "platform.lake.query.list-quotes",
        lagging.clone(),
        "fx",
    );
    // Before any write, a read names nothing and is answered where it lands.
    bus.call("platform.lake.query.list-quotes", "t", vec![], None, None)
        .await
        .unwrap();
    bus.call(
        "platform.lake.command.record-prices",
        "t",
        vec![],
        None,
        None,
    )
    .await
    .unwrap();
    // The replica catches up a moment after the write was acknowledged.
    let catching = Arc::clone(&lagging);
    let caught = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        catching.0.lock().unwrap().insert("fx".into(), 5);
    });
    let started = Instant::now();
    bus.call("platform.lake.query.list-quotes", "t", vec![], None, None)
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(200));
    caught.join().unwrap();
    let answered = answered.lock().unwrap().clone();
    assert_eq!(
        answered[0],
        (None, 3),
        "a read before anything seen names nothing"
    );
    assert_eq!(answered[1].0, Some(mark("fx", 5)), "stamped by the sidecar");
    assert!(
        answered[1].1 >= 5,
        "never answered from behind the write: {:?}",
        answered[1]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_after_a_delivery_is_held_to_it_and_refused_past_the_bound() {
    let bus = bus();
    let seen = Seen::keep_on(&bus);
    let lagging = Arc::new(Lagging(Mutex::new([("P0".to_string(), 8)].into())));
    bus.reads_after().keep("book", lagging.clone());
    bus.reads_after().set_bound(Duration::from_millis(300));
    let answered = serve_read(&bus, "platform.book.query.list-positions", lagging, "P0");
    seen.delivered("platform.book.event.position-changed", "P0", 9);
    let started = Instant::now();
    let refused = bus
        .call(
            "platform.book.query.list-positions",
            "t",
            vec![],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(
        refused.to_string().contains("behind read_after"),
        "{refused}"
    );
    assert!(
        answered.lock().unwrap().is_empty(),
        "never answered from behind"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reads_own_answer_is_counted_and_each_store_is_its_own() {
    let bus = bus();
    let seen = Seen::keep_on(&bus);
    // Ruled 2026-11-10: the watermark a read was answered at is counted, so
    // the plugin's next read is never answered behind it (monotonic reads).
    seen.answered(
        "platform.lake.query.list-quotes",
        "meridian.v1.ListQuotesReply",
        &domain::ListQuotesReply {
            watermark: Some(mark("fx", 50)),
            ..Default::default()
        }
        .encode_to_vec(),
    );
    assert_eq!(seen.read_after("lake"), Some(mark("fx", 50)));
    seen.delivered("platform.lake.event.prices-recorded", "fx", 4);
    assert_eq!(
        seen.read_after("lake"),
        Some(mark("fx", 50)),
        "only forward"
    );
    seen.delivered("platform.lake.event.prices-recorded", "fx", 52);
    assert_eq!(seen.read_after("lake"), Some(mark("fx", 52)));
    assert_eq!(seen.read_after("book"), None);
    let calls = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&calls);
    bus.serve("platform.book.query.list-positions", move |envelope| {
        assert!(envelope.meta.unwrap().read_after.is_none());
        counting.fetch_add(1, Ordering::Relaxed);
        let reply = domain::ListPositionsReply {
            as_of: Some(mark("P0", 9)),
            ..Default::default()
        };
        Ok((
            "meridian.v1.ListPositionsReply".into(),
            reply.encode_to_vec(),
        ))
    });
    bus.call(
        "platform.book.query.list-positions",
        "t",
        vec![],
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // The book's answer, counted through the bus: the next read names it.
    assert_eq!(seen.read_after("book"), Some(mark("P0", 9)));
}

#[test]
fn each_read_replys_watermark_is_read_from_its_own_field() {
    macro_rules! replies {
        ($($reply:ident . $field:ident),* $(,)?) => {
            vec![$((
                stringify!($reply),
                domain::$reply {
                    $field: Some(mark("p", 7)),
                    ..Default::default()
                }
                .encode_to_vec(),
            )),*]
        };
    }
    let replies = replies!(
        ListBarsReply.watermark,
        ListCorporateActionsReply.watermark,
        ListExpirationsReply.watermark,
        ListFactorsReply.watermark,
        ListFilingsReply.watermark,
        ListHoldingsReply.watermark,
        ListOutcomesReply.watermark,
        ListPricesReply.watermark,
        ListQuotesReply.watermark,
        ListRatesReply.watermark,
        ListRatingActionsReply.watermark,
        ListReportedFactsReply.watermark,
        ListSeriesReply.watermark,
        ListStatisticsReply.watermark,
        ListTradesReply.watermark,
        QueryLakeReply.watermark,
        ListAccountAttributesReply.as_of,
        ListAccountFiguresReply.as_of,
        ListActivitiesReply.as_of,
        ListAlertsReply.as_of,
        ListBookingsReply.as_of,
        ListBreaksReply.as_of,
        ListCustodialPositionsReply.as_of,
        ListLifecycleEventsReply.as_of,
        ListLocatesReply.as_of,
        ListOrdersReply.as_of,
        ListPositionsReply.as_of,
        ListStatementsReply.as_of,
        ListSyncStatusesReply.as_of,
        ListTradingInstructionsReply.as_of,
    );
    assert_eq!(replies.len(), ANSWERED_AT.len());
    for (name, bytes) in replies {
        assert_eq!(
            answered_at(&format!("meridian.v1.{name}"), &bytes),
            vec![("p".to_string(), 7)],
            "{name}"
        );
    }
    // Every other field skipped: a page of rows before it, read past.
    let page = domain::ListQuotesReply {
        quotes: vec![Default::default(); 3],
        watermark: Some(mark("fx", 11)),
        next_cursor: "next".into(),
        ..Default::default()
    };
    assert_eq!(
        answered_at("meridian.v1.ListQuotesReply", &page.encode_to_vec()),
        vec![("fx".to_string(), 11)]
    );
    assert!(answered_at("meridian.v1.ListQuotesReply", &[]).is_empty());
    assert!(answered_at("meridian.v1.Unknown", &page.encode_to_vec()).is_empty());
}

#[test]
fn a_book_writes_journal_is_what_its_acknowledgement_counts() {
    let reply = domain::OrderReply {
        journal: Some(domain::JournalRef {
            partition: "P0".into(),
            sequence: 41,
            previous_sequence: 0,
        }),
        ..Default::default()
    };
    assert_eq!(
        acknowledged("meridian.v1.OrderReply", &reply.encode_to_vec()),
        vec![("P0".to_string(), 41)]
    );
    assert!(acknowledged("meridian.v1.Unknown", &[]).is_empty());
}

#[test]
fn the_session_holds_at_most_its_bound() {
    let seen = Seen::default();
    for n in 0..10_000 {
        seen.delivered(
            "platform.lake.event.prices-recorded",
            &format!("dataset-{n:05}"),
            1,
        );
    }
    let held = seen.at.lock().unwrap();
    assert!(held.bytes <= MOST_BYTES);
    let count = held.stores["lake"].len();
    assert!(count > 1000 && count < 10_000, "{count}");
}
