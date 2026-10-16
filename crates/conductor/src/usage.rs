//! The weekly usage report (contract v24, W3.28): per identifier scheme,
//! the counts the instrument store keeps -- how many instruments carry it,
//! how many resolve only through it, how many misses arrived with it as
//! their only identifier -- sent to the platform with the scheme names the
//! deployment declares it is licensed for, unless the deployment admin has
//! turned the report off. Counts only: never a value, a holding, a quantity
//! or an account.
//!
//! The counts are the store's: the conductor asks for them on
//! `platform.reference.query.list-identifier-usage` and writes into no
//! other component's store (decisions/011). Each report sent is the
//! deployment's own record, received or not; one not received is not sent
//! again, the next week's replacing it on the platform. `licensed_schemes`
//! are those the deployment admin declares (contract v25; until v25 the
//! report sent them empty, ruled 2026-10-15).

use std::sync::Arc;
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::date::Date;
use meridian_domain::v1::{
    IdentifierUsageReport, ListIdentifierUsageReply, ListIdentifierUsageRequest, SentUsageReport,
};
use prost::Message;

use crate::platform::Platform;

/// W3.28: the store's counts for a period.
pub const LIST_IDENTIFIER_USAGE: &str = "platform.reference.query.list-identifier-usage";

/// How often the sender looks whether a week's report is due.
pub const LOOK_EVERY: Duration = Duration::from_secs(3_600);

/// How long the store has to answer.
pub const ASK_WITHIN: Duration = Duration::from_secs(30);

/// Where the conductor keeps its setting and the reports sent: its own
/// configuration store, which the binary hands in.
pub trait UsageBook: Send + Sync {
    /// The deployment admin's setting, on by default.
    fn reports_usage(&self) -> Result<bool, String>;
    /// The reports sent, newest first, at most `limit`.
    fn sent(&self, limit: usize) -> Result<Vec<SentUsageReport>, String>;
    /// A report sent, its own record.
    fn record(&self, sent: &SentUsageReport) -> Result<(), String>;
    /// The schemes the deployment admin declares it is licensed for
    /// (contract v25, W3.28); none until one is declared.
    fn licensed_schemes(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

/// The week a report covers: the last whole one, Monday to Sunday, before
/// the week `today` is in.
pub fn last_week(today: Date) -> (Date, Date) {
    // 1970-01-01 was a Thursday: Monday is day 0 of a week counted so.
    let days = today.days_since_epoch();
    let weekday = (days + 3).rem_euclid(7);
    let this_monday = days - weekday;
    let start = Date::from_days(this_monday - 7).unwrap_or(today);
    let end = Date::from_days(this_monday - 1).unwrap_or(today);
    (start, end)
}

/// The store's reads and writes block: each on the blocking pool, never on
/// the runtime driving the bus (a Postgres store panics there).
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|failed| format!("the conductor's store: {failed}"))?
}

/// Sends the week's report once it is due.
pub struct UsageSender {
    bus: Arc<Bus>,
    platform: Arc<Platform>,
    clock: Arc<dyn meridian_clock::Clock>,
    book: Arc<dyn UsageBook>,
}

impl UsageSender {
    pub fn new(
        bus: Arc<Bus>,
        platform: Arc<Platform>,
        clock: Arc<dyn meridian_clock::Clock>,
        book: Arc<dyn UsageBook>,
    ) -> Self {
        Self {
            bus,
            platform,
            clock,
            book,
        }
    }

