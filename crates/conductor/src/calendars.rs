//! The settlement calendars and date rules a deployment needs, pulled from
//! the platform (contract v22; W3.16; spec/settlement-calendars-are-core-
//! reference-data, requirement 11 and Q5).
//!
//! Pulled with what needs it, never the whole set, as venues are: a
//! currency's calendar when its cash instrument is pulled, a venue's when
//! the venue is; a calendar, a pair's spot-date rule or a market's
//! settlement-date rule when the instrument store reports one missing
//! (W3.18). Each held subject is asked again daily -- the store names them
//! -- for the versions after the highest this conductor has seen, and an
//! ask the platform did not answer is asked again once it answers another.
//! What the platform answered goes to the instrument store on the bus as it
//! came (`platform.reference.event.calendar-pulled`); this keeps no store.
//! A query carries only a calendar's subject, never a holding.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meridian_bus::Bus;
use meridian_domain::v1::{
    date_rule_version, AssetClass, BusinessDayConvention, CalendarCoverage, CalendarDay,
    CalendarDayKind, CalendarKind, CalendarRecord, CalendarSource, CalendarWeekend,
    DateRuleVersion, DayCut, Identifier, InstrumentLifecycleState, MissingCalendarDetectedEvent,
    PullCalendarReply, SettlementDateRule, SettlementSystem, SpotDateRule, TenorDateRule, Weekday,
};
use prost::Message;
use serde_json::Value;

use crate::platform::{encode, Platform, PlatformError};

/// W3.18, heard: a calendar or a rule the instrument store does not hold.
pub const CALENDAR_MISSING: &str = "platform.reference.event.calendar-missing";

/// W3.16: what the platform answered, for the instrument store to keep.
pub const CALENDAR_PULLED: &str = "platform.reference.event.calendar-pulled";

/// How long one subject is not asked about again: a read reports a miss
/// each time it meets it, and the platform needs asking once.
const ASKED_AGAIN_AFTER: Duration = Duration::from_secs(600);

/// How long one pull may take.
const PULLING: Duration = Duration::from_secs(8);

/// The one subject a pull names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subject {
    Calendar(String),
    /// A currency's calendar, by its cash instrument.
    Currency(String),
    /// A venue's trading calendar.
    Venue(String),
    /// A pair's spot-date rule and the calendars it names.
    Pair(String, String),
    /// A market's settlement-date rule and the calendars it names.
    Market(String, i32),
}

impl Subject {
    /// From a miss the store reported; none for one naming nothing.
    pub fn of(event: &MissingCalendarDetectedEvent) -> Option<Subject> {
        let subject = event.subject.as_ref()?;
        if !subject.calendar_id.is_empty() {
            return Some(Subject::Calendar(subject.calendar_id.clone()));
        }
        if let [a, b] = &subject.pair[..] {
            // Either order names one pair.
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            return Some(Subject::Pair(a.clone(), b.clone()));
        }
        (!subject.country_code.is_empty() && subject.asset_class != 0)
            .then(|| Subject::Market(subject.country_code.clone(), subject.asset_class))
    }

    /// The query string the platform's route reads.
    pub fn query(&self, after_version: Option<i64>) -> String {
        let mut query = match self {
            Subject::Calendar(id) => format!("calendar_id={}", encode(id)),
            Subject::Currency(id) => format!("instrument_id={}", encode(id)),
            Subject::Venue(id) => format!("venue_id={}", encode(id)),
            Subject::Pair(a, b) => format!(
                "kind=DATE_RULE_KIND_SPOT&pair={}&pair={}",
                encode(a),
                encode(b)
            ),
            Subject::Market(country, asset_class) => format!(
                "kind=DATE_RULE_KIND_SETTLEMENT&country_code={}&asset_class={}",
                encode(country),
                AssetClass::try_from(*asset_class)
                    .map(|class| class.as_str_name())
                    .unwrap_or("ASSET_CLASS_UNSPECIFIED")
            ),
        };
        if let Some(after) = after_version.filter(|after| *after > 0) {
            query.push_str(&format!("&after_version={after}"));
        }
        query
    }
}

