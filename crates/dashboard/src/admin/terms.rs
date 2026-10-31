//! A debt record's terms on its page (contract v29; rulings 676 to 682;
//! W1.1, W1.24): its bond's, pool's, TBA's, tranche's or CD's, as the
//! platform's record states them, one line each so the page fits one
//! screen, a schedule's rows each a line of its own, paged.
//!
//! The issuer, a guarantor, a municipal obligor and an insurer are `ENT-`
//! company records (594): each is named by the record the store holds,
//! resolved by its ID ([`company_ids`] for the page's handler to ask), and
//! by its ID alone where the store holds none. Another instrument a term
//! names -- a floater's index, TIPS' CPI-U, a make-whole's benchmark, a
//! STRIPS component's whole bond -- links to its record. The values are the
//! contract's: a day count by FIX's name for it (1950), each other enum by
//! its value, a decimal exactly as held, never through a float.

use std::collections::BTreeMap;

use meridian_domain::conventions::day_count_value_words;
use meridian_domain::debt::value_words;
use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    BusinessDayConvention, CertificateOfDeposit, CmoTranche, CouponType, DebtSecurityType,
    FederalTaxStatus, FloatingRateTerms, InstrumentRecord, InterestType, IssuerType, MbsPool,
    MortgageAgency, PrincipalType, RecordDateRule, RedemptionKind, RedemptionStyle,
    RedemptionWindow, RefundingStatus, Seniority, SifmaSettlementClass, Tba,
};
use meridian_pb::v1::Decimal;

use crate::html::escape;

/// One line of the terms: its label and its value, as HTML (escaped here).
struct Line {
    label: &'static str,
    value: String,
}

fn exact(decimal: Option<&Decimal>) -> String {
    decimal
        .and_then(|decimal| Exact::from_wire(decimal).ok())
        .map(|exact| exact.to_string())
        .unwrap_or_default()
}

fn words<E: TryFrom<i32>>(value: i32, name: fn(&E) -> &'static str, prefix: &str) -> String {
    E::try_from(value)
        .ok()
        .map(|known| value_words(name(&known), prefix, false))
        .unwrap_or_default()
}

fn joined(parts: impl IntoIterator<Item = String>, between: &str) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(between)
}

/// A record another term names, linked to its page.
fn record_link(instrument_id: &str) -> String {
    if instrument_id.is_empty() {
        return String::new();
    }
    format!(
        "<a href=\"/admin/instruments/{id}\">{id}</a>",
        id = escape(instrument_id)
    )
}

/// A company by the name its record holds, with its ID; its ID alone where
/// the store holds none.
fn company(company_id: &str, names: &BTreeMap<String, String>) -> String {
    if company_id.is_empty() {
        return String::new();
    }
    match names.get(company_id) {
        Some(name) => format!("{} ({})", escape(name), escape(company_id)),
        None => escape(company_id),
    }
}

/// The company IDs a record's terms name, for the page's handler to resolve
/// by their records: the issuer, a guarantor, a municipal obligor and an
/// insurer, a CD's bank.
pub fn company_ids(record: &InstrumentRecord) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(bond) = &record.bond {
        ids.push(bond.issuer_company_id.clone());
        ids.push(bond.guarantor_company_id.clone());
        if let Some(muni) = &bond.municipal {
            ids.push(muni.obligor_company_id.clone());
            ids.push(muni.insurer_company_id.clone());
        }
    }
    if let Some(cd) = &record.certificate_of_deposit {
        ids.push(cd.issuer_company_id.clone());
    }
    ids.retain(|id| !id.is_empty());
    ids.sort();
    ids.dedup();
    ids
}

fn day_count(value: i32) -> String {
    day_count_value_words(value).to_string()
}

fn record_date(rule: i32, days: u32) -> String {
    match RecordDateRule::try_from(rule).unwrap_or(RecordDateRule::Unspecified) {
        RecordDateRule::Unspecified => String::new(),
        RecordDateRule::CalendarDaysBeforePayment => {
            format!("record date {days} calendar days before payment")
        }
        RecordDateRule::BusinessDaysBeforePayment => {
            format!("record date {days} business days before payment")
        }
        RecordDateRule::LastDayOfPriorMonth => "record date the prior month's last day".into(),
    }
}

