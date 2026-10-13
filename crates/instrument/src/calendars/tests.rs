//! The date read's cases (fixtures/reference/compute-date.yaml; the
//! calendars spec's verification), each against calendars written from its
//! source, and the calendar read's and the list's.

use meridian_domain::v1::{
    compute_date_request::Inputs, BusinessDayInputs, CalendarCoverage, CalendarDay, CalendarKind,
    CalendarSource, CalendarWeekend, DayCut, Identifier, ListCalendarsRequest, RollInputs,
    SettlementDateInputs, SettlementSystem, SpotDateInputs, Tenor, TenorDateInputs, TenorDateRule,
};

use super::*;

const USSS: &str = "CAL-01JAE0000000000000000USSS";
const USD: &str = "CAL-01JAE000000000000000USD1";
const EUR: &str = "CAL-01JAE000000000000000EUR1";
const CAD: &str = "CAL-01JAE000000000000000CAD1";
const JPY: &str = "CAL-01JAE000000000000000JPY1";
const CASH_USD: &str = "INS-01J8XQ4M7K00000000CASHUSD";
const CASH_EUR: &str = "INS-01J8XQ4M7K00000000CASHEUR";
const CASH_CAD: &str = "INS-01J8XQ4M7K00000000CASHCAD";
const CASH_JPY: &str = "INS-01J8XQ4M7K00000000CASHJPY";
const AAPL: &str = "INS-01J8XQ4M7K0000000000AAPL";
const KEPT: i64 = 1_760_400_000_000_000_000;

fn calendar(id: &str, version: i64, closed: &[(&str, &str)]) -> CalendarRecord {
    CalendarRecord {
        calendar_id: id.into(),
        name: id.into(),
        weekends: vec![CalendarWeekend {
            weekdays: vec![Weekday::Saturday as i32, Weekday::Sunday as i32],
            valid_from_date: "2016-01-01".into(),
        }],
        days: closed
            .iter()
            .map(|(date, name)| CalendarDay {
                date: (*date).into(),
                kind: CalendarDayKind::Closed as i32,
                name: (*name).into(),
                source: "primary".into(),
            })
            .collect(),
        coverage: Some(CalendarCoverage {
            from_date: "2016-01-01".into(),
            to_date: "2028-12-31".into(),
        }),
        sources: vec![CalendarSource {
            source_id: "primary".into(),
            name: "the public primary source".into(),
            address: "https://example.org/holidays".into(),
            read_at_ns: 1,
        }],
        version,
        ..Default::default()
    }
}

fn kept(record: CalendarRecord, at: i64) -> KeptCalendar {
    KeptCalendar {
        record,
        kept_at_ns: at,
    }
}

fn settlement_rule(version: i64, lag: i32, from: &str) -> KeptDateRule {
    KeptDateRule {
        rule: DateRuleVersion {
            kind: DateRuleKind::Settlement as i32,
            subject: "US/equity".into(),
            version,
            parameters: Some(date_rule_version::Parameters::Settlement(
                SettlementDateRule {
                    country_code: "US".into(),
                    asset_class: AssetClass::Equity as i32,
                    venue_id: String::new(),
                    lag_days: lag,
                    counting_calendars: vec![USSS.into()],
                    effective_from_date: from.into(),
                },
            )),
            ..Default::default()
        },
        kept_at_ns: KEPT,
    }
}

fn spot_rule(
    subject: &str,
    pair: [&str; 2],
    lag: i32,
    counting: &[&str],
    spot: &[&str],
) -> KeptDateRule {
    KeptDateRule {
        rule: DateRuleVersion {
            kind: DateRuleKind::Spot as i32,
            subject: subject.into(),
            version: 1,
            parameters: Some(date_rule_version::Parameters::Spot(SpotDateRule {
                pair: pair.iter().map(|side| side.to_string()).collect(),
                lag_days: lag,
                counting_calendars: counting.iter().map(|c| c.to_string()).collect(),
                spot_calendars: spot.iter().map(|c| c.to_string()).collect(),
                cut: Some(DayCut {
                    local_time: "17:00".into(),
                    time_zone: "America/New_York".into(),
                }),
                tenor_rule: Some(TenorDateRule {
                    convention: BusinessDayConvention::ModifiedFollowing as i32,
                    end_of_month: true,
                }),
                effective_from_date: "2016-01-01".into(),
                counting_mode: 0,
            })),
            ..Default::default()
        },
        kept_at_ns: KEPT,
    }
}