/// Asks the platform for the calendars and rules the deployment needs, and
/// hands what came back to the instrument store.
pub struct CalendarAsker {
    bus: Arc<Bus>,
    platform: Arc<Platform>,
    clock: Arc<dyn meridian_clock::Clock>,
    /// The highest version seen of the record each subject names: what a
    /// refresh asks after.
    highest: Mutex<BTreeMap<Subject, i64>>,
    /// When each subject was last asked, so a miss reported again and again
    /// is asked once.
    asked: Mutex<BTreeMap<Subject, i64>>,
    /// Subjects the platform did not answer, asked again once it answers.
    unanswered: Mutex<BTreeSet<Subject>>,
}

impl CalendarAsker {
    pub fn new(
        bus: Arc<Bus>,
        platform: Arc<Platform>,
        clock: Arc<dyn meridian_clock::Clock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            bus,
            platform,
            clock,
            highest: Mutex::new(BTreeMap::new()),
            asked: Mutex::new(BTreeMap::new()),
            unanswered: Mutex::new(BTreeSet::new()),
        })
    }

    /// Subscribe before returning; the loop that asks is what is left.
    pub fn start(self: Arc<Self>) -> impl std::future::Future<Output = ()> {
        let mut missing = self.bus.subscribe(CALENDAR_MISSING);
        async move {
            while let Some(delivery) = missing.recv().await {
                let Ok(event) =
                    MissingCalendarDetectedEvent::decode(&delivery.envelope.payload[..])
                else {
                    tracing::warn!("a calendar reported missing did not decode");
                    continue;
                };
                let Some(subject) = Subject::of(&event) else {
                    tracing::warn!("a calendar reported missing named no subject");
                    continue;
                };
                let meta = delivery.envelope.meta.unwrap_or_default();
                self.pull(subject, &meta.correlation_id, &meta.message_id)
                    .await;
            }
        }
    }

    /// Ask once for `subject`, unless it was asked within the last ten
    /// minutes, and publish what came back; true when the platform answered.
    pub async fn pull(&self, subject: Subject, correlation: &str, causation: &str) -> bool {
        let now = self.clock.now_ns();
        {
            let mut asked = self.asked.lock().expect("asked lock poisoned");
            asked.retain(|_, at| now - *at < ASKED_AGAIN_AFTER.as_nanos() as i64);
            if asked.contains_key(&subject) {
                return true;
            }
            asked.insert(subject.clone(), now);
        }
        let answered = self.ask(&subject, correlation, causation, now).await;
        if answered {
            // Reconnected: what the platform did not answer before, again.
            let again: Vec<Subject> =
                std::mem::take(&mut *self.unanswered.lock().expect("unanswered lock poisoned"))
                    .into_iter()
                    .collect();
            for subject in again {
                Box::pin(self.ask(&subject, correlation, causation, now)).await;
            }
        }
        answered
    }

    async fn ask(&self, subject: &Subject, correlation: &str, causation: &str, now: i64) -> bool {
        let after = self
            .highest
            .lock()
            .expect("highest lock poisoned")
            .get(subject)
            .copied();
        let pulled = tokio::time::timeout(
            PULLING,
            self.platform.pull_calendars(&subject.query(after), now),
        )
        .await;
        let answer = match pulled {
            Err(_) => Err("the platform did not answer in time".to_string()),
            Ok(Err(PlatformError::Refused { status })) if status < 500 => {
                tracing::warn!(?subject, status, "the platform refused a calendar's pull");
                return true;
            }
            Ok(Err(failed)) => Err(failed.to_string()),
            Ok(Ok(answer)) => Ok(answer),
        };
        let answer = match answer {
            Err(failed) => {
                tracing::warn!(
                    ?subject,
                    failed,
                    "a calendar's pull failed; asked again later"
                );
                self.unanswered
                    .lock()
                    .expect("unanswered lock poisoned")
                    .insert(subject.clone());
                return false;
            }
            Ok(None) => {
                tracing::info!(
                    ?subject,
                    "the platform holds no calendar or rule for this yet"
                );
                return true;
            }
            Ok(Some(answer)) => answer,
        };
        let reply = match reply_of(&answer) {
            Ok(reply) => reply,
            Err(why) => {
                tracing::warn!(
                    ?subject,
                    why,
                    "a calendar's pull did not read; nothing kept"
                );
                return true;
            }
        };
        if reply.calendars.is_empty() && reply.rules.is_empty() {
            return true;
        }
        let seen = match subject {
            Subject::Calendar(id) => reply
                .calendars
                .iter()
                .filter(|record| &record.calendar_id == id)
                .map(|record| record.version)
                .max(),
            Subject::Pair(..) | Subject::Market(..) => {
                reply.rules.iter().map(|rule| rule.version).max()
            }
            Subject::Currency(_) | Subject::Venue(_) => None,
        };
        if let Some(seen) = seen {
            let mut highest = self.highest.lock().expect("highest lock poisoned");
            let held = highest.entry(subject.clone()).or_insert(0);
            *held = (*held).max(seen);
        }
        if let Err(failed) = self.bus.publish(
            CALENDAR_PULLED,
            "meridian.v1.PullCalendarReply",
            reply.encode_to_vec(),
            Some(correlation).filter(|c| !c.is_empty()),
            Some(causation).filter(|c| !c.is_empty()),
        ) {
            tracing::warn!(%failed, "a pulled calendar could not be published");
        }
        true
    }
}