fn conventions(convention: i32, calendar: &str, end_of_month: bool) -> String {
    joined(
        [
            words::<BusinessDayConvention>(
                convention,
                BusinessDayConvention::as_str_name,
                "BUSINESS_DAY_CONVENTION_",
            ),
            if calendar.is_empty() {
                String::new()
            } else {
                format!("on {}", escape(calendar))
            },
            if end_of_month {
                "end of month".to_string()
            } else {
                String::new()
            },
        ],
        " ",
    )
}

fn per_year(frequency: u32) -> String {
    match frequency {
        0 => String::new(),
        1 => "annual".into(),
        2 => "semi-annual".into(),
        4 => "quarterly".into(),
        12 => "monthly".into(),
        n => format!("{n} a year"),
    }
}

fn floater(terms: &FloatingRateTerms) -> String {
    let spread = exact(terms.spread_bps.as_ref());
    joined(
        [
            format!(
                "{}{}",
                record_link(&terms.index_instrument_id),
                if spread.is_empty() {
                    String::new()
                } else {
                    format!(" + {spread} bp")
                }
            ),
            match exact(terms.multiplier.as_ref()) {
                multiplier if multiplier.is_empty() => String::new(),
                multiplier => format!("times {multiplier}"),
            },
            match exact(terms.cap_rate.as_ref()) {
                cap if cap.is_empty() => String::new(),
                cap => format!("cap {cap}%"),
            },
            match exact(terms.floor_rate.as_ref()) {
                floor if floor.is_empty() => String::new(),
                floor => format!("floor {floor}%"),
            },
            match terms.reset_frequency {
                0 => String::new(),
                n => format!("reset {n} a year"),
            },
            match terms.lookback_days {
                0 => String::new(),
                n => format!("{n}-day lookback"),
            },
            if terms.in_arrears {
                "in arrears".to_string()
            } else {
                String::new()
            },
        ],
        ", ",
    )
}

fn window(window: &RedemptionWindow) -> String {
    let kind =
        words::<RedemptionKind>(window.kind, RedemptionKind::as_str_name, "REDEMPTION_KIND_");
    let dates = match (window.from_date.as_str(), window.to_date.as_str()) {
        ("", "") => String::new(),
        (from, "") => format!("from {from}"),
        (from, to) => format!("{from} to {to}"),
    };
    let price = match exact(window.make_whole_spread_bps.as_ref()) {
        spread if !spread.is_empty() => format!(
            "{spread} bp over {}",
            if window.make_whole_benchmark_instrument_id.is_empty() {
                "its benchmark".to_string()
            } else {
                record_link(&window.make_whole_benchmark_instrument_id)
            }
        ),
        _ => match exact(window.price.as_ref()) {
            price if price.is_empty() => String::new(),
            price => format!("at {price}"),
        },
    };
    let notice = match (window.notice_days_min, window.notice_days_max) {
        (0, 0) => String::new(),
        (least, 0) => format!("notice {least} days"),
        (least, most) => format!("notice {least} to {most} days"),
    };
    let style = words::<RedemptionStyle>(
        window.style,
        RedemptionStyle::as_str_name,
        "REDEMPTION_STYLE_",
    );
    joined(
        [joined([kind, escape(&dates)], " "), price, notice, style],
        ", ",
    )
}