/// The seed: the US settlement calendar (Columbus Day and Veterans Day
/// closed, as DTC is; Memorial Day 2024), USD, TARGET's EUR (Good Friday and
/// Easter Monday 2027), CAD and JPY; the US equities rule, T+2 from 5
/// September 2017 and T+1 from 28 May 2024; and three spot rules.
fn seed() -> (Vec<KeptCalendar>, Vec<KeptDateRule>) {
    let calendars = vec![
        kept(
            calendar(
                USSS,
                4,
                &[
                    ("2024-05-27", "Memorial Day"),
                    ("2026-10-12", "Columbus Day"),
                    ("2026-11-11", "Veterans Day"),
                ],
            ),
            KEPT,
        ),
        kept(
            calendar(
                USD,
                1,
                &[
                    ("2026-10-12", "Columbus Day"),
                    ("2026-11-11", "Veterans Day"),
                ],
            ),
            KEPT,
        ),
        kept(
            calendar(
                EUR,
                1,
                &[
                    ("2027-03-26", "Good Friday"),
                    ("2027-03-29", "Easter Monday"),
                ],
            ),
            KEPT,
        ),
        kept(calendar(CAD, 1, &[]), KEPT),
        kept(
            calendar(JPY, 1, &[("2027-02-23", "Emperor's Birthday")]),
            KEPT,
        ),
    ];
    let rules = vec![
        settlement_rule(1, 2, "2017-09-05"),
        settlement_rule(2, 1, "2024-05-28"),
        spot_rule("EUR/USD", [CASH_EUR, CASH_USD], 2, &[EUR], &[EUR, USD]),
        spot_rule("USD/CAD", [CASH_USD, CASH_CAD], 1, &[CAD], &[USD, CAD]),
        spot_rule("USD/JPY", [CASH_USD, CASH_JPY], 2, &[JPY], &[USD, JPY]),
    ];
    (calendars, rules)
}

fn us_equity(instrument: &str) -> Option<Market> {
    (instrument == AAPL).then(|| Market {
        asset_class: AssetClass::Equity as i32,
        country_code: "US".into(),
        venue_id: "VEN-XNAS".into(),
    })
}

fn ask(held: &Held, inputs: Inputs) -> Answered {
    compute(
        held,
        &ComputeDateRequest {
            inputs: Some(inputs),
            as_of_ns: 0,
        },
        us_equity,
        KEPT + 1,
    )
    .unwrap()
}

fn latest() -> Held {
    let (calendars, rules) = seed();
    Held::as_of(&calendars, &rules, 0)
}

fn settlement(trade: &str) -> Inputs {
    Inputs::Settlement(SettlementDateInputs {
        instrument_id: AAPL.into(),
        trade_date: trade.into(),
    })
}

fn spot(pair: [&str; 2], trade: &str) -> Inputs {
    Inputs::Spot(SpotDateInputs {
        pair: pair.iter().map(|s| s.to_string()).collect(),
        trade_date: trade.into(),
        at_ns: 0,
    })
}

fn calendars_of(answer: &Answered) -> Vec<(String, i64)> {
    answer
        .reply
        .basis
        .as_ref()
        .unwrap()
        .calendars
        .iter()
        .map(|c| (c.calendar_id.clone(), c.version))
        .collect()
}

#[test]
fn a_us_equity_bought_friday_9_october_2026_settles_tuesday_13_october() {
    // Columbus Day: DTC provides no settlement while NYSE is open.
    let answer = ask(&latest(), settlement("2026-10-09"));
    assert_eq!(answer.reply.outcome, DateOutcome::Computed as i32);
    assert_eq!(answer.reply.date, "2026-10-13");
    let basis = answer.reply.basis.as_ref().unwrap();
    assert_eq!(
        basis.rule,
        Some(DateRuleRef {
            kind: DateRuleKind::Settlement as i32,
            subject: "US/equity".into(),
            version: 2,
        })
    );
    assert_eq!(calendars_of(&answer), vec![(USSS.to_string(), 4)]);
    assert!(answer.missing.is_none());
}

#[test]
fn the_t2_version_answers_before_28_may_2024_and_the_t1_version_after() {
    let held = latest();
    let before = ask(&held, settlement("2024-05-23"));
    // Friday the 24th, then Memorial Day closed: Tuesday the 28th, T+2.
    assert_eq!(before.reply.date, "2024-05-28");
    assert_eq!(
        before
            .reply
            .basis
            .as_ref()
            .unwrap()
            .rule
            .as_ref()
            .unwrap()
            .version,
        1
    );
    let after = ask(&held, settlement("2024-05-28"));
    assert_eq!(after.reply.date, "2024-05-29");
    assert_eq!(
        after
            .reply
            .basis
            .as_ref()
            .unwrap()
            .rule
            .as_ref()
            .unwrap()
            .version,
        2
    );
}