// ── The platform's answer, read ─────────────────────────────────────────────

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A 64-bit number as the platform writes it: a number, or proto3 JSON's
/// string.
fn number(value: &Value, key: &str) -> Result<i64, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| format!("{key} is not a whole number")),
        Some(Value::String(s)) => s
            .parse()
            .map_err(|_| format!("{key} {s:?} is not a number")),
        Some(_) => Err(format!("{key} is not a number")),
    }
}

fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    list(value, key)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// An enum by its name, as proto3 JSON writes it, or by its number.
fn named(value: &Value, key: &str, read: impl Fn(&str) -> Option<i32>) -> Result<i32, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(Value::String(name)) => {
            read(name).ok_or_else(|| format!("{key} {name:?} is not a value the schema defines"))
        }
        Some(Value::Number(n)) => n
            .as_i64()
            .map(|n| n as i32)
            .ok_or_else(|| format!("{key} is not a whole number")),
        Some(_) => Err(format!("{key} is not an enum's name")),
    }
}

fn sources(value: &Value) -> Result<Vec<CalendarSource>, String> {
    list(value, "sources")
        .iter()
        .map(|source| {
            Ok(CalendarSource {
                source_id: text(source, "source_id"),
                name: text(source, "name"),
                address: text(source, "address"),
                read_at_ns: number(source, "read_at_ns")?,
            })
        })
        .collect()
}