    /// The report for a period: the store's counts asked for, sent to the
    /// platform, and kept as sent, received or not. An error where the
    /// store did not answer: nothing is sent or kept, and it is asked again.
    pub async fn send_for(&self, start: Date, end: Date) -> Result<SentUsageReport, String> {
        let (_, bytes) = self
            .bus
            .call(
                LIST_IDENTIFIER_USAGE,
                "meridian.v1.ListIdentifierUsageRequest",
                ListIdentifierUsageRequest {
                    period_start: start.to_string(),
                    period_end: end.to_string(),
                }
                .encode_to_vec(),
                None,
                Some(ASK_WITHIN),
            )
            .await
            .map_err(|failed| format!("the instrument store's counts: {failed}"))?;
        let counts = ListIdentifierUsageReply::decode(bytes.as_slice())
            .map_err(|failed| format!("the instrument store's counts: {failed}"))?;
        let book = Arc::clone(&self.book);
        let licensed_schemes = blocking(move || book.licensed_schemes()).await?;
        let report = IdentifierUsageReport {
            period_start: start.to_string(),
            period_end: end.to_string(),
            licensed_schemes,
            scheme_counts: counts.scheme_counts,
        };
        let now = self.clock.now_ns();
        let received = match self.platform.report_identifier_usage(&report, now).await {
            Ok(received) => received,
            Err(failed) => {
                tracing::info!(%failed, "the usage report was not received; the next replaces it");
                false
            }
        };
        let sent = SentUsageReport {
            sent_at_ns: now,
            received,
            report: Some(report),
        };
        let (book, kept) = (Arc::clone(&self.book), sent.clone());
        blocking(move || book.record(&kept)).await?;
        Ok(sent)
    }

    /// The last whole week's report, unless it was sent already or the
    /// report is off. Answers what was sent, if anything.
    pub async fn send_if_due(&self) -> Result<Option<SentUsageReport>, String> {
        let book = Arc::clone(&self.book);
        if !blocking(move || book.reports_usage()).await? {
            return Ok(None);
        }
        let (start, end) = last_week(Date::of_utc_instant(self.clock.now_ns()));
        let book = Arc::clone(&self.book);
        let sent = blocking(move || book.sent(1)).await?;
        let covered = sent.first().and_then(|last| {
            last.report
                .as_ref()
                .and_then(|report| Date::parse(&report.period_end).ok())
        });
        if covered.is_some_and(|covered| covered >= end) {
            return Ok(None);
        }
        self.send_for(start, end).await.map(Some)
    }