#[test]
fn eur_usd_traded_wednesday_24_march_2027_has_spot_tuesday_30_march() {
    // Good Friday and Easter Monday are TARGET closing days.
    let answer = ask(&latest(), spot([CASH_EUR, CASH_USD], "2027-03-24"));
    assert_eq!(answer.reply.date, "2027-03-30");
    assert_eq!(
        calendars_of(&answer),
        vec![(EUR.to_string(), 1), (USD.to_string(), 1)]
    );
    // The pair in either order is the one rule.
    let reversed = ask(&latest(), spot([CASH_USD, CASH_EUR], "2027-03-24"));
    assert_eq!(reversed.reply.date, "2027-03-30");
}

#[test]
fn usd_cad_spot_is_t1_and_never_on_a_usd_holiday() {
    // T+1 on CAD is Wednesday 11 November, Veterans Day: spot rolls to the
    // first day good in USD and CAD.
    let answer = ask(&latest(), spot([CASH_USD, CASH_CAD], "2026-11-10"));
    assert_eq!(answer.reply.date, "2026-11-12");
    let ordinary = ask(&latest(), spot([CASH_USD, CASH_CAD], "2026-11-03"));
    assert_eq!(ordinary.reply.date, "2026-11-04");
}

#[test]
fn usd_jpy_one_month_from_a_month_end_spot_lands_on_the_target_months_last_business_day() {
    // Spot is Friday 26 February 2027, February's last business day: 1M
    // under the end-of-month rule is Wednesday 31 March, not the 26th.
    let tenor = Inputs::Tenor(TenorDateInputs {
        pair: vec![CASH_USD.into(), CASH_JPY.into()],
        tenor: Some(Tenor {
            count: 1,
            unit: TenorUnit::Month as i32,
        }),
        trade_date: "2027-02-24".into(),
    });
    let answer = ask(&latest(), tenor);
    assert_eq!(answer.reply.date, "2027-03-31");
    assert_eq!(
        answer
            .reply
            .basis
            .as_ref()
            .unwrap()
            .rule
            .as_ref()
            .unwrap()
            .subject,
        "USD/JPY"
    );
}

#[test]
fn the_short_dates_count_single_business_days() {
    let short = |unit: TenorUnit, trade: &str| {
        ask(
            &latest(),
            Inputs::Tenor(TenorDateInputs {
                pair: vec![CASH_EUR.into(), CASH_USD.into()],
                tenor: Some(Tenor {
                    count: 1,
                    unit: unit as i32,
                }),
                trade_date: trade.into(),
            }),
        )
        .reply
        .date
    };
    // Thursday 25 March 2027: overnight is the next good day in EUR and USD,
    // over Good Friday and Easter Monday; tom-next the one after; spot-next
    // the day after spot.
    assert_eq!(short(TenorUnit::Overnight, "2027-03-25"), "2027-03-30");
    assert_eq!(short(TenorUnit::TomNext, "2027-03-25"), "2027-03-31");
    assert_eq!(short(TenorUnit::SpotNext, "2027-03-24"), "2027-03-31");
}

#[test]
fn an_execution_at_1730_new_york_takes_the_next_trade_date() {
    // Monday 13 October 2025 at 21:30 UTC is 17:30 in New York, after the
    // 17:00 cut: trade date Tuesday the 14th, EUR/USD spot Thursday the 16th.
    let at = |ns: i64| {
        Inputs::Spot(SpotDateInputs {
            pair: vec![CASH_EUR.into(), CASH_USD.into()],
            trade_date: String::new(),
            at_ns: ns,
        })
    };
    let after = ask(&latest(), at(1_760_391_000_000_000_000));
    assert_eq!(after.reply.date, "2025-10-16");
    // An hour earlier, 16:30, is Monday's trade: spot Wednesday the 15th.
    let before = ask(&latest(), at(1_760_387_400_000_000_000));
    assert_eq!(before.reply.date, "2025-10-15");
}