fn bond_lines(bond: &meridian_domain::v1::Bond, names: &BTreeMap<String, String>) -> Vec<Line> {
    let coupon_type = CouponType::try_from(bond.coupon_type).unwrap_or(CouponType::Unspecified);
    let mut lines = vec![
        Line {
            label: "Security type",
            value: escape(&value_words(
                DebtSecurityType::try_from(bond.security_type)
                    .map(|t| t.as_str_name())
                    .unwrap_or(""),
                "DEBT_SECURITY_TYPE_",
                true,
            )),
        },
        Line {
            label: "Issuer",
            value: joined(
                [
                    company(&bond.issuer_company_id, names),
                    words::<IssuerType>(bond.issuer_type, IssuerType::as_str_name, "ISSUER_TYPE_"),
                ],
                ", ",
            ),
        },
        Line {
            label: "Guarantor",
            value: company(&bond.guarantor_company_id, names),
        },
        Line {
            label: "Issued under",
            value: escape(&joined(
                [
                    bond.governing_law.clone(),
                    joined(
                        [
                            bond.state_or_province_of_issue.clone(),
                            bond.country_of_issue.clone(),
                        ],
                        ", ",
                    ),
                ],
                " law; ",
            )),
        },
        Line {
            label: "Coupon",
            value: match coupon_type {
                CouponType::Zero => "zero coupon".to_string(),
                CouponType::Floating => joined(
                    [
                        "floating".to_string(),
                        bond.floating.as_ref().map(floater).unwrap_or_default(),
                        per_year(bond.coupon_frequency),
                    ],
                    ", ",
                ),
                _ => joined(
                    [
                        match exact(bond.coupon_rate.as_ref()) {
                            rate if rate.is_empty() => "not yet stated".to_string(),
                            rate => format!("{rate}%"),
                        },
                        words::<CouponType>(
                            bond.coupon_type,
                            CouponType::as_str_name,
                            "COUPON_TYPE_",
                        ),
                        per_year(bond.coupon_frequency),
                    ],
                    ", ",
                ),
            },
        },
        Line {
            label: "Day count",
            value: day_count(bond.day_count),
        },
        Line {
            label: "Dates",
            value: escape(&joined(
                [
                    labelled("issued", &bond.issue_date),
                    labelled("dated", &bond.dated_date),
                    labelled("first coupon", &bond.first_coupon_date),
                    labelled("matures", &bond.maturity_date),
                ],
                ", ",
            )),
        },
        Line {
            label: "Conventions",
            value: joined(
                [
                    conventions(
                        bond.business_day_convention,
                        &bond.payment_calendar_id,
                        bond.end_of_month,
                    ),
                    record_date(bond.record_date_rule, bond.record_date_days),
                ],
                ", ",
            ),
        },
        Line {
            label: "Denominations",
            value: joined(
                [
                    labelled_value("minimum", exact(bond.minimum_denomination.as_ref())),
                    labelled_value("increment", exact(bond.denomination_increment.as_ref())),
                    labelled_value("issued", exact(bond.original_issue_amount.as_ref())),
                ],
                ", ",
            ),
        },
        Line {
            label: "Ranking",
            value: joined(
                [
                    words::<Seniority>(bond.seniority, Seniority::as_str_name, "SENIORITY_"),
                    if bond.rule_144a {
                        "Rule 144A".to_string()
                    } else {
                        String::new()
                    },
                    if bond.reg_s {
                        "Regulation S".to_string()
                    } else {
                        String::new()
                    },
                ],
                ", ",
            ),
        },
        Line {
            label: "Inflation index",
            value: joined(
                [
                    record_link(&bond.inflation_index_instrument_id),
                    labelled_value("base", exact(bond.base_index_level.as_ref())),
                ],
                ", ",
            ),
        },
        Line {
            label: "Stripped from",
            value: record_link(&bond.stripped_from),
        },
    ];
    if let Some(muni) = &bond.municipal {
        lines.push(Line {
            label: "Municipal",
            value: joined(
                [
                    match FederalTaxStatus::try_from(muni.federal_tax_status) {
                        Ok(FederalTaxStatus::Exempt) => "federally tax-exempt".to_string(),
                        Ok(FederalTaxStatus::Taxable) => "federally taxable".to_string(),
                        _ => String::new(),
                    },
                    flag(muni.subject_to_amt, "subject to AMT"),
                    flag(muni.state_tax_exempt, "state tax-exempt"),
                    flag(muni.bank_qualified, "bank qualified"),
                    labelled_html("obligor", company(&muni.obligor_company_id, names)),
                    labelled_html("insured by", company(&muni.insurer_company_id, names)),
                    match RefundingStatus::try_from(muni.refunding_status) {
                        Ok(RefundingStatus::PreRefunded) => escape(&joined(
                            [
                                "pre-refunded".to_string(),
                                labelled("to", &muni.refunded_to_date),
                            ],
                            " ",
                        )),
                        Ok(RefundingStatus::EscrowedToMaturity) => {
                            "escrowed to maturity".to_string()
                        }
                        _ => String::new(),
                    },
                ],
                ", ",
            ),
        });
    }
    for step in &bond.coupon_steps {
        lines.push(Line {
            label: "Coupon step",
            value: escape(&format!(
                "from {}: {}%",
                step.from_date,
                exact(step.coupon_rate.as_ref())
            )),
        });
    }
    lines.extend(bond.calls.iter().map(|call| Line {
        label: "Call",
        value: window(call),
    }));
    lines.extend(bond.puts.iter().map(|put| Line {
        label: "Put",
        value: window(put),
    }));
    lines.extend(bond.sinking_funds.iter().map(|fund| Line {
        label: "Sinking fund",
        value: escape(&format!(
            "{}: {} at {}",
            fund.date,
            exact(fund.face_amount.as_ref()),
            match exact(fund.price.as_ref()) {
                price if price.is_empty() => "par".to_string(),
                price => price,
            }
        )),
    }));
    lines
}

