//! The settlement calendars and date rules, and the one way a date is worked
//! out from them (contract v22; W3.16 to W3.21;
//! spec/settlement-calendars-are-core-reference-data).
//!
//! The platform keeps each calendar and rule, every version dated and each
//! day citing its source; the conductor pulls what the deployment needs; this
//! store keeps every version it receives and answers two reads: a calendar's
//! days over a range, and a date. It mints and edits nothing.
//!
//! **One way, in the kernel's own code.** A set of calendars combines one
//! way: a day is good only if it is good in every one, and a day a calendar
//! does not cover is not known -- never a business day by default, and
//! nothing is computed from it. The same inputs and the same versions always
//! give the same date: the versions are those this store had kept by the
//! recorded time asked for, the latest held when none is, and the wall clock
//! is never read. Every answer names its basis -- the rule and its version,
//! each calendar and its version -- or why it has none: a calendar not held,
//! a rule not held (each reported missing by its subject, for the conductor
//! to pull), or a date outside a calendar's coverage. A malformed input is
//! refused, naming its field. Nothing here claims anything regulatory: a
//! computed date is what a named rule gives on named calendars.

use std::collections::BTreeMap;

use meridian_domain::date::Date;
use meridian_domain::v1::{
    compute_date_request, date_rule_version, AssetClass, BusinessDayConvention, CalendarDay,
    CalendarDayKind, CalendarRecord, CalendarSubject, ComputeDateReply, ComputeDateRequest,
    DateOutcome, DateRuleVersion, HeldCalendar, HeldDateRule, ListCalendarsReply,
    ListCalendarsRequest, MissingCalendarDetectedEvent, ReadCalendarReply, ReadCalendarRequest,
    SettlementDateRule, SpotCountingMode, SpotDateRule, TenorUnit, Weekday,
};
use meridian_pb::v1::{CalendarRef, DateBasis, DateRuleKind, DateRuleRef};

use crate::store::{KeptCalendar, KeptDateRule};

/// The most days a count or a roll walks before it says the calendars do
/// not cover the way: ten years, more than any coverage a source publishes.
const WALK_MOST: i64 = 3_660;

/// What the store holds, as of a recorded time: the versions it had kept by
/// then, the latest held when the time is zero.
pub struct Held {
    calendars: BTreeMap<String, CalendarRecord>,
    rules: Vec<(DateRuleVersion, i64)>,
}

impl Held {
    pub fn as_of(calendars: &[KeptCalendar], rules: &[KeptDateRule], as_of_ns: i64) -> Held {
        let known = |kept_at: i64| as_of_ns == 0 || kept_at <= as_of_ns;
        let mut latest: BTreeMap<String, CalendarRecord> = BTreeMap::new();
        for kept in calendars.iter().filter(|kept| known(kept.kept_at_ns)) {
            let id = &kept.record.calendar_id;
            if latest
                .get(id)
                .is_none_or(|held| held.version < kept.record.version)
            {
                latest.insert(id.clone(), kept.record.clone());
            }
        }
        Held {
            calendars: latest,
            rules: rules
                .iter()
                .filter(|kept| known(kept.kept_at_ns))
                .map(|kept| (kept.rule.clone(), kept.kept_at_ns))
                .collect(),
        }
    }

    /// A rule's version in force for `date`: of the versions held of each
    /// rule that `matches`, the highest whose date of effect is on or before
    /// it, so a date before a version is answered by the one before (T+2
    /// before 28 May 2024, T+1 after).
    fn rule(
        &self,
        kind: DateRuleKind,
        date: Option<Date>,
        matches: impl Fn(&DateRuleVersion) -> bool,
    ) -> Option<&DateRuleVersion> {
        self.rules
            .iter()
            .map(|(rule, _)| rule)
            .filter(|rule| rule.kind == kind as i32 && matches(rule))
            .filter(|rule| match (date, effective_from(rule)) {
                (Some(date), Some(from)) => from <= date,
                _ => true,
            })
            .max_by_key(|rule| rule.version)
    }
}

fn effective_from(rule: &DateRuleVersion) -> Option<Date> {
    let text = match &rule.parameters {
        Some(date_rule_version::Parameters::Settlement(held)) => &held.effective_from_date,
        Some(date_rule_version::Parameters::Spot(held)) => &held.effective_from_date,
        None => return None,
    };
    Date::parse(text).ok()
}

/// Why a date could not be worked out.
enum Stop {
    /// A calendar named that the store does not hold.
    NotHeld(String),
    /// A day a calendar does not cover was needed.
    Outside,
}

/// A day's standing on a set of calendars.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    Good,
    Closed,
}

/// The calendars a date is worked on, with the basis they give.
struct Set<'a> {
    calendars: Vec<&'a CalendarRecord>,
}