#[test]
fn a_date_past_coverage_is_not_computed_and_never_a_business_day_by_default() {
    let answer = ask(&latest(), settlement("2028-12-29"));
    // Friday the 29th: T+1 is Monday 1 January 2029, past the coverage.
    assert_eq!(answer.reply.outcome, DateOutcome::OutsideCoverage as i32);
    assert!(answer.reply.date.is_empty());
    let checked = ask(
        &latest(),
        Inputs::BusinessDay(BusinessDayInputs {
            date: "2029-01-02".into(),
            calendars: vec![USD.into()],
        }),
    );
    assert_eq!(checked.reply.outcome, DateOutcome::OutsideCoverage as i32);
    assert!(!checked.reply.business_day);
}

#[test]
fn a_rule_or_a_calendar_not_held_says_so_and_is_reported_by_its_subject() {
    let pair = [CASH_USD, "INS-01J8XQ4M7K00000000CASHGBP"];
    let answer = ask(&latest(), spot(pair, "2026-10-09"));
    assert_eq!(answer.reply.outcome, DateOutcome::RuleNotHeld as i32);
    let missing = answer.missing.unwrap();
    assert_eq!(missing.kind, DateRuleKind::Spot as i32);
    assert_eq!(
        missing.subject.unwrap().pair,
        pair.map(String::from).to_vec()
    );

    let answer = ask(
        &latest(),
        Inputs::Roll(RollInputs {
            date: "2026-10-10".into(),
            convention: BusinessDayConvention::Following as i32,
            calendars: vec![USD.into(), "CAL-NOT-HELD".into()],
        }),
    );
    assert_eq!(answer.reply.outcome, DateOutcome::CalendarNotHeld as i32);
    assert_eq!(
        answer.missing.unwrap().subject.unwrap().calendar_id,
        "CAL-NOT-HELD"
    );

    // An instrument whose market is not known -- no listing venue, so no
    // country: rule not held, and reported missing as a calendar is, by the
    // asset class it has and the country it lacks.
    let unlisted = compute(
        &latest(),
        &ComputeDateRequest {
            inputs: Some(settlement("2026-10-09")),
            as_of_ns: 0,
        },
        |_| {
            Some(Market {
                asset_class: AssetClass::Equity as i32,
                country_code: String::new(),
                venue_id: String::new(),
            })
        },
        KEPT + 1,
    )
    .unwrap();
    assert_eq!(unlisted.reply.outcome, DateOutcome::RuleNotHeld as i32);
    assert!(unlisted.reply.date.is_empty());
    let missing = unlisted.missing.expect("reported missing");
    assert_eq!(missing.kind, DateRuleKind::Settlement as i32);
    assert_eq!(missing.observed_at_ns, KEPT + 1);
    let subject = missing.subject.unwrap();
    assert_eq!(
        (subject.country_code.as_str(), subject.asset_class),
        ("", AssetClass::Equity as i32)
    );

    // A settlement rule not held for the market: reported by its market.
    let (calendars, _) = seed();
    let none = Held::as_of(&calendars, &[], 0);
    let answer = ask(&none, settlement("2026-10-09"));
    assert_eq!(answer.reply.outcome, DateOutcome::RuleNotHeld as i32);
    let subject = answer.missing.unwrap().subject.unwrap();
    assert_eq!(
        (subject.country_code.as_str(), subject.asset_class),
        ("US", AssetClass::Equity as i32)
    );
}

#[test]
fn a_read_as_of_an_earlier_recorded_time_reproduces_the_answer_before_an_ad_hoc_closure() {
    // USD's version 2 closes Thursday 12 November 2026, kept later.
    let (mut calendars, rules) = seed();
    calendars.push(kept(
        calendar(
            USD,
            2,
            &[
                ("2026-10-12", "Columbus Day"),
                ("2026-11-11", "Veterans Day"),
                ("2026-11-12", "A day of mourning"),
            ],
        ),
        KEPT + 1_000,
    ));
    let asked = |as_of_ns: i64| {
        compute(
            &Held::as_of(&calendars, &rules, as_of_ns),
            &ComputeDateRequest {
                inputs: Some(spot([CASH_USD, CASH_CAD], "2026-11-10")),
                as_of_ns,
            },
            us_equity,
            KEPT + 2_000,
        )
        .unwrap()
    };
    let before = asked(KEPT + 500);
    assert_eq!(before.reply.date, "2026-11-12");
    assert!(calendars_of(&before).contains(&(USD.to_string(), 1)));
    let now = asked(0);
    assert_eq!(now.reply.date, "2026-11-13");
    assert!(calendars_of(&now).contains(&(USD.to_string(), 2)));
    // The same inputs and the same versions always give the same date.
    assert_eq!(asked(KEPT + 500).reply, before.reply);
    assert_eq!(asked(0).reply, now.reply);
}