fn agency(value: i32) -> String {
    MortgageAgency::try_from(value)
        .ok()
        .map(|known| value_words(known.as_str_name(), "MORTGAGE_AGENCY_", true))
        .unwrap_or_default()
}

fn pool_lines(pool: &MbsPool) -> Vec<Line> {
    vec![
        Line {
            label: "Pool",
            value: escape(&joined(
                [
                    agency(pool.agency),
                    pool.program.clone(),
                    pool.pool_number.clone(),
                ],
                " ",
            )),
        },
        Line {
            label: "Pass-through rate",
            value: percent(pool.pass_through_rate.as_ref()),
        },
        Line {
            label: "Dates",
            value: escape(&joined(
                [
                    labelled("issued", &pool.issue_date),
                    labelled("matures", &pool.maturity_date),
                    match pool.original_term_months {
                        0 => String::new(),
                        months => format!("{months}-month term"),
                    },
                ],
                ", ",
            )),
        },
        Line {
            label: "Original face",
            value: exact(pool.original_face.as_ref()),
        },
        Line {
            label: "Day count",
            value: day_count(pool.day_count),
        },
        Line {
            label: "Payment",
            value: joined(
                [
                    match pool.payment_delay_days {
                        0 => String::new(),
                        days => format!("{days}-day delay"),
                    },
                    match pool.payment_day {
                        0 => String::new(),
                        day => format!("paid on the {day}"),
                    },
                    record_date(pool.record_date_rule, 0),
                    conventions(0, &pool.payment_calendar_id, false),
                ],
                ", ",
            ),
        },
    ]
}

fn tba_lines(tba: &Tba) -> Vec<Line> {
    vec![
        Line {
            label: "TBA",
            value: escape(&joined(
                [
                    agency(tba.agency),
                    tba.program.clone(),
                    exact(tba.pass_through_rate.as_ref()),
                    match tba.original_term_months {
                        0 => String::new(),
                        months => format!("{}-year", months / 12),
                    },
                ],
                " ",
            )),
        },
        Line {
            label: "Settles",
            value: escape(&joined(
                [
                    labelled("month", &tba.settlement_month),
                    match SifmaSettlementClass::try_from(tba.sifma_class) {
                        Ok(class) if tba.sifma_class != 0 => format!(
                            "SIFMA class {}",
                            value_words(class.as_str_name(), "SIFMA_SETTLEMENT_CLASS_", true)
                        ),
                        _ => String::new(),
                    },
                    labelled("on", &tba.settlement_date),
                ],
                ", ",
            )),
        },
    ]
}