impl<'a> Set<'a> {
    fn of(held: &'a Held, ids: &[String]) -> Result<Set<'a>, Stop> {
        let mut calendars = Vec::new();
        for id in ids {
            let record = held
                .calendars
                .get(id)
                .ok_or_else(|| Stop::NotHeld(id.clone()))?;
            if !calendars
                .iter()
                .any(|held: &&CalendarRecord| held.calendar_id == *id)
            {
                calendars.push(record);
            }
        }
        Ok(Set { calendars })
    }

    /// Good only if good in every calendar; not known if any does not cover
    /// it.
    fn standing(&self, date: Date) -> Result<Standing, Stop> {
        let mut standing = Standing::Good;
        for calendar in &self.calendars {
            if day_on(calendar, date)? == Standing::Closed {
                standing = Standing::Closed;
            }
        }
        Ok(standing)
    }

    fn refs(&self) -> Vec<CalendarRef> {
        self.calendars
            .iter()
            .map(|calendar| CalendarRef {
                calendar_id: calendar.calendar_id.clone(),
                version: calendar.version,
            })
            .collect()
    }
}

/// One calendar's word on a day: closed or open as listed, else by its
/// weekend in force then; outside its coverage, or before any weekend it
/// dates, not known.
fn day_on(calendar: &CalendarRecord, date: Date) -> Result<Standing, Stop> {
    let coverage = calendar.coverage.as_ref().ok_or(Stop::Outside)?;
    let (Ok(from), Ok(to)) = (
        Date::parse(&coverage.from_date),
        Date::parse(&coverage.to_date),
    ) else {
        return Err(Stop::Outside);
    };
    if date < from || date > to {
        return Err(Stop::Outside);
    }
    let text = date.to_string();
    if let Some(day) = calendar.days.iter().find(|day| day.date == text) {
        if day.kind == CalendarDayKind::Closed as i32 {
            return Ok(Standing::Closed);
        }
        if day.kind == CalendarDayKind::Open as i32 {
            return Ok(Standing::Good);
        }
    }
    let weekend = calendar
        .weekends
        .iter()
        .filter_map(|weekend| {
            Date::parse(&weekend.valid_from_date)
                .ok()
                .filter(|from| *from <= date)
                .map(|from| (from, weekend))
        })
        .max_by_key(|(from, _)| *from)
        .map(|(_, weekend)| weekend)
        .ok_or(Stop::Outside)?;
    if weekend.weekdays.contains(&(weekday(date) as i32)) {
        Ok(Standing::Closed)
    } else {
        Ok(Standing::Good)
    }
}

fn weekday(date: Date) -> Weekday {
    // 1970-01-01 was a Thursday.
    match (date.days_since_epoch() + 3).rem_euclid(7) {
        0 => Weekday::Monday,
        1 => Weekday::Tuesday,
        2 => Weekday::Wednesday,
        3 => Weekday::Thursday,
        4 => Weekday::Friday,
        5 => Weekday::Saturday,
        _ => Weekday::Sunday,
    }
}

fn shift(date: Date, days: i64) -> Result<Date, Stop> {
    Date::from_days(date.days_since_epoch() + days).ok_or(Stop::Outside)
}

/// `count` business days after `from` on `set`, each day counted only if
/// good there; `from` itself, rolled forward, for none.
fn count_forward(set: &Set, from: Date, count: i64) -> Result<Date, Stop> {
    if count <= 0 {
        return roll(set, from, BusinessDayConvention::Following);
    }
    let mut date = from;
    let mut counted = 0;
    for _ in 0..WALK_MOST {
        date = shift(date, 1)?;
        if set.standing(date)? == Standing::Good {
            counted += 1;
            if counted == count {
                return Ok(date);
            }
        }
    }
    Err(Stop::Outside)
}

fn step(set: &Set, from: Date, direction: i64) -> Result<Date, Stop> {
    let mut date = from;
    for _ in 0..WALK_MOST {
        if set.standing(date)? == Standing::Good {
            return Ok(date);
        }
        date = shift(date, direction)?;
    }
    Err(Stop::Outside)
}

/// A date rolled by a convention on a set of calendars.
fn roll(set: &Set, date: Date, convention: BusinessDayConvention) -> Result<Date, Stop> {
    match convention {
        BusinessDayConvention::Following | BusinessDayConvention::Unspecified => step(set, date, 1),
        BusinessDayConvention::Preceding => step(set, date, -1),
        BusinessDayConvention::ModifiedFollowing => {
            let following = step(set, date, 1)?;
            if following.month() == date.month() {
                Ok(following)
            } else {
                step(set, date, -1)
            }
        }
        BusinessDayConvention::None => {
            set.standing(date)?;
            Ok(date)
        }
    }
}

fn days_in_month(year: u16, month: u8) -> u8 {
    (28..=31)
        .rev()
        .find(|day| Date::from_ymd(year, month, *day).is_some())
        .unwrap_or(28)
}

/// `months` months after `date`, its day kept where the month has it and
/// the month's last day otherwise.
fn add_months(date: Date, months: i64) -> Result<Date, Stop> {
    let index = i64::from(date.year()) * 12 + i64::from(date.month()) - 1 + months;
    let year = u16::try_from(index.div_euclid(12)).map_err(|_| Stop::Outside)?;
    let month = (index.rem_euclid(12) + 1) as u8;
    let day = date.day().min(days_in_month(year, month));
    Date::from_ymd(year, month, day).ok_or(Stop::Outside)
}

/// The last business day of `date`'s month on `set`.
fn month_end(set: &Set, date: Date) -> Result<Date, Stop> {
    let last = Date::from_ymd(
        date.year(),
        date.month(),
        days_in_month(date.year(), date.month()),
    )
    .ok_or(Stop::Outside)?;
    step(set, last, -1)
}

/// A spot or tenor read's pair, each member followed through its
/// replacement (W3.20; the one-ID spec's requirement 19, one rule): a spot
/// rule pulled from the platform names its `INS-` IDs, and so does the
/// deployment's record, a resolved `LCL-` ID read as the `INS-` record it
/// became (`became`). A member that became none is kept as asked, so the
/// rule is not held and the miss names it honestly, never a guess. From
/// contract v25 nothing is read from an `open_meridian` identifier: v23's
/// translation, kept read only through v24 for a record not yet migrated,
/// is gone, and such a record misses until its migration line is made
/// (W3.26).
pub fn pair_followed(
    request: &ComputeDateRequest,
    became: impl Fn(&str) -> Option<String>,
) -> ComputeDateRequest {
    use compute_date_request::Inputs;
    let mut request = request.clone();
    let pair = match request.inputs.as_mut() {
        Some(Inputs::Spot(asked)) => &mut asked.pair,
        Some(Inputs::Tenor(asked)) => &mut asked.pair,
        _ => return request,
    };
    for member in pair.iter_mut() {
        if let Some(platform) = became(member.trim()) {
            *member = platform;
        }
    }
    request
}

/// A pair, its two cash instruments in either order, as a rule's subject
/// names it.
fn same_pair(rule: &[String], asked: &[String]) -> bool {
    asked.len() == 2
        && rule.len() == 2
        && ((rule[0] == asked[0] && rule[1] == asked[1])
            || (rule[0] == asked[1] && rule[1] == asked[0]))
}

/// An asset class as a rule's subject spells it: its enum name, less its
/// prefix, in lower case (`equity`).
pub fn asset_class_word(asset_class: i32) -> String {
    AssetClass::try_from(asset_class)
        .map(|class| {
            class
                .as_str_name()
                .trim_start_matches("ASSET_CLASS_")
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

/// What a date read needs of an instrument the store holds: its asset class,
/// and its market's country and venue from its listing venue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Market {
    pub asset_class: i32,
    pub country_code: String,
    pub venue_id: String,
}

/// What a date read answers, and the miss it reports, if any.
pub struct Answered {
    pub reply: ComputeDateReply,
    pub missing: Option<MissingCalendarDetectedEvent>,
}

fn answered(outcome: DateOutcome, basis: DateBasis) -> Answered {
    Answered {
        reply: ComputeDateReply {
            date: String::new(),
            outcome: outcome as i32,
            basis: Some(basis),
            business_day: false,
        },
        missing: None,
    }
}

fn stopped(stop: Stop, basis: DateBasis, now_ns: i64) -> Answered {
    match stop {
        Stop::Outside => answered(DateOutcome::OutsideCoverage, basis),
        Stop::NotHeld(calendar_id) => Answered {
            missing: Some(MissingCalendarDetectedEvent {
                subject: Some(CalendarSubject {
                    calendar_id,
                    ..Default::default()
                }),
                kind: DateRuleKind::Unspecified as i32,
                observed_at_ns: now_ns,
            }),
            ..answered(DateOutcome::CalendarNotHeld, basis)
        },
    }
}

fn rule_not_held(subject: CalendarSubject, kind: DateRuleKind, now_ns: i64) -> Answered {
    Answered {
        missing: Some(MissingCalendarDetectedEvent {
            subject: Some(subject),
            kind: kind as i32,
            observed_at_ns: now_ns,
        }),
        ..answered(DateOutcome::RuleNotHeld, DateBasis::default())
    }
}

fn computed(date: Date, basis: DateBasis) -> Answered {
    Answered {
        reply: ComputeDateReply {
            date: date.to_string(),
            outcome: DateOutcome::Computed as i32,
            basis: Some(basis),
            business_day: false,
        },
        missing: None,
    }
}

fn rule_ref(rule: &DateRuleVersion) -> DateRuleRef {
    DateRuleRef {
        kind: rule.kind,
        subject: rule.subject.clone(),
        version: rule.version,
    }
}

fn date_field(field: &str, text: &str) -> Result<Date, String> {
    if text.is_empty() {
        return Err(format!("{field}: is required, a date YYYY-MM-DD"));
    }
    Date::parse(text).map_err(|_| format!("{field}: {text:?} is not a date that exists"))
}

fn pair_field(field: &str, pair: &[String]) -> Result<(), String> {
    if pair.len() != 2 || pair.iter().any(|side| side.trim().is_empty()) || pair[0] == pair[1] {
        return Err(format!(
            "{field}: names a pair by its two cash instruments' IDs, two and different"
        ));
    }
    Ok(())
}

fn calendars_field(field: &str, calendars: &[String]) -> Result<(), String> {
    if calendars.is_empty() || calendars.iter().any(|id| id.trim().is_empty()) {
        return Err(format!("{field}: names at least one calendar by its ID"));
    }
    Ok(())
}

/// The trade date of an execution at `at_ns`: its date in the cut's zone,
/// and the next day at or after the cut (the calendars spec's Q7), then
/// rolled forward past the pair's weekend days (contract v25; decision 65,
/// item 111): after Friday's cut the trade date is Monday, never Saturday.
/// Weekends only: a holiday does not move a trade date, the dealing day the
/// market keeps; the dates counted from it skip holidays.
fn trade_date_at(held: &Held, rule: &SpotDateRule, at_ns: i64) -> Result<Date, String> {
    let cut = rule
        .cut
        .as_ref()
        .filter(|cut| !cut.time_zone.is_empty())
        .ok_or_else(|| {
            format!(
                "spot.at_ns: the spot-date rule for {} names no cut, so no moment is read as \
                 a trade date; give spot.trade_date",
                rule.pair.join("/")
            )
        })?;
    let (hours, minutes) = cut
        .local_time
        .split_once(':')
        .and_then(|(h, m)| Some((h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)))
        .filter(|(h, m)| (0..24).contains(h) && (0..60).contains(m))
        .ok_or_else(|| {
            format!(
                "spot.at_ns: the rule's cut {:?} does not read",
                cut.local_time
            )
        })?;
    let zone =
        crate::zone::Zone::load(&cut.time_zone).map_err(|why| format!("spot.at_ns: {why}"))?;
    let (date, seconds) = zone
        .local(at_ns)
        .ok_or_else(|| "spot.at_ns: is outside the dates a calendar names".to_string())?;
    let outside = || "spot.at_ns: is outside the dates a calendar names".to_string();
    let mut trade = if seconds >= hours * 3600 + minutes * 60 {
        Date::from_days(date.days_since_epoch() + 1).ok_or_else(outside)?
    } else {
        date
    };
    // The pair's weekend: the weekend days in force on each calendar the
    // rule counts or settles on, those held; none held, the date stands and
    // the spot's read names the calendar not held.
    for _ in 0..7 {
        if !pair_weekend(held, rule, trade) {
            break;
        }
        trade = Date::from_days(trade.days_since_epoch() + 1).ok_or_else(outside)?;
    }
    Ok(trade)
}

/// Whether `date` is a weekend day of the pair: on any of the rule's
/// counting or spot calendars held, by the weekend in force then, whatever
/// the calendar lists for the day itself.
fn pair_weekend(held: &Held, rule: &SpotDateRule, date: Date) -> bool {
    rule.counting_calendars
        .iter()
        .chain(rule.spot_calendars.iter())
        .filter_map(|id| held.calendars.get(id))
        .any(|calendar| {
            calendar
                .weekends
                .iter()
                .filter_map(|weekend| {
                    Date::parse(&weekend.valid_from_date)
                        .ok()
                        .filter(|from| *from <= date)
                        .map(|from| (from, weekend))
                })
                .max_by_key(|(from, _)| *from)
                .is_some_and(|(_, weekend)| weekend.weekdays.contains(&(weekday(date) as i32)))
        })
}

/// A pair's spot date from a trade date: counted by the lag on the counting
/// calendars, then rolled forward to a day good on the spot calendars. From
/// contract v23 the rule's counting mode says how the lag counts: jointly, a
/// day counting only if good on every counting calendar (unspecified, v22's
/// way), or each currency on its own calendars, the later of the two spots
/// rolled forward to a day good on both and on the spot calendars.
fn spot_from(
    held: &Held,
    rule: &SpotDateRule,
    trade: Date,
) -> Result<(Date, Vec<CalendarRef>), Stop> {
    let counting = Set::of(held, &rule.counting_calendars)?;
    let spot = Set::of(held, &rule.spot_calendars)?;
    let lag = i64::from(rule.lag_days);
    let date = match each_currency(rule, &counting, &spot, trade, lag)? {
        Some(date) => date,
        None => step(&spot, count_forward(&counting, trade, lag)?, 1)?,
    };
    let mut refs = counting.refs();
    for held in spot.refs() {
        if !refs.contains(&held) {
            refs.push(held);
        }
    }
    Ok((date, refs))
}

/// A cross's spot counted per currency (SPOT_COUNTING_MODE_EACH_CURRENCY):
/// each currency's spot on the counting calendars that are its own, or name
/// no currency, the later of them rolled forward to a day good on every
/// counting and spot calendar. None for a rule counting jointly, or one whose
/// counting calendars name neither currency, which counts jointly.
fn each_currency(
    rule: &SpotDateRule,
    counting: &Set,
    spot: &Set,
    trade: Date,
    lag: i64,
) -> Result<Option<Date>, Stop> {
    if rule.counting_mode != SpotCountingMode::EachCurrency as i32 {
        return Ok(None);
    }
    let mut later: Option<Date> = None;
    for currency in &rule.pair {
        if !counting
            .calendars
            .iter()
            .any(|calendar| calendar.instrument_id == *currency)
        {
            continue;
        }
        let own = Set {
            calendars: counting
                .calendars
                .iter()
                .filter(|calendar| {
                    calendar.instrument_id == *currency || calendar.instrument_id.is_empty()
                })
                .copied()
                .collect(),
        };
        let date = count_forward(&own, trade, lag)?;
        later = Some(later.map_or(date, |held| held.max(date)));
    }
    let Some(later) = later else {
        return Ok(None);
    };
    let mut both = Set {
        calendars: counting.calendars.clone(),
    };
    for calendar in &spot.calendars {
        if !both
            .calendars
            .iter()
            .any(|held| held.calendar_id == calendar.calendar_id)
        {
            both.calendars.push(calendar);
        }
    }
    step(&both, later, 1).map(Some)
}

/// W3.20: one way to work out a date. A malformed input is the refusal's
/// words, naming its field; anything else is answered, with its basis, or
/// with why there is none and the miss to report.
pub fn compute(
    held: &Held,
    request: &ComputeDateRequest,
    market: impl Fn(&str) -> Option<Market>,
    now_ns: i64,
) -> Result<Answered, String> {
    use compute_date_request::Inputs;
    match request.inputs.as_ref() {
        None => {
            Err("inputs: names one question: settlement, spot, tenor, roll or business_day".into())
        }
        Some(Inputs::BusinessDay(asked)) => {
            let date = date_field("business_day.date", &asked.date)?;
            calendars_field("business_day.calendars", &asked.calendars)?;
            let set = match Set::of(held, &asked.calendars) {
                Ok(set) => set,
                Err(stop) => return Ok(stopped(stop, DateBasis::default(), now_ns)),
            };
            let basis = DateBasis {
                rule: None,
                calendars: set.refs(),
            };
            Ok(match set.standing(date) {
                Ok(standing) => {
                    let mut answer = computed(date, basis);
                    answer.reply.business_day = standing == Standing::Good;
                    answer
                }
                Err(stop) => stopped(stop, basis, now_ns),
            })
        }
        Some(Inputs::Roll(asked)) => {
            let date = date_field("roll.date", &asked.date)?;
            calendars_field("roll.calendars", &asked.calendars)?;
            let convention = BusinessDayConvention::try_from(asked.convention)
                .ok()
                .filter(|c| *c != BusinessDayConvention::Unspecified)
                .ok_or("roll.convention: names how the date rolls")?;
            let set = match Set::of(held, &asked.calendars) {
                Ok(set) => set,
                Err(stop) => return Ok(stopped(stop, DateBasis::default(), now_ns)),
            };
            let basis = DateBasis {
                rule: None,
                calendars: set.refs(),
            };
            Ok(match roll(&set, date, convention) {
                Ok(rolled) => computed(rolled, basis),
                Err(stop) => stopped(stop, basis, now_ns),
            })
        }
        Some(Inputs::Settlement(asked)) => {
            if asked.instrument_id.trim().is_empty() {
                return Err("settlement.instrument_id: names the instrument".into());
            }
            let trade = date_field("settlement.trade_date", &asked.trade_date)?;
            let market = market(&asked.instrument_id).ok_or_else(|| {
                format!(
                    "settlement.instrument_id: {} is no record this deployment holds",
                    asked.instrument_id
                )
            })?;
            let subject = CalendarSubject {
                country_code: market.country_code.clone(),
                asset_class: market.asset_class,
                ..Default::default()
            };
            if market.country_code.is_empty() || market.asset_class == 0 {
                // Its market is not known -- no listing venue's country, or no
                // asset class: no rule can be named for it, and nothing is
                // guessed. Reported missing as a calendar is (W3.18; the
                // dictionary's "rule not held"), by what is known of it.
                return Ok(rule_not_held(subject, DateRuleKind::Settlement, now_ns));
            }
            let matches = |rule: &DateRuleVersion, venue: &str| match &rule.parameters {
                Some(date_rule_version::Parameters::Settlement(held)) => {
                    held.country_code == market.country_code
                        && held.asset_class == market.asset_class
                        && held.venue_id == venue
                }
                _ => false,
            };
            // A venue's override where it differs, else the market's rule.
            let rule = (!market.venue_id.is_empty())
                .then(|| {
                    held.rule(DateRuleKind::Settlement, Some(trade), |rule| {
                        matches(rule, &market.venue_id)
                    })
                })
                .flatten()
                .or_else(|| {
                    held.rule(DateRuleKind::Settlement, Some(trade), |rule| {
                        matches(rule, "")
                    })
                });
            let Some(rule) = rule else {
                return Ok(rule_not_held(subject, DateRuleKind::Settlement, now_ns));
            };
            let Some(date_rule_version::Parameters::Settlement(parameters)) = &rule.parameters
            else {
                return Ok(rule_not_held(subject, DateRuleKind::Settlement, now_ns));
            };
            Ok(settle(held, rule, parameters, trade, now_ns))
        }
        Some(Inputs::Spot(asked)) => {
            pair_field("spot.pair", &asked.pair)?;
            let given = (!asked.trade_date.is_empty())
                .then(|| date_field("spot.trade_date", &asked.trade_date))
                .transpose()?;
            if given.is_some() == (asked.at_ns != 0) {
                return Err(
                    "spot.trade_date: give a trade date or an execution's moment (spot.at_ns), \
                     one of the two"
                        .into(),
                );
            }
            let subject = CalendarSubject {
                pair: asked.pair.clone(),
                ..Default::default()
            };
            let pair = |rule: &DateRuleVersion| {
                matches!(&rule.parameters,
                    Some(date_rule_version::Parameters::Spot(held)) if same_pair(&held.pair, &asked.pair))
            };
            // With a moment, the rule in force on the moment's UTC date reads
            // its cut; the trade date through the cut then chooses again.
            let first =
                given.or_else(|| Date::from_days(asked.at_ns.div_euclid(86_400_000_000_000)));
            let Some(rule) = held.rule(DateRuleKind::Spot, first, pair) else {
                return Ok(rule_not_held(subject, DateRuleKind::Spot, now_ns));
            };
            let trade = match given {
                Some(trade) => trade,
                None => {
                    let Some(date_rule_version::Parameters::Spot(parameters)) = &rule.parameters
                    else {
                        return Ok(rule_not_held(subject, DateRuleKind::Spot, now_ns));
                    };
                    trade_date_at(held, parameters, asked.at_ns)?
                }
            };
            let Some(rule) = held.rule(DateRuleKind::Spot, Some(trade), pair) else {
                return Ok(rule_not_held(subject, DateRuleKind::Spot, now_ns));
            };
            let Some(date_rule_version::Parameters::Spot(parameters)) = &rule.parameters else {
                return Ok(rule_not_held(subject, DateRuleKind::Spot, now_ns));
            };
            let mut basis = DateBasis {
                rule: Some(rule_ref(rule)),
                calendars: Vec::new(),
            };
            Ok(match spot_from(held, parameters, trade) {
                Ok((date, calendars)) => {
                    basis.calendars = calendars;
                    computed(date, basis)
                }
                Err(stop) => stopped(stop, basis, now_ns),
            })
        }
        Some(Inputs::Tenor(asked)) => {
            pair_field("tenor.pair", &asked.pair)?;
            let trade = date_field("tenor.trade_date", &asked.trade_date)?;
            let tenor = asked.tenor.as_ref().ok_or("tenor.tenor: names the tenor")?;
            let unit = TenorUnit::try_from(tenor.unit)
                .ok()
                .filter(|unit| *unit != TenorUnit::Unspecified)
                .ok_or("tenor.tenor.unit: names the tenor's unit")?;
            let short = matches!(
                unit,
                TenorUnit::Overnight | TenorUnit::TomNext | TenorUnit::SpotNext
            );
            if (short && tenor.count > 1) || (!short && tenor.count < 1) {
                return Err(format!(
                    "tenor.tenor.count: is {}; a day, week, month or year tenor counts 1 or more, \
                     and overnight, tom-next and spot-next count 1",
                    tenor.count
                ));
            }
            let subject = CalendarSubject {
                pair: asked.pair.clone(),
                ..Default::default()
            };
            let pair = |rule: &DateRuleVersion| {
                matches!(&rule.parameters,
                    Some(date_rule_version::Parameters::Spot(held)) if same_pair(&held.pair, &asked.pair))
            };
            let Some(rule) = held.rule(DateRuleKind::Spot, Some(trade), pair) else {
                return Ok(rule_not_held(subject, DateRuleKind::Spot, now_ns));
            };
            let Some(date_rule_version::Parameters::Spot(parameters)) = &rule.parameters else {
                return Ok(rule_not_held(subject, DateRuleKind::Spot, now_ns));
            };
            let mut basis = DateBasis {
                rule: Some(rule_ref(rule)),
                calendars: Vec::new(),
            };
            Ok(
                match tenor_from(held, parameters, trade, unit, i64::from(tenor.count)) {
                    Ok((date, calendars)) => {
                        basis.calendars = calendars;
                        computed(date, basis)
                    }
                    Err(stop) => stopped(stop, basis, now_ns),
                },
            )
        }
    }
}

fn settle(
    held: &Held,
    rule: &DateRuleVersion,
    parameters: &SettlementDateRule,
    trade: Date,
    now_ns: i64,
) -> Answered {
    let mut basis = DateBasis {
        rule: Some(rule_ref(rule)),
        calendars: Vec::new(),
    };
    let set = match Set::of(held, &parameters.counting_calendars) {
        Ok(set) => set,
        Err(stop) => return stopped(stop, basis, now_ns),
    };
    basis.calendars = set.refs();
    match count_forward(&set, trade, i64::from(parameters.lag_days)) {
        Ok(date) => computed(date, basis),
        Err(stop) => stopped(stop, basis, now_ns),
    }
}

/// A tenor date from a trade date, computed from spot: days and weeks add
/// calendar days then roll by the rule's convention; months and years add
/// months, a month-end spot landing on the target month's last business day
/// under the end-of-month rule. The short dates count single business days
/// from the trade date, the next day and spot.
fn tenor_from(
    held: &Held,
    rule: &SpotDateRule,
    trade: Date,
    unit: TenorUnit,
    count: i64,
) -> Result<(Date, Vec<CalendarRef>), Stop> {
    let (spot, refs) = spot_from(held, rule, trade)?;
    let mut ids = rule.counting_calendars.clone();
    for id in &rule.spot_calendars {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    let both = Set::of(held, &ids)?;
    let calendars = Set::of(held, &rule.spot_calendars)?;
    let tenor_rule = rule.tenor_rule.unwrap_or_default();
    let convention = BusinessDayConvention::try_from(tenor_rule.convention)
        .unwrap_or(BusinessDayConvention::ModifiedFollowing);
    let date = match unit {
        TenorUnit::Overnight => count_forward(&both, trade, 1)?,
        TenorUnit::TomNext => {
            let tom = count_forward(&both, trade, 1)?;
            count_forward(&both, tom, 1)?
        }
        TenorUnit::SpotNext => count_forward(&both, spot, 1)?,
        TenorUnit::Day | TenorUnit::Week => {
            let days = if unit == TenorUnit::Week {
                7 * count
            } else {
                count
            };
            roll(&calendars, shift(spot, days)?, convention)?
        }
        TenorUnit::Month | TenorUnit::Year => {
            let months = if unit == TenorUnit::Year {
                12 * count
            } else {
                count
            };
            let target = add_months(spot, months)?;
            if tenor_rule.end_of_month && month_end(&calendars, spot)? == spot {
                month_end(&calendars, target)?
            } else {
                roll(&calendars, target, convention)?
            }
        }
        TenorUnit::Unspecified => return Err(Stop::Outside),
    };
    let mut refs = refs;
    for held in both.refs() {
        if !refs.contains(&held) {
            refs.push(held);
        }
    }
    Ok((date, refs))
}

/// W3.19: a calendar's days over a range, as of a recorded time; or not
/// found, with the miss to report.
pub fn read(
    held: &Held,
    request: &ReadCalendarRequest,
    now_ns: i64,
) -> Result<(ReadCalendarReply, Option<MissingCalendarDetectedEvent>), String> {
    if request.calendar_id.trim().is_empty() {
        return Err("calendar_id: names the calendar".into());
    }
    let from = date_field("from_date", &request.from_date)?;
    let to = date_field("to_date", &request.to_date)?;
    if to < from {
        return Err(format!(
            "to_date: {} is before from_date {}; a range ends on or after it starts",
            request.to_date, request.from_date
        ));
    }
    let Some(record) = held.calendars.get(&request.calendar_id) else {
        return Ok((
            ReadCalendarReply {
                found: false,
                calendar_id: request.calendar_id.clone(),
                ..Default::default()
            },
            Some(MissingCalendarDetectedEvent {
                subject: Some(CalendarSubject {
                    calendar_id: request.calendar_id.clone(),
                    ..Default::default()
                }),
                kind: DateRuleKind::Unspecified as i32,
                observed_at_ns: now_ns,
            }),
        ));
    };
    let days: Vec<CalendarDay> = record
        .days
        .iter()
        .filter(|day| Date::parse(&day.date).is_ok_and(|date| date >= from && date <= to))
        .cloned()
        .collect();
    Ok((
        ReadCalendarReply {
            found: true,
            calendar_id: record.calendar_id.clone(),
            version: record.version,
            days,
            weekends: record.weekends.clone(),
            coverage: record.coverage.clone(),
            sources: record.sources.clone(),
        },
        None,
    ))
}

/// The most a page of the calendars held lists, and its default.
pub const LIST_MOST: usize = 200;
pub const LIST_DEFAULT: usize = 25;

/// Whether a calendar answers a list's kind and text (W3.21; contract v23):
/// unspecified is every kind, and the text matches, ignoring case, its name
/// or its subject's codes -- an identifier others use (an ISO 4217 code, a
/// business centre), the settlement system's name and country, and what
/// `subject_codes` knows of its subject from the deployment's own records:
/// its currency's ISO 4217 code, each venue's MIC and name.
fn listed(
    record: &CalendarRecord,
    request: &ListCalendarsRequest,
    subject_codes: &dyn Fn(&CalendarRecord) -> Vec<String>,
) -> bool {
    if request.kind != 0 && record.kind != request.kind {
        return false;
    }
    let query = request.query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let mut codes: Vec<String> = vec![record.name.clone()];
    codes.extend(record.identifiers.iter().map(|held| held.value.clone()));
    if let Some(system) = &record.settlement_system {
        codes.push(system.name.clone());
        codes.push(system.country_code.clone());
    }
    codes.extend(subject_codes(record));
    codes
        .iter()
        .any(|code| code.to_lowercase().contains(&query))
}

/// W3.21: the calendars the deployment holds, one line each, at their latest
/// versions, paged by ID, from contract v23 each with its subject and
/// filtered by kind and text; and the rules held, on the first page.
pub fn list(
    calendars: &[KeptCalendar],
    rules: &[KeptDateRule],
    request: &ListCalendarsRequest,
) -> Result<ListCalendarsReply, String> {
    list_with(calendars, rules, request, &|_| Vec::new())
}

/// The same, the text also matched on what the deployment's records say of
/// each calendar's subject (its currency's code, its venues' MICs and names).
pub fn list_with(
    calendars: &[KeptCalendar],
    rules: &[KeptDateRule],
    request: &ListCalendarsRequest,
    subject_codes: &dyn Fn(&CalendarRecord) -> Vec<String>,
) -> Result<ListCalendarsReply, String> {
    if request.page_size < 0 || request.page_size as usize > LIST_MOST {
        return Err(format!(
            "page_size: is {}; 1 to {LIST_MOST}, or 0 for {LIST_DEFAULT}",
            request.page_size
        ));
    }
    let size = if request.page_size == 0 {
        LIST_DEFAULT
    } else {
        request.page_size as usize
    };
    let mut latest: BTreeMap<&str, &KeptCalendar> = BTreeMap::new();
    for kept in calendars {
        let id = kept.record.calendar_id.as_str();
        if latest
            .get(id)
            .is_none_or(|held| held.record.version < kept.record.version)
        {
            latest.insert(id, kept);
        }
    }
    let mut lines: Vec<HeldCalendar> = latest
        .values()
        .filter(|kept| kept.record.calendar_id.as_str() > request.cursor.as_str())
        .filter(|kept| listed(&kept.record, request, subject_codes))
        .map(|kept| HeldCalendar {
            calendar_id: kept.record.calendar_id.clone(),
            name: kept.record.name.clone(),
            kind: kept.record.kind,
            version: kept.record.version,
            coverage_to_date: kept
                .record
                .coverage
                .as_ref()
                .map(|coverage| coverage.to_date.clone())
                .unwrap_or_default(),
            refreshed_at_ns: kept.kept_at_ns,
            instrument_id: kept.record.instrument_id.clone(),
            venue_ids: kept.record.venue_ids.clone(),
            settlement_system: kept.record.settlement_system.clone(),
        })
        .collect();
    let next_cursor = if lines.len() > size {
        lines.truncate(size);
        lines
            .last()
            .map(|line| line.calendar_id.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut held_rules: BTreeMap<(i32, &str), &KeptDateRule> = BTreeMap::new();
    for kept in rules {
        let key = (kept.rule.kind, kept.rule.subject.as_str());
        if held_rules
            .get(&key)
            .is_none_or(|held| held.rule.version < kept.rule.version)
        {
            held_rules.insert(key, kept);
        }
    }
    let rules = if request.cursor.is_empty() {
        held_rules
            .values()
            .map(|kept| HeldDateRule {
                kind: kept.rule.kind,
                subject: kept.rule.subject.clone(),
                version: kept.rule.version,
                refreshed_at_ns: kept.kept_at_ns,
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(ListCalendarsReply {
        calendars: lines,
        next_cursor,
        rules,
    })
}

/// Each subject the store holds, as the conductor is asked to refresh it
/// (W3.16): each calendar by its ID, each settlement rule by its market and
/// each spot rule by its pair.
pub fn held_subjects(
    calendars: &[KeptCalendar],
    rules: &[KeptDateRule],
    now_ns: i64,
) -> Vec<MissingCalendarDetectedEvent> {
    let mut out: Vec<MissingCalendarDetectedEvent> = Vec::new();
    let mut push = |event: MissingCalendarDetectedEvent| {
        if !out.contains(&event) {
            out.push(event);
        }
    };
    for kept in calendars {
        push(MissingCalendarDetectedEvent {
            subject: Some(CalendarSubject {
                calendar_id: kept.record.calendar_id.clone(),
                ..Default::default()
            }),
            kind: DateRuleKind::Unspecified as i32,
            observed_at_ns: now_ns,
        });
    }
    for kept in rules {
        let (subject, kind) = match &kept.rule.parameters {
            Some(date_rule_version::Parameters::Settlement(held)) => (
                CalendarSubject {
                    country_code: held.country_code.clone(),
                    asset_class: held.asset_class,
                    ..Default::default()
                },
                DateRuleKind::Settlement,
            ),
            Some(date_rule_version::Parameters::Spot(held)) => (
                CalendarSubject {
                    pair: held.pair.clone(),
                    ..Default::default()
                },
                DateRuleKind::Spot,
            ),
            None => continue,
        };
        push(MissingCalendarDetectedEvent {
            subject: Some(subject),
            kind: kind as i32,
            observed_at_ns: now_ns,
        });
    }
    out
}

#[cfg(test)]
mod served;
#[cfg(test)]
mod tests;