#[test]
fn a_roll_and_a_business_day_name_their_calendars_and_no_rule() {
    let roll = |convention: BusinessDayConvention, date: &str| {
        ask(
            &latest(),
            Inputs::Roll(RollInputs {
                date: date.into(),
                convention: convention as i32,
                calendars: vec![EUR.into()],
            }),
        )
        .reply
        .date
    };
    // Easter 2027: Friday 26 and Monday 29 March closed.
    assert_eq!(
        roll(BusinessDayConvention::Following, "2027-03-26"),
        "2027-03-30"
    );
    assert_eq!(
        roll(BusinessDayConvention::Preceding, "2027-03-29"),
        "2027-03-25"
    );
    // Saturday 31 July 2027: following is in August, so modified following
    // goes back to Friday the 30th.
    assert_eq!(
        roll(BusinessDayConvention::ModifiedFollowing, "2027-07-31"),
        "2027-07-30"
    );
    let checked = ask(
        &latest(),
        Inputs::BusinessDay(BusinessDayInputs {
            date: "2026-10-12".into(),
            calendars: vec![USSS.into(), USD.into()],
        }),
    );
    assert_eq!(checked.reply.outcome, DateOutcome::Computed as i32);
    assert!(!checked.reply.business_day);
    assert!(checked.reply.basis.as_ref().unwrap().rule.is_none());
}

#[test]
fn a_malformed_question_is_refused_naming_its_field() {
    let refused = |inputs: Option<Inputs>| {
        compute(
            &latest(),
            &ComputeDateRequest {
                inputs,
                as_of_ns: 0,
            },
            us_equity,
            KEPT,
        )
        .err()
        .unwrap()
    };
    assert!(refused(None).starts_with("inputs"));
    assert!(refused(Some(settlement("2026-02-30"))).starts_with("settlement.trade_date"));
    assert!(refused(Some(spot([CASH_USD, CASH_USD], "2026-10-09"))).starts_with("spot.pair"));
    assert!(refused(Some(Inputs::Spot(SpotDateInputs {
        pair: vec![CASH_EUR.into(), CASH_USD.into()],
        trade_date: "2026-10-09".into(),
        at_ns: 1,
    })))
    .starts_with("spot.trade_date"));
    assert!(refused(Some(Inputs::Settlement(SettlementDateInputs {
        instrument_id: "INS-UNKNOWN".into(),
        trade_date: "2026-10-09".into(),
    })))
    .starts_with("settlement.instrument_id"));
}

#[test]
fn a_calendar_is_read_over_a_range_as_of_a_recorded_time() {
    let read_range = |held: &Held, from: &str, to: &str| {
        read(
            held,
            &ReadCalendarRequest {
                calendar_id: USSS.into(),
                from_date: from.into(),
                to_date: to.into(),
                as_of_ns: 0,
            },
            KEPT,
        )
    };
    let (reply, missing) = read_range(&latest(), "2026-10-01", "2026-10-31").unwrap();
    assert!(reply.found && missing.is_none());
    assert_eq!(reply.version, 4);
    assert_eq!(reply.days.len(), 1);
    assert_eq!(reply.days[0].name, "Columbus Day");
    assert_eq!(reply.days[0].source, "primary");
    assert_eq!(reply.sources.len(), 1);
    assert!(read_range(&latest(), "2026-10-31", "2026-10-01")
        .unwrap_err()
        .starts_with("to_date"));
    // Not held: answered so, and reported missing.
    let (calendars, rules) = seed();
    let before = Held::as_of(&calendars, &rules, KEPT - 1);
    let (reply, missing) = read_range(&before, "2026-10-01", "2026-10-31").unwrap();
    assert!(!reply.found);
    assert_eq!(missing.unwrap().subject.unwrap().calendar_id, USSS);
}