fn tranche_lines(tranche: &CmoTranche) -> Vec<Line> {
    let types = |values: &[i32], name: fn(i32) -> String| {
        joined(values.iter().map(|value| name(*value)), " and ")
    };
    vec![
        Line {
            label: "Class",
            value: escape(&joined(
                [
                    agency(tranche.agency),
                    tranche.deal.clone(),
                    labelled("class", &tranche.class_name),
                ],
                " ",
            )),
        },
        Line {
            label: "Types",
            value: escape(&joined(
                [
                    types(&tranche.principal_types, |value| {
                        PrincipalType::try_from(value)
                            .ok()
                            .map(|t| value_words(t.as_str_name(), "PRINCIPAL_TYPE_", true))
                            .unwrap_or_default()
                    }),
                    types(&tranche.interest_types, |value| {
                        InterestType::try_from(value)
                            .ok()
                            .map(|t| value_words(t.as_str_name(), "INTEREST_TYPE_", true))
                            .unwrap_or_default()
                    }),
                ],
                "; ",
            )),
        },
        Line {
            label: "Coupon",
            value: match &tranche.floating {
                Some(terms) => floater(terms),
                None => percent(tranche.coupon_rate.as_ref()),
            },
        },
        Line {
            label: "Original face",
            value: exact(tranche.original_face.as_ref()),
        },
        Line {
            label: "Dates",
            value: escape(&joined(
                [
                    labelled("issued", &tranche.issue_date),
                    labelled("matures", &tranche.maturity_date),
                ],
                ", ",
            )),
        },
        Line {
            label: "Day count",
            value: day_count(tranche.day_count),
        },
        Line {
            label: "Payment",
            value: joined(
                [
                    match tranche.payment_delay_days {
                        0 => "no delay".to_string(),
                        days => format!("{days}-day delay"),
                    },
                    match tranche.payment_day {
                        0 => String::new(),
                        day => format!("paid on the {day}"),
                    },
                    record_date(tranche.record_date_rule, tranche.record_date_days),
                ],
                ", ",
            ),
        },
    ]
}

fn cd_lines(cd: &CertificateOfDeposit, names: &BTreeMap<String, String>) -> Vec<Line> {
    let mut lines = vec![
        Line {
            label: "Issuer",
            value: company(&cd.issuer_company_id, names),
        },
        Line {
            label: "Coupon",
            value: joined(
                [
                    percent(cd.coupon_rate.as_ref()),
                    match cd.coupon_frequency {
                        0 => "paid at maturity".to_string(),
                        n => per_year(n),
                    },
                ],
                ", ",
            ),
        },
        Line {
            label: "Day count",
            value: day_count(cd.day_count),
        },
        Line {
            label: "Dates",
            value: escape(&joined(
                [
                    labelled("issued", &cd.issue_date),
                    labelled("matures", &cd.maturity_date),
                ],
                ", ",
            )),
        },
        Line {
            label: "Conventions",
            value: conventions(cd.business_day_convention, &cd.payment_calendar_id, false),
        },
        Line {
            label: "Deposit",
            value: joined(
                [
                    labelled_value("minimum", exact(cd.minimum_denomination.as_ref())),
                    flag(cd.deposit_insured, "insured"),
                    flag(cd.survivor_option, "survivor option"),
                ],
                ", ",
            ),
        },
    ];
    lines.extend(cd.calls.iter().map(|call| Line {
        label: "Call",
        value: window(call),
    }));
    lines
}

/// A line's HTML as its text, for a cell's title: its links' tags left out,
/// its escaping kept.
fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn percent(decimal: Option<&Decimal>) -> String {
    match exact(decimal) {
        value if value.is_empty() => String::new(),
        value => format!("{value}%"),
    }
}

fn flag(set: bool, words: &str) -> String {
    if set {
        words.to_string()
    } else {
        String::new()
    }
}

fn labelled(label: &str, value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        format!("{label} {value}")
    }
}