    /// Look hourly whether the week's report is due, from the start.
    pub async fn start(self: Arc<Self>) {
        loop {
            if let Err(failed) = self.send_if_due().await {
                tracing::info!(%failed, "the usage report was not sent; tried again shortly");
            }
            tokio::time::sleep(LOOK_EVERY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use meridian_bus::MemoryBackend;
    use meridian_domain::v1::SchemeUsageCount;

    use crate::platform::tests::{platform as platform_with, reply, Fake};

    // 2026-10-14, a Wednesday.
    const NOW: i64 = 1_792_022_400_000_000_000;

    #[derive(Default)]
    struct Book {
        off: bool,
        sent: Mutex<Vec<SentUsageReport>>,
        licensed: Vec<String>,
    }

    impl UsageBook for Book {
        fn reports_usage(&self) -> Result<bool, String> {
            Ok(!self.off)
        }
        fn sent(&self, limit: usize) -> Result<Vec<SentUsageReport>, String> {
            Ok(self
                .sent
                .lock()
                .unwrap()
                .iter()
                .rev()
                .take(limit)
                .cloned()
                .collect())
        }
        fn record(&self, sent: &SentUsageReport) -> Result<(), String> {
            self.sent.lock().unwrap().push(sent.clone());
            Ok(())
        }
        fn licensed_schemes(&self) -> Result<Vec<String>, String> {
            Ok(self.licensed.clone())
        }
    }

    fn wired(book: Arc<Book>, fake: Arc<Fake>) -> UsageSender {
        let clock = Arc::new(meridian_clock::ManualClock::at(NOW));
        let bus = Arc::new(Bus::single(
            "conductor-1",
            Arc::new(MemoryBackend::new()),
            clock.clone(),
        ));
        bus.serve(LIST_IDENTIFIER_USAGE, |envelope| {
            let asked = ListIdentifierUsageRequest::decode(&envelope.payload[..]).unwrap();
            assert_eq!(
                (asked.period_start.as_str(), asked.period_end.as_str()),
                ("2026-10-05", "2026-10-11")
            );
            Ok((
                "meridian.v1.ListIdentifierUsageReply".into(),
                ListIdentifierUsageReply {
                    scheme_counts: vec![SchemeUsageCount {
                        scheme: "cusip".into(),
                        instruments_carrying: 412,
                        instruments_only_by: 37,
                        misses_only_by: 4,
                    }],
                }
                .encode_to_vec(),
            ))
        });
        UsageSender::new(bus, Arc::new(platform_with(fake)), clock, book)
    }

    #[tokio::test]
    async fn the_weeks_counts_are_sent_once_and_kept_as_sent() {
        let book = Arc::new(Book::default());
        let fake = Fake::new(vec![Ok(reply(200, "{\"received\": true}"))]);
        let sender = wired(book.clone(), fake.clone());
        let sent = sender.send_if_due().await.unwrap().expect("due");
        assert!(sent.received);
        let report = sent.report.unwrap();
        assert_eq!(report.period_end, "2026-10-11");
        assert!(report.licensed_schemes.is_empty(), "none declared");
        // Counts and scheme names only, as the platform reads them.
        let body: serde_json::Value = serde_json::from_str(&fake.bodies()[0]).unwrap();
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "licensed_schemes",
                "period_end",
                "period_start",
                "scheme_counts"
            ]
        );
        assert_eq!(body["scheme_counts"][0]["instruments_only_by"], 37);
        assert!(fake.urls()[0].ends_with("/api/v1/reference/usage-reports"));
        // The same week is not sent again.
        assert!(sender.send_if_due().await.unwrap().is_none());
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn not_received_is_kept_so_and_off_sends_nothing() {
        let book = Arc::new(Book::default());
        let fake = Fake::new(vec![Ok(reply(404, "{}"))]);
        let sender = wired(book.clone(), fake);
        let sent = sender.send_if_due().await.unwrap().expect("due");
        assert!(!sent.received, "a platform before v24 is not reached");
        assert_eq!(book.sent.lock().unwrap().len(), 1);

        let off = Arc::new(Book {
            off: true,
            ..Book::default()
        });
        let fake = Fake::new(vec![Ok(reply(200, "{\"received\": true}"))]);
        let sender = wired(off.clone(), fake.clone());
        assert!(sender.send_if_due().await.unwrap().is_none());
        assert_eq!(fake.calls(), 0);
        assert!(off.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn the_week_is_the_last_whole_one_monday_to_sunday() {
        let wednesday = Date::parse("2026-10-14").unwrap();
        let (start, end) = last_week(wednesday);
        assert_eq!(
            (start.to_string(), end.to_string()),
            ("2026-10-05".into(), "2026-10-11".into())
        );
        let monday = Date::parse("2026-10-12").unwrap();
        assert_eq!(last_week(monday), (start, end));
        let sunday = Date::parse("2026-10-11").unwrap();
        let (start, end) = last_week(sunday);
        assert_eq!(
            (start.to_string(), end.to_string()),
            ("2026-09-28".into(), "2026-10-04".into())
        );
    }

    #[tokio::test]
    async fn the_report_carries_the_schemes_the_deployment_admin_declares() {
        // Contract v25 (W3.28).
        let book = Arc::new(Book {
            licensed: vec!["cusip".into(), "sedol".into()],
            ..Default::default()
        });
        let fake = Fake::new(vec![Ok(reply(200, "{\"received\": true}"))]);
        let sender = wired(book, fake.clone());
        let sent = sender.send_if_due().await.unwrap().expect("due");
        assert_eq!(
            sent.report.unwrap().licensed_schemes,
            vec!["cusip", "sedol"]
        );
        let body: serde_json::Value = serde_json::from_str(&fake.bodies()[0]).unwrap();
        assert_eq!(
            body["licensed_schemes"],
            serde_json::json!(["cusip", "sedol"])
        );
    }
}