#[test]
fn the_calendars_held_are_listed_one_line_each_with_the_rules() {
    let (calendars, rules) = seed();
    let first = list(
        &calendars,
        &rules,
        &ListCalendarsRequest {
            page_size: 2,
            cursor: String::new(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(first.calendars.len(), 2);
    assert_eq!(first.rules.len(), 4, "the US rule once, at its latest");
    assert!(first
        .rules
        .iter()
        .any(|rule| rule.subject == "US/equity" && rule.version == 2));
    let second = list(
        &calendars,
        &rules,
        &ListCalendarsRequest {
            page_size: 2,
            cursor: first.next_cursor.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(second.calendars.len(), 2);
    assert!(second.rules.is_empty());
    assert_eq!(second.calendars[0].coverage_to_date, "2028-12-31");
    assert!(list(
        &calendars,
        &rules,
        &ListCalendarsRequest {
            page_size: 201,
            cursor: String::new(),
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn every_subject_held_is_named_for_the_conductor_to_refresh() {
    let (calendars, rules) = seed();
    let subjects = held_subjects(&calendars, &rules, KEPT);
    assert_eq!(subjects.len(), 5 + 4);
}

// ── Contract v23 ──

#[test]
fn a_cross_counted_per_currency_takes_each_currencys_spot_and_the_later() {
    // EUR/GBP traded Thursday 30 April 2026: 1 May a TARGET closing day, 4
    // May England's Early May bank holiday. Per currency: EUR's T+2 on
    // TARGET is Tuesday 5 May, GBP's on its own calendar Tuesday 5 May too;
    // jointly, a day counting only if good on both, Wednesday 6 May.
    const GBP: &str = "CAL-01JAE000000000000000GBP1";
    const CASH_GBP: &str = "INS-01J8XQ4M7K00000000CASHGBP";
    let mut eur = calendar(EUR, 1, &[("2026-05-01", "Labour Day")]);
    eur.instrument_id = CASH_EUR.into();
    let mut gbp = calendar(GBP, 1, &[("2026-05-04", "Early May bank holiday")]);
    gbp.instrument_id = CASH_GBP.into();
    let calendars = vec![kept(eur, KEPT), kept(gbp, KEPT)];
    let mut joint = spot_rule("EUR/GBP", [CASH_EUR, CASH_GBP], 2, &[EUR, GBP], &[EUR, GBP]);
    let joint_held = Held::as_of(&calendars, &[joint.clone()], 0);
    let answer = ask(&joint_held, spot([CASH_EUR, CASH_GBP], "2026-04-30"));
    assert_eq!(
        answer.reply.date, "2026-05-06",
        "unspecified counts jointly"
    );
    if let Some(date_rule_version::Parameters::Spot(rule)) = joint.rule.parameters.as_mut() {
        rule.counting_mode = SpotCountingMode::EachCurrency as i32;
    }
    let each = Held::as_of(&calendars, &[joint], 0);
    let answer = ask(&each, spot([CASH_EUR, CASH_GBP], "2026-04-30"));
    assert_eq!(answer.reply.date, "2026-05-05");
    assert_eq!(
        calendars_of(&answer),
        vec![(EUR.to_string(), 1), (GBP.to_string(), 1)]
    );
}

#[test]
fn a_calendar_line_carries_its_subject_and_the_list_filters_by_kind_and_text() {
    let (mut calendars, rules) = seed();
    for held in &mut calendars {
        if held.record.calendar_id == USSS {
            held.record.kind = CalendarKind::SecuritiesSettlement as i32;
            held.record.settlement_system = Some(SettlementSystem {
                name: "DTC".into(),
                country_code: "US".into(),
            });
        }
        if held.record.calendar_id == EUR {
            held.record.kind = CalendarKind::Currency as i32;
            held.record.instrument_id = CASH_EUR.into();
            held.record.identifiers = vec![Identifier {
                scheme: "iso4217".into(),
                value: "EUR".into(),
                ..Default::default()
            }];
        }
    }
    let everything = list(&calendars, &rules, &ListCalendarsRequest::default()).unwrap();
    let eur = everything
        .calendars
        .iter()
        .find(|line| line.calendar_id == EUR)
        .unwrap();
    assert_eq!(eur.instrument_id, CASH_EUR);
    let settlement = list(
        &calendars,
        &rules,
        &ListCalendarsRequest {
            kind: CalendarKind::SecuritiesSettlement as i32,
            query: "us".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(settlement.calendars.len(), 1);
    assert_eq!(settlement.calendars[0].calendar_id, USSS);
    assert_eq!(
        settlement.calendars[0]
            .settlement_system
            .as_ref()
            .map(|system| system.name.as_str()),
        Some("DTC")
    );
    let by_code = list(
        &calendars,
        &rules,
        &ListCalendarsRequest {
            query: "eur".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        by_code
            .calendars
            .iter()
            .map(|line| line.calendar_id.as_str())
            .collect::<Vec<_>>(),
        vec![EUR]
    );
}