fn labelled_value(label: &str, value: String) -> String {
    labelled(label, &value)
}

fn labelled_html(label: &str, html: String) -> String {
    if html.is_empty() {
        String::new()
    } else {
        format!("{label} {html}")
    }
}

/// The record's terms as one-line rows, paged; empty for a record that
/// carries none.
pub fn section(record: &InstrumentRecord, names: &BTreeMap<String, String>) -> String {
    let lines = if let Some(bond) = &record.bond {
        bond_lines(bond, names)
    } else if let Some(pool) = &record.mbs_pool {
        pool_lines(pool)
    } else if let Some(tba) = &record.tba {
        tba_lines(tba)
    } else if let Some(tranche) = &record.cmo_tranche {
        tranche_lines(tranche)
    } else if let Some(cd) = &record.certificate_of_deposit {
        cd_lines(cd, names)
    } else {
        return String::new();
    };
    // The kit's facts (0.12.0): the label a row header, the value on one
    // line with its whole text on hover, and the whole term a click away
    // in the row's detail.
    let rows: String = lines
        .iter()
        .filter(|line| !line.value.is_empty())
        .map(|line| {
            format!(
                "<tr><th scope=\"row\">{label}</th><td title=\"{plain}\">{value}</td>\
                 <td class=\"more\"><details class=\"row-detail\" name=\"term\">\
                 <summary aria-label=\"{label}, whole\">…</summary>\
                 <div class=\"row-detail-pop\"><h3>{label}</h3><p>{value}</p></div></details></td></tr>",
                label = line.label,
                value = line.value,
                plain = plain(&line.value),
            )
        })
        .collect();
    let lacking = if record.missing_fields.is_empty() {
        String::new()
    } else {
        format!(
            " Not yet stated: {}.",
            escape(&record.missing_fields.join(", "))
        )
    };
    format!(
        "<section class=\"admin-section\" id=\"terms\"><div class=\"section-head\"><div><h2>Terms</h2>\
         <p class=\"hint\">As the platform's record states them, version {version}.{lacking}</p></div></div>\
         <om-pager><table class=\"list one-line terms\"><tbody>{rows}</tbody></table></om-pager></section>",
        version = record.version,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{Bond, DayCount};

    fn decimal(text: &str) -> Option<Decimal> {
        Some(text.parse::<Exact>().unwrap().to_wire())
    }

    /// A term's row as the page draws it: its label, then its value with
    /// its text as the cell's title.
    fn row(label: &str, value: &str) -> String {
        format!(
            "<th scope=\"row\">{label}</th><td title=\"{}\">{value}</td>",
            plain(value)
        )
    }

    #[test]
    fn a_bonds_terms_are_one_line_each_its_issuer_by_name_and_its_calls_each_a_row() {
        let bond = Bond {
            security_type: DebtSecurityType::Corp as i32,
            issuer_company_id: "ENT-AXP".into(),
            issuer_type: IssuerType::Corporate as i32,
            country_of_issue: "US".into(),
            issue_date: "2026-02-05".into(),
            maturity_date: "2036-02-05".into(),
            coupon_rate: decimal("4.875"),
            coupon_type: CouponType::Fixed as i32,
            coupon_frequency: 2,
            day_count: DayCount::Thirty360BondBasis as i32,
            calls: vec![
                RedemptionWindow {
                    kind: RedemptionKind::MakeWhole as i32,
                    from_date: "2026-02-05".into(),
                    to_date: "2035-11-05".into(),
                    make_whole_spread_bps: decimal("20"),
                    make_whole_benchmark_instrument_id: "INS-H15".into(),
                    style: RedemptionStyle::Continuous as i32,
                    ..Default::default()
                },
                RedemptionWindow {
                    kind: RedemptionKind::Par as i32,
                    from_date: "2035-11-05".into(),
                    price: decimal("100"),
                    notice_days_min: 10,
                    notice_days_max: 60,
                    style: RedemptionStyle::Continuous as i32,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let record = InstrumentRecord {
            instrument_id: "INS-AXP2036".into(),
            version: 2,
            bond: Some(bond),
            missing_fields: vec!["dated_date".into()],
            ..Default::default()
        };
        assert_eq!(company_ids(&record), vec!["ENT-AXP".to_string()]);
        let names = BTreeMap::from([("ENT-AXP".to_string(), "American Express Co".to_string())]);
        let html = section(&record, &names);
        assert!(html.contains(&row("Issuer", "American Express Co (ENT-AXP), corporate")));
        assert!(html.contains(&row("Security type", "CORP")));
        assert!(html.contains(&row("Coupon", "4.875%, fixed, semi-annual")));
        assert!(html.contains(&row("Day count", "30/360 (bond basis)")));
        assert!(html.contains(&row(
            "Call",
            "make whole 2026-02-05 to 2035-11-05, 20 bp over \
             <a href=\"/admin/instruments/INS-H15\">INS-H15</a>, continuous"
        )));
        assert!(html.contains(
            "title=\"make whole 2026-02-05 to 2035-11-05, 20 bp over INS-H15, continuous\""
        ));
        assert!(html.contains(&row(
            "Call",
            "par from 2035-11-05, at 100, notice 10 to 60 days, continuous"
        )));
        assert!(html.contains("Not yet stated: dated_date."));
        // A term left unstated is no row at all.
        assert!(!html.contains("Guarantor"));
        assert!(!html.contains("Stripped from"));
        // An issuer the store holds no record of is named by its ID.
        assert!(section(&record, &BTreeMap::new()).contains(">ENT-AXP, corporate</td>"));
        // A record with no terms has no section.
        assert!(section(&InstrumentRecord::default(), &names).is_empty());
    }

    #[test]
    fn a_pool_a_tba_a_tranche_and_a_cd_each_say_their_terms() {
        let pool = InstrumentRecord {
            mbs_pool: Some(MbsPool {
                agency: MortgageAgency::Fnma as i32,
                program: "CL".into(),
                pool_number: "MA5123".into(),
                pass_through_rate: decimal("5.5"),
                payment_delay_days: 55,
                payment_day: 25,
                ..Default::default()
            }),
            ..Default::default()
        };
        let html = section(&pool, &BTreeMap::new());
        assert!(html.contains(&row("Pool", "FNMA CL MA5123")));
        assert!(html.contains(&row("Payment", "55-day delay, paid on the 25")));
        let tba = InstrumentRecord {
            tba: Some(Tba {
                agency: MortgageAgency::Fnma as i32,
                program: "UMBS".into(),
                pass_through_rate: decimal("5.5"),
                original_term_months: 360,
                settlement_month: "202612".into(),
                sifma_class: SifmaSettlementClass::A as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        let html = section(&tba, &BTreeMap::new());
        assert!(html.contains(&row("TBA", "FNMA UMBS 5.5 30-year")));
        assert!(html.contains(&row("Settles", "month 202612, SIFMA class A")));
        let tranche = InstrumentRecord {
            cmo_tranche: Some(CmoTranche {
                agency: MortgageAgency::Fhlmc as i32,
                deal: "5300".into(),
                class_name: "FA".into(),
                principal_types: vec![PrincipalType::Pac as i32],
                interest_types: vec![InterestType::Floater as i32],
                ..Default::default()
            }),
            ..Default::default()
        };
        let html = section(&tranche, &BTreeMap::new());
        assert!(html.contains(&row("Types", "PAC; FLOATER")));
        assert!(html.contains(&row("Payment", "no delay")));
        let cd = InstrumentRecord {
            certificate_of_deposit: Some(CertificateOfDeposit {
                issuer_company_id: "ENT-BANK".into(),
                coupon_rate: decimal("4.1"),
                deposit_insured: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let html = section(&cd, &BTreeMap::new());
        assert!(html.contains(&row("Coupon", "4.1%, paid at maturity")));
        assert!(html.contains(&row("Deposit", "insured")));
    }
}