fn calendar(value: &Value) -> Result<CalendarRecord, String> {
    let calendar_id = text(value, "calendar_id");
    if !calendar_id.starts_with("CAL-") {
        return Err(format!(
            "a calendar arrived with ID {calendar_id:?}, not the platform's"
        ));
    }
    let weekends = list(value, "weekends")
        .iter()
        .map(|weekend| {
            Ok(CalendarWeekend {
                weekdays: list(weekend, "weekdays")
                    .iter()
                    .map(|day| match day {
                        Value::String(name) => Weekday::from_str_name(name)
                            .map(|day| day as i32)
                            .ok_or_else(|| format!("weekday {name:?} is not one")),
                        Value::Number(n) => Ok(n.as_i64().unwrap_or(0) as i32),
                        _ => Err("a weekday is not a name".to_string()),
                    })
                    .collect::<Result<Vec<i32>, String>>()?,
                valid_from_date: text(weekend, "valid_from_date"),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let days = list(value, "days")
        .iter()
        .map(|day| {
            Ok(CalendarDay {
                date: text(day, "date"),
                kind: named(day, "kind", |n| {
                    CalendarDayKind::from_str_name(n).map(|k| k as i32)
                })?,
                name: text(day, "name"),
                source: text(day, "source"),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let coverage = value.get("coverage").map(|coverage| CalendarCoverage {
        from_date: text(coverage, "from_date"),
        to_date: text(coverage, "to_date"),
    });
    let identifiers = list(value, "identifiers")
        .iter()
        .map(|identifier| Identifier {
            scheme: text(identifier, "scheme"),
            value: text(identifier, "value"),
            source: text(identifier, "source"),
            ..Default::default()
        })
        .collect();
    Ok(CalendarRecord {
        calendar_id,
        name: text(value, "name"),
        kind: named(value, "kind", |n| {
            CalendarKind::from_str_name(n).map(|k| k as i32)
        })?,
        instrument_id: text(value, "instrument_id"),
        venue_ids: strings(value, "venue_ids"),
        settlement_system: value
            .get("settlement_system")
            .map(|system| SettlementSystem {
                name: text(system, "name"),
                country_code: text(system, "country_code"),
            }),
        identifiers,
        weekends,
        days,
        coverage,
        sources: sources(value)?,
        version: number(value, "version")?,
        valid_time_ns: number(value, "valid_time_ns")?,
        record_time_ns: number(value, "record_time_ns")?,
        note: text(value, "note"),
        lifecycle_state: named(value, "lifecycle_state", |n| {
            InstrumentLifecycleState::from_str_name(n).map(|s| s as i32)
        })?,
    })
}

fn rule(value: &Value) -> Result<DateRuleVersion, String> {
    let parameters = if let Some(settlement) = value.get("settlement") {
        Some(date_rule_version::Parameters::Settlement(
            SettlementDateRule {
                country_code: text(settlement, "country_code"),
                asset_class: named(settlement, "asset_class", |n| {
                    AssetClass::from_str_name(n).map(|c| c as i32)
                })?,
                venue_id: text(settlement, "venue_id"),
                lag_days: number(settlement, "lag_days")? as i32,
                counting_calendars: strings(settlement, "counting_calendars"),
                effective_from_date: text(settlement, "effective_from_date"),
            },
        ))
    } else if let Some(spot) = value.get("spot") {
        Some(date_rule_version::Parameters::Spot(SpotDateRule {
            pair: strings(spot, "pair"),
            lag_days: number(spot, "lag_days")? as i32,
            counting_calendars: strings(spot, "counting_calendars"),
            spot_calendars: strings(spot, "spot_calendars"),
            cut: spot.get("cut").map(|cut| DayCut {
                local_time: text(cut, "local_time"),
                time_zone: text(cut, "time_zone"),
            }),
            tenor_rule: spot
                .get("tenor_rule")
                .map(|tenor| -> Result<TenorDateRule, String> {
                    Ok(TenorDateRule {
                        convention: named(tenor, "convention", |n| {
                            BusinessDayConvention::from_str_name(n).map(|c| c as i32)
                        })?,
                        end_of_month: tenor
                            .get("end_of_month")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .transpose()?,
            effective_from_date: text(spot, "effective_from_date"),
        }))
    } else {
        None
    };
    Ok(DateRuleVersion {
        kind: named(value, "kind", rule_kind)?,
        subject: text(value, "subject"),
        version: number(value, "version")?,
        parameters,
        sources: sources(value)?,
        note: text(value, "note"),
        record_time_ns: number(value, "record_time_ns")?,
    })
}

/// A rule's kind by its name (meridian.v1.DateRuleKind, which the schema's
/// crate carries and this one does not link).
fn rule_kind(name: &str) -> Option<i32> {
    match name {
        "DATE_RULE_KIND_UNSPECIFIED" => Some(0),
        "DATE_RULE_KIND_SETTLEMENT" => Some(1),
        "DATE_RULE_KIND_SPOT" => Some(2),
        _ => None,
    }
}

/// The platform's answer as the bus carries it, or why it does not read.
pub fn reply_of(answer: &Value) -> Result<PullCalendarReply, String> {
    Ok(PullCalendarReply {
        calendars: list(answer, "calendars")
            .iter()
            .map(calendar)
            .collect::<Result<_, _>>()?,
        rules: list(answer, "rules")
            .iter()
            .map(rule)
            .collect::<Result<_, _>>()?,
    })
}

#[cfg(test)]
mod tests {
    use meridian_bus::MemoryBackend;
    use serde_json::json;

    use super::*;
    use crate::platform::tests::{platform as platform_with, reply, Fake};

    const NOW: i64 = 1_760_400_000_000_000_000;

    fn answer() -> Value {
        json!({
            "calendars": [{
                "calendar_id": "CAL-01JAE0000000000000000USSS",
                "name": "US securities settlement",
                "kind": "CALENDAR_KIND_SECURITIES_SETTLEMENT",
                "settlement_system": {"name": "DTC", "country_code": "US"},
                "weekends": [{"weekdays": ["WEEKDAY_SATURDAY", "WEEKDAY_SUNDAY"],
                              "valid_from_date": "2016-01-01"}],
                "days": [{"date": "2026-10-12", "kind": "CALENDAR_DAY_KIND_CLOSED",
                          "name": "Columbus Day", "source": "fed-holidays"}],
                "coverage": {"from_date": "2016-01-01", "to_date": "2028-12-31"},
                "sources": [{"source_id": "fed-holidays", "name": "Federal Reserve holidays",
                             "address": "https://www.federalreserve.gov/aboutthefed/k8.htm",
                             "read_at_ns": "1760140800000000000"}],
                "version": "4",
                "lifecycle_state": "INSTRUMENT_LIFECYCLE_STATE_ACTIVE"
            }],
            "rules": [{
                "kind": "DATE_RULE_KIND_SETTLEMENT", "subject": "US/equity", "version": 2,
                "settlement": {"country_code": "US", "asset_class": "ASSET_CLASS_EQUITY",
                               "lag_days": 1,
                               "counting_calendars": ["CAL-01JAE0000000000000000USSS"],
                               "effective_from_date": "2024-05-28"}
            }]
        })
    }

    #[test]
    fn the_platforms_answer_reads_as_the_fixture_has_it() {
        let reply = reply_of(&answer()).unwrap();
        let calendar = &reply.calendars[0];
        assert_eq!(calendar.version, 4);
        assert_eq!(calendar.kind, CalendarKind::SecuritiesSettlement as i32);
        assert_eq!(calendar.days[0].kind, CalendarDayKind::Closed as i32);
        assert_eq!(calendar.sources[0].read_at_ns, 1_760_140_800_000_000_000);
        assert_eq!(calendar.weekends[0].weekdays.len(), 2);
        let Some(date_rule_version::Parameters::Settlement(rule)) = &reply.rules[0].parameters
        else {
            panic!("a settlement rule");
        };
        assert_eq!(rule.asset_class, AssetClass::Equity as i32);
        assert_eq!(rule.lag_days, 1);
        assert!(reply_of(&json!({"calendars": [{"calendar_id": "LCL-1"}]})).is_err());
        assert!(reply_of(
            &json!({"calendars": [{"calendar_id": "CAL-1", "kind": "CALENDAR_KIND_MOON"}]})
        )
        .is_err());
    }

    #[test]
    fn a_query_names_one_subject_and_never_a_holding() {
        let market = MissingCalendarDetectedEvent {
            subject: Some(meridian_domain::v1::CalendarSubject {
                country_code: "US".into(),
                asset_class: AssetClass::Equity as i32,
                ..Default::default()
            }),
            kind: 1,
            observed_at_ns: NOW,
        };
        assert_eq!(
            Subject::of(&market).unwrap().query(Some(2)),
            "kind=DATE_RULE_KIND_SETTLEMENT&country_code=US&asset_class=ASSET_CLASS_EQUITY&after_version=2"
        );
        let pair = MissingCalendarDetectedEvent {
            subject: Some(meridian_domain::v1::CalendarSubject {
                pair: vec!["INS-B".into(), "INS-A".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            Subject::of(&pair).unwrap(),
            Subject::Pair("INS-A".into(), "INS-B".into()),
            "either order names one pair"
        );
        assert!(Subject::of(&MissingCalendarDetectedEvent::default()).is_none());
    }

    #[tokio::test]
    async fn a_miss_is_pulled_once_and_handed_on_and_a_refresh_asks_after_the_highest() {
        let bus = Arc::new(Bus::single(
            "conductor-1",
            Arc::new(MemoryBackend::new()),
            Arc::new(meridian_clock::ManualClock::at(NOW)),
        ));
        let fake = Fake::new(vec![Ok(reply(200, &answer().to_string()))]);
        let clock = Arc::new(meridian_clock::ManualClock::at(NOW));
        let asker = CalendarAsker::new(
            Arc::clone(&bus),
            Arc::new(platform_with(fake.clone())),
            clock.clone(),
        );
        let mut pulled = bus.subscribe(CALENDAR_PULLED);
        let market = Subject::Market("US".into(), AssetClass::Equity as i32);
        assert!(asker.pull(market.clone(), "", "").await);
        let delivery = tokio::time::timeout(Duration::from_secs(2), pulled.recv())
            .await
            .unwrap()
            .unwrap();
        let handed = PullCalendarReply::decode(&delivery.envelope.payload[..]).unwrap();
        assert_eq!(handed.rules[0].subject, "US/equity");
        // Reported again within ten minutes: not asked again.
        asker.pull(market.clone(), "", "").await;
        assert_eq!(fake.calls(), 1);
        // A day on, the refresh asks only after the highest version seen.
        clock.advance(86_400_000_000_000);
        asker.pull(market, "", "").await;
        assert_eq!(fake.calls(), 2);
        assert!(
            fake.urls()[1].contains("after_version=2"),
            "{:?}",
            fake.urls()
        );
        assert!(
            fake.urls()[0].starts_with("https://")
                || fake.urls()[0].contains("/api/v1/reference/calendars?")
        );
    }
}
