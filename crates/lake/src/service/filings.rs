//! The filings slice (contract v28; FDP §1 items 6 to 11, ruling 568; W10.4
//! to W10.6): filings, reported facts and rating actions, and -- the
//! guardrail's second move-out (557), kept apart below so they can be held
//! back -- holdings reports and their holdings.
//!
//! Each row names its subjects in roles: a company by its platform record
//! (`ENT-`, W3.32), held by the instrument store, in a role its kind gives a
//! company -- a filing's filer, a fact's filer, a rating action's `rater`
//! and the issuer it rates, a report's holder, a holding's holder and, on a
//! 13D or 13G, the issuer -- and a security by its `INS-` record. Each is
//! dated by its own fields, never a business date: a filing by its filed
//! date, a fact by its period's end, a rating action by its valid time's
//! day, a report by its report date, a holding by its report's. A filing is
//! published every version, never conflated, as a corporate action is;
//! facts, rating actions and holdings are read, not heard (W10.5). Each is
//! read point in time, by recorded time or by what the source had published
//! (v27's published-time read), by subject and role, dates, and kind where
//! the kind has one; a filing also caught up after a watermark.

use meridian_domain::v1::{
    FilingKind, FilingRef, HoldingsReport, HoldingsReportKind, ListFilingsReply,
    ListFilingsRequest, ListHoldingsReply, ListHoldingsRequest, ListRatingActionsReply,
    ListRatingActionsRequest, ListReportedFactsReply, ListReportedFactsRequest, RatingActionKind,
    RecordFilingsRequest, RecordHoldingsReportsRequest, RecordHoldingsRequest,
    RecordRatingActionsRequest, RecordReportedFactsRequest, UnitValue,
};

use super::*;

/// The roles a company takes on a row of each kind (contract v28, W10.4;
/// FDP §2): who filed, who holds, who issued, an insider reporting, what a
/// row is about, and the agency that rates (FDP Q7).
fn company_roles(data_type: DataType) -> &'static [SubjectRole] {
    use SubjectRole::*;
    match data_type {
        DataType::CorporateAction => &[Issuer],
        DataType::Filing => &[Filer, Subject, Issuer, Holder, ReportingOwner],
        DataType::ReportedFact => &[Filer, Subject],
        DataType::RatingAction => &[Subject, Rater],
        DataType::HoldingsReport => &[Holder, Filer],
        DataType::Holding => &[Holder, Issuer],
        _ => &[],
    }
}

/// The roles a security takes on a row of each kind: what is held, or an
/// issue rated (`held`); what the row is about. Empty: any role, as a
/// corporate action's security has taken since v28.
fn security_roles(data_type: DataType) -> &'static [SubjectRole] {
    use SubjectRole::*;
    match data_type {
        DataType::Filing | DataType::ReportedFact | DataType::HoldingsReport => {
            &[Unspecified, Subject]
        }
        DataType::RatingAction | DataType::Holding => &[Held],
        _ => &[],
    }
}

fn role_of(subject: &SubjectRef) -> SubjectRole {
    SubjectRole::try_from(subject.role).unwrap_or(SubjectRole::Unspecified)
}

fn roles_said(roles: &[SubjectRole]) -> String {
    roles
        .iter()
        .map(|role| role.as_str_name())
        .collect::<Vec<_>>()
        .join(", ")
}

fn a_row_of(data_type: DataType) -> &'static str {
    match data_type {
        DataType::CorporateAction => "a corporate action",
        DataType::Filing => "a filing",
        DataType::ReportedFact => "a reported fact",
        DataType::RatingAction => "a rating action",
        DataType::HoldingsReport => "a holdings report",
        _ => "a holding",
    }
}

impl Lake {
    /// A row of the filings slice's subjects held to W10.4: each a company
    /// the deployment holds, in a role its kind gives a company, or a record
    /// it holds, in a role its kind gives a security; and what its kind must
    /// name -- a corporate action its security, a filing a company, a fact
    /// its filer, a rating action one `rater` and what it rates, a report its
    /// holder, a holding its holder and what is held.
    pub(super) fn filings_subjects_refused(
        &self,
        at: &str,
        data_type: DataType,
        subjects: &[SubjectRef],
    ) -> Result<(), String> {
        let company = |subject: &SubjectRef| subject.entity_id.starts_with(COMPANY_PREFIX);
        for (j, subject) in subjects.iter().enumerate() {
            if !(1..=64).contains(&subject.entity_id.chars().count()) {
                return Err(format!(
                    "{at}.meta.subjects[{j}].entity_id is 1 to 64 characters"
                ));
            }
            let role = role_of(subject);
            if company(subject) {
                let roles = company_roles(data_type);
                if !roles.contains(&role) {
                    return Err(format!(
                        "{at}.meta.subjects[{j}].role: a company names {}'s {}",
                        a_row_of(data_type),
                        roles_said(roles)
                    ));
                }
                if !self.company_held(&subject.entity_id)? {
                    return Err(format!(
                        "{at}.meta.subjects[{j}].entity_id {:?} is no company the deployment \
                         holds; resolve it (W3.32) first",
                        subject.entity_id
                    ));
                }
                continue;
            }
            let roles = security_roles(data_type);
            if !roles.is_empty() && !roles.contains(&role) {
                return Err(format!(
                    "{at}.meta.subjects[{j}].role: a security names {}'s {}",
                    a_row_of(data_type),
                    roles_said(roles)
                ));
            }
            if self.subject_record(&subject.entity_id)?.is_none() {
                return Err(format!(
                    "{at}.meta.subjects[{j}].entity_id {:?} is no record the deployment holds",
                    subject.entity_id
                ));
            }
        }
        let a_company_in = |roles: &[SubjectRole]| {
            subjects
                .iter()
                .any(|s| company(s) && roles.contains(&role_of(s)))
        };
        let a_security_in = |roles: &[SubjectRole]| {
            subjects
                .iter()
                .any(|s| !company(s) && roles.contains(&role_of(s)))
        };
        let missing = match data_type {
            DataType::CorporateAction if !subjects.iter().any(|s| !company(s)) => Some(
                "names no security: a corporate action's subject is the security, its issuer \
                 beside it where stated"
                    .to_string(),
            ),
            DataType::Filing if !subjects.iter().any(company) => {
                Some("names no company: a filing names its filer".to_string())
            }
            DataType::ReportedFact
                if !a_company_in(&[SubjectRole::Filer, SubjectRole::Subject]) =>
            {
                Some("names no filer: a fact is its filer's, a company".to_string())
            }
            DataType::RatingAction => {
                let raters = subjects
                    .iter()
                    .filter(|s| company(s) && role_of(s) == SubjectRole::Rater)
                    .count();
                if raters == 0 {
                    Some(
                        "names no rater: the agency is a company in the role \
                         SUBJECT_ROLE_RATER (FDP Q7)"
                            .to_string(),
                    )
                } else if raters > 1 {
                    Some(format!("names {raters} raters; one"))
                } else if !a_company_in(&[SubjectRole::Subject])
                    && !a_security_in(&[SubjectRole::Held])
                {
                    Some(
                        "names nothing rated: the issuer as a company in the role \
                         SUBJECT_ROLE_SUBJECT, or the issue's security in the role \
                         SUBJECT_ROLE_HELD"
                            .to_string(),
                    )
                } else {
                    None
                }
            }
            DataType::HoldingsReport if !a_company_in(&[SubjectRole::Holder]) => {
                Some("names no holder: a report is its holder's, a company".to_string())
            }
            DataType::Holding if !a_company_in(&[SubjectRole::Holder]) => {
                Some("names no holder: a holding is its report's holder's".to_string())
            }
            DataType::Holding
                if !a_security_in(&[SubjectRole::Held])
                    && !a_company_in(&[SubjectRole::Issuer]) =>
            {
                Some(
                    "names nothing held: the security in the role SUBJECT_ROLE_HELD, or on a \
                     13D or 13G the issuer, a company in the role SUBJECT_ROLE_ISSUER"
                        .to_string(),
                )
            }
            _ => None,
        };
        match missing {
            Some(why) => Err(format!("{at}.meta.subjects {why}")),
            None => Ok(()),
        }
    }
}

/// A filing's accession and form, each 1 to 32 characters (W10.4: a filing
/// with no accession is refused).
fn filing_ref_refused(at: &str, filing: Option<&FilingRef>) -> Result<(), String> {
    let filing = filing.cloned().unwrap_or_default();
    if filing.accession_number.trim().is_empty()
        || !bounds::FILING_REF_ACCESSION_NUMBER_LENGTH
            .admits(filing.accession_number.chars().count())
    {
        return Err(format!(
            "{at}.filing.accession_number is 1 to 32 characters: the filing's accession"
        ));
    }
    if filing.form.trim().is_empty()
        || !bounds::FILING_REF_FORM_LENGTH.admits(filing.form.chars().count())
    {
        return Err(format!(
            "{at}.filing.form is 1 to 32 characters: the form as reported"
        ));
    }
    Ok(())
}

/// A required plain date.
fn date_required(path: &str, text: &str, why: &str) -> Result<Date, String> {
    meridian_domain::date::optional(path, text)?.ok_or_else(|| format!("{path} is required: {why}"))
}

/// A value with its unit (v27's `UnitValue`): a number, and a unit named.
fn unit_value_refused(path: &str, value: &UnitValue) -> Result<(), String> {
    let Some(number) = value.value.as_ref() else {
        return Err(format!("{path}.value is required"));
    };
    meridian_domain::exact::Exact::from_wire(number)
        .map_err(|out| format!("{path}.value {out}"))?;
    let unit = ValueUnit::try_from(value.unit).unwrap_or(ValueUnit::Unspecified);
    if unit == ValueUnit::Unspecified {
        return Err(format!("{path}.unit is unspecified"));
    }
    if unit == ValueUnit::Ucum && value.ucum_code.trim().is_empty() {
        return Err(format!("{path}.ucum_code names the physical unit"));
    }
    Ok(())
}

/// A filing held to W10.4: its accession and form, its kind from the
/// platform's list, its items as reported, its period of report where the
/// form states one and its filed date, each a plain date.
pub(super) fn filing_refused(at: &str, filing: &meridian_domain::v1::Filing) -> Result<(), String> {
    filing_ref_refused(at, filing.filing.as_ref())?;
    if FilingKind::try_from(filing.kind).unwrap_or(FilingKind::Unspecified)
        == FilingKind::Unspecified
    {
        return Err(format!("{at}.kind is unspecified"));
    }
    for (i, item) in filing.items.iter().enumerate() {
        if item.trim().is_empty() || item.chars().count() > 32 {
            return Err(format!(
                "{at}.items[{i}] is an item as reported, 2.02, 1 to 32 characters"
            ));
        }
    }
    meridian_domain::date::optional(&format!("{at}.period_of_report"), &filing.period_of_report)?;
    date_required(
        &format!("{at}.filed_date"),
        &filing.filed_date,
        "the date it was filed; its acceptance is the published source time",
    )?;
    Ok(())
}

/// A reported fact held to W10.4: its concept, taxonomy and name; an amount
/// or a value, never neither and never both; its period, an instant its end
/// alone; its dimensions as reported; its filing.
pub(super) fn reported_fact_refused(
    at: &str,
    fact: &meridian_domain::v1::ReportedFact,
) -> Result<(), String> {
    if !bounds::REPORTED_FACT_CONCEPT_LENGTH.admits(fact.concept.chars().count())
        || !fact
            .concept
            .split_once(':')
            .is_some_and(|(taxonomy, name)| !taxonomy.trim().is_empty() && !name.trim().is_empty())
    {
        return Err(format!(
            "{at}.concept is its taxonomy and name, us-gaap:EarningsPerShareBasic, 1 to 256 \
             characters"
        ));
    }
    match (&fact.amount, &fact.value) {
        (None, None) => {
            return Err(format!(
                "{at}.amount is required, or value: a fact with neither is refused"
            ))
        }
        (Some(_), Some(_)) => {
            return Err(format!(
                "{at}.value is unset where amount is set: a fact is one or the other"
            ))
        }
        (Some(amount), None) if amount.amount.is_none() => {
            return Err(format!("{at}.amount.amount is required"))
        }
        (None, Some(value)) => unit_value_refused(&format!("{at}.value"), value)?,
        _ => {}
    }
    let end = date_required(
        &format!("{at}.period_end"),
        &fact.period_end,
        "the period's end, or the instant",
    )?;
    if let Some(start) =
        meridian_domain::date::optional(&format!("{at}.period_start"), &fact.period_start)?
    {
        if start > end {
            return Err(format!("{at}.period_start is on or before period_end"));
        }
    }
    if !bounds::REPORTED_FACT_DIMENSIONS_COUNT.admits(fact.dimensions.len()) {
        return Err(format!(
            "{at}.dimensions names {}; at most 16",
            fact.dimensions.len()
        ));
    }
    for (i, dimension) in fact.dimensions.iter().enumerate() {
        if dimension.axis.trim().is_empty() {
            return Err(format!(
                "{at}.dimensions[{i}].axis is required, as reported"
            ));
        }
        if dimension.member.trim().is_empty() {
            return Err(format!(
                "{at}.dimensions[{i}].member is required, as reported"
            ));
        }
    }
    filing_ref_refused(at, fact.filing.as_ref())
}

/// A rating action held to W10.4: the agency's symbol, empty only on a
/// withdrawal; the 17g-7 rating type as reported; the action.
pub(super) fn rating_action_refused(
    at: &str,
    action: &meridian_domain::v1::RatingAction,
) -> Result<(), String> {
    let kind = RatingActionKind::try_from(action.action).unwrap_or(RatingActionKind::Unspecified);
    if kind == RatingActionKind::Unspecified {
        return Err(format!("{at}.action is unspecified"));
    }
    if !bounds::RATING_ACTION_RATING_LENGTH.admits(action.rating.chars().count()) {
        return Err(format!("{at}.rating is at most 32 characters"));
    }
    if action.rating.trim().is_empty() && kind != RatingActionKind::Withdrawn {
        return Err(format!(
            "{at}.rating is the agency's symbol, AA+ or Baa2; empty only on a withdrawal"
        ));
    }
    if action.rating_class.trim().is_empty()
        || !bounds::RATING_ACTION_RATING_CLASS_LENGTH.admits(action.rating_class.chars().count())
    {
        return Err(format!(
            "{at}.rating_class is the 17g-7 rating type as reported, 1 to 64 characters"
        ));
    }
    Ok(())
}

/// A holdings report held to W10.4: its kind, its report date, its filing.
pub(super) fn holdings_report_refused(at: &str, report: &HoldingsReport) -> Result<(), String> {
    if HoldingsReportKind::try_from(report.kind).unwrap_or(HoldingsReportKind::Unspecified)
        == HoldingsReportKind::Unspecified
    {
        return Err(format!("{at}.kind is unspecified"));
    }
    date_required(
        &format!("{at}.report_date"),
        &report.report_date,
        "a 13F's quarter end, a 13D's event date",
    )?;
    filing_ref_refused(at, report.filing.as_ref())
}

/// A holding held to W10.4: its report named (a holding naming no report is
/// refused), its quantity and percent of class each a value with its unit.
pub(super) fn holding_refused(
    at: &str,
    holding: &meridian_domain::v1::Holding,
) -> Result<(), String> {
    if holding.report_row_key.trim().is_empty() || holding.report_row_key.chars().count() > 128 {
        return Err(format!(
            "{at}.report_row_key is required: the HoldingsReport's row key, 1 to 128 characters"
        ));
    }
    if let Some(quantity) = &holding.quantity {
        unit_value_refused(&format!("{at}.quantity"), quantity)?;
    }
    if let Some(percent) = &holding.percent_of_class {
        unit_value_refused(&format!("{at}.percent_of_class"), percent)?;
    }
    Ok(())
}

/// A read's dates on its kind's date (W10.6): from and to, each a plain date
/// and inclusive, either open; `None` for neither.
fn dates_of(from_date: &str, to_date: &str) -> Result<Option<(Date, Date)>, String> {
    if from_date.is_empty() && to_date.is_empty() {
        return Ok(None);
    }
    let from =
        meridian_domain::date::optional("from_date", from_date)?.unwrap_or(Date::of_utc_instant(0));
    let to = match meridian_domain::date::optional("to_date", to_date)? {
        Some(to) => to,
        None => Date::from_ymd(9999, 12, 31).expect("the last day"),
    };
    if to < from {
        return Err("to_date is on or after from_date".into());
    }
    Ok(Some((from, to)))
}

/// What a read of the filings slice asks (W10.6), before it is read.
pub(super) struct Asked {
    pub data_type: DataType,
    pub subjects: Vec<SubjectRef>,
    pub subjects_bound: bounds::Count,
    pub kinds: Vec<i32>,
    pub from_date: String,
    pub to_date: String,
    pub published_as_of_ns: i64,
    pub sources: Option<SourceChoice>,
    pub as_of_ns: i64,
    pub page_size: u32,
    pub cursor: String,
    pub after: Option<Watermark>,
}

impl Asked {
    /// The read W10.6 says, or its refusal naming the field: 1 to 64
    /// subjects; each kind named; its dates plain dates, from on or before
    /// to; a catch-up after a watermark whatever the dates (ruling 598); no
    /// published-time cut below 0.
    pub(super) fn request(self) -> Result<ReadRequest, String> {
        if !self.subjects_bound.admits(self.subjects.len()) {
            return Err(format!("subjects names {}; 1 to 64", self.subjects.len()));
        }
        for (i, kind) in self.kinds.iter().enumerate() {
            if *kind == 0 {
                return Err(format!("kinds[{i}] is unspecified"));
            }
        }
        let caught_up = self
            .after
            .as_ref()
            .is_some_and(|watermark| !watermark.partitions.is_empty());
        let dates = dates_of(&self.from_date, &self.to_date)?;
        if caught_up && dates.is_some() {
            return Err(
                "after_watermark and from_date or to_date: a catch-up reads whatever the dates"
                    .into(),
            );
        }
        if self.published_as_of_ns < 0 {
            return Err("published_as_of_ns is never negative".into());
        }
        Ok(ReadRequest {
            data_type: self.data_type,
            subjects: self.subjects,
            kinds: self.kinds,
            interval_ns: 0,
            sources: self.sources,
            at_ns: 0,
            business_date: String::new(),
            valid_from_ns: 0,
            valid_until_ns: 0,
            as_of_ns: self.as_of_ns,
            page_size: self.page_size,
            cursor: self.cursor,
            after: self.after,
            tenors: Vec::new(),
            published_as_of_ns: self.published_as_of_ns,
            series_keys: Vec::new(),
            all_series: false,
            dates,
        })
    }
}

impl Lake {
    /// W10.6, contract v28 (the filings slice): the filings of 1 to 64
    /// subjects -- a filer, or a company or security in the role asked -- by
    /// kind and filed date, point in time by recorded or published time; or
    /// every version recorded after a watermark, whatever the dates. An
    /// 8-K's items say which dates an earnings report (2.02).
    pub(crate) fn list_filings(
        &self,
        request: ListFilingsRequest,
        envelope: &Envelope,
    ) -> Result<ListFilingsReply, String> {
        let asked = Asked {
            data_type: DataType::Filing,
            subjects: request.subjects,
            subjects_bound: bounds::LIST_FILINGS_REQUEST_SUBJECTS_COUNT,
            kinds: request.kinds,
            from_date: request.from_date,
            to_date: request.to_date,
            published_as_of_ns: request.published_as_of_ns,
            sources: request.sources,
            as_of_ns: request.as_of_ns,
            page_size: request.page_size,
            cursor: request.cursor,
            after: request.after_watermark,
        };
        let answer = self.read(asked.request()?, envelope)?;
        Ok(ListFilingsReply {
            filings: answer
                .rows
                .into_iter()
                .filter_map(|row| match row {
                    Observation::Filing(filing) => Some(filing),
                    _ => None,
                })
                .collect(),
            unanswered: answer.unanswered,
            datasets: answer.datasets,
            watermark: Some(answer.watermark),
            next_cursor: answer.next_cursor,
        })
    }

    /// W10.6, contract v28: the facts of 1 to 64 subjects, by concept and
    /// period end, each row key at its version in force -- the latest, or
    /// with `published_as_of_ns` the one first published by then.
    pub(crate) fn list_reported_facts(
        &self,
        request: ListReportedFactsRequest,
        envelope: &Envelope,
    ) -> Result<ListReportedFactsReply, String> {
        for (i, concept) in request.concepts.iter().enumerate() {
            if concept.trim().is_empty()
                || !bounds::REPORTED_FACT_CONCEPT_LENGTH.admits(concept.chars().count())
            {
                return Err(format!(
                    "concepts[{i}] is a taxonomy and name, 1 to 256 characters"
                ));
            }
        }
        let asked = Asked {
            data_type: DataType::ReportedFact,
            subjects: request.subjects,
            subjects_bound: bounds::LIST_REPORTED_FACTS_REQUEST_SUBJECTS_COUNT,
            kinds: Vec::new(),
            from_date: request.from_date,
            to_date: request.to_date,
            published_as_of_ns: request.published_as_of_ns,
            sources: request.sources,
            as_of_ns: request.as_of_ns,
            page_size: request.page_size,
            cursor: request.cursor,
            after: None,
        };
        let answer = self.read_within(
            reader_of(envelope),
            asked.request()?,
            true,
            read::Narrowed {
                concepts: request.concepts.into_iter().collect(),
                reports: None,
            },
        )?;
        Ok(ListReportedFactsReply {
            facts: answer
                .rows
                .into_iter()
                .filter_map(|row| match row {
                    Observation::ReportedFact(fact) => Some(fact),
                    _ => None,
                })
                .collect(),
            unanswered: answer.unanswered,
            datasets: answer.datasets,
            watermark: Some(answer.watermark),
            next_cursor: answer.next_cursor,
        })
    }

    /// W10.6, contract v28: the rating actions of 1 to 64 subjects -- the
    /// issuer rated, an issue rated, or the agency in the role `rater` -- by
    /// action and the action's date.
    pub(crate) fn list_rating_actions(
        &self,
        request: ListRatingActionsRequest,
        envelope: &Envelope,
    ) -> Result<ListRatingActionsReply, String> {
        let asked = Asked {
            data_type: DataType::RatingAction,
            subjects: request.subjects,
            subjects_bound: bounds::LIST_RATING_ACTIONS_REQUEST_SUBJECTS_COUNT,
            kinds: request.kinds,
            from_date: request.from_date,
            to_date: request.to_date,
            published_as_of_ns: request.published_as_of_ns,
            sources: request.sources,
            as_of_ns: request.as_of_ns,
            page_size: request.page_size,
            cursor: request.cursor,
            after: None,
        };
        let answer = self.read(asked.request()?, envelope)?;
        Ok(ListRatingActionsReply {
            actions: answer
                .rows
                .into_iter()
                .filter_map(|row| match row {
                    Observation::RatingAction(action) => Some(action),
                    _ => None,
                })
                .collect(),
            unanswered: answer.unanswered,
            datasets: answer.datasets,
            watermark: Some(answer.watermark),
            next_cursor: answer.next_cursor,
        })
    }
}

// ── Holdings: the guardrail's second move-out (557) ──

impl Lake {
    /// The holdings reports a reader may read, by dataset and row key, each
    /// at its version in force as of a recorded time -- or, given a
    /// published-time cut, the one in force by then -- stripped to the
    /// fields it may read; one whose state is invalid left out, as a default
    /// read leaves it, unless the read includes the quarantined.
    fn reports_of(
        &self,
        reader: &Reader,
        as_of_ns: i64,
        published_as_of_ns: i64,
        include_quarantined: bool,
    ) -> Result<BTreeMap<(String, String), HoldingsReport>, String> {
        let config = self.config();
        let mut reports = BTreeMap::new();
        for dataset in config.readable(reader, DataType::HoldingsReport) {
            let rows = if published_as_of_ns != 0 {
                let every = self
                    .store
                    .read_dataset_versions(&dataset, DataType::HoldingsReport)
                    .map_err(|failed| failed.to_string())?;
                read::as_published(
                    crate::store::recorded_by(every, as_of_ns),
                    published_as_of_ns,
                )
            } else {
                self.store
                    .read_dataset(&dataset, DataType::HoldingsReport, as_of_ns)
                    .map_err(|failed| failed.to_string())?
            };
            let allowed = config.fields_for(&dataset, reader).unwrap_or_default();
            for mut row in rows {
                if !include_quarantined && crate::store::invalid(&row) {
                    continue;
                }
                row.strip(&allowed);
                let row_key = row.meta().row_key.clone();
                if let Observation::HoldingsReport(report) = row {
                    reports.insert((dataset.clone(), row_key), report);
                }
            }
        }
        Ok(reports)
    }

    /// W10.6, contract v28: holdings, by the holder (a report's holdings)
    /// or by the security held (its holders across reports), each subject
    /// in the role asked; narrowed by their report's kind and report date;
    /// answered with each report they belong to, once. Point in time by
    /// recorded time, the reports read as of the same cut, or by what the
    /// source had published.
    pub(crate) fn list_holdings(
        &self,
        request: ListHoldingsRequest,
        envelope: &Envelope,
    ) -> Result<ListHoldingsReply, String> {
        for (i, kind) in request.kinds.iter().enumerate() {
            if HoldingsReportKind::try_from(*kind).unwrap_or(HoldingsReportKind::Unspecified)
                == HoldingsReportKind::Unspecified
            {
                return Err(format!("kinds[{i}] is unspecified"));
            }
        }
        let dates = dates_of(&request.from_date, &request.to_date)?;
        // One recorded-time cut for the holdings and their reports, carried
        // by the cursor so every page reads as one.
        let as_of = match cursor_of(&request.cursor)? {
            Some((as_of, _)) => as_of,
            None if request.as_of_ns != 0 => request.as_of_ns,
            None => self.clock.now_ns(),
        };
        let reader = reader_of(envelope);
        let include_quarantined = request
            .sources
            .as_ref()
            .is_some_and(|sources| sources.include_quarantined);
        let reports = self.reports_of(
            &reader,
            as_of,
            request.published_as_of_ns.max(0),
            include_quarantined,
        )?;
        let narrowed = read::Narrowed {
            concepts: BTreeSet::new(),
            reports: (!request.kinds.is_empty() || dates.is_some()).then(|| {
                reports
                    .iter()
                    .filter(|(_, report)| {
                        request.kinds.is_empty() || request.kinds.contains(&report.kind)
                    })
                    .filter(|(_, report)| {
                        dates.as_ref().is_none_or(|(from, to)| {
                            Date::parse(&report.report_date)
                                .is_ok_and(|date| &date >= from && &date <= to)
                        })
                    })
                    .map(|(key, _)| key.clone())
                    .collect()
            }),
        };
        let asked = Asked {
            data_type: DataType::Holding,
            subjects: request.subjects,
            subjects_bound: bounds::LIST_HOLDINGS_REQUEST_SUBJECTS_COUNT,
            // A holding's kind and date are its report's: narrowed above.
            kinds: Vec::new(),
            from_date: String::new(),
            to_date: String::new(),
            published_as_of_ns: request.published_as_of_ns,
            sources: request.sources,
            as_of_ns: as_of,
            page_size: request.page_size,
            cursor: request.cursor,
            after: None,
        };
        let answer = self.read_within(reader, asked.request()?, true, narrowed)?;
        let mut holdings = Vec::new();
        let mut answered: Vec<HoldingsReport> = Vec::new();
        let mut once: BTreeSet<(String, String)> = BTreeSet::new();
        for row in answer.rows {
            let dataset = row.dataset().to_string();
            if let Observation::Holding(holding) = row {
                let key = (dataset, holding.report_row_key.clone());
                if let Some(report) = reports.get(&key) {
                    if once.insert(key) {
                        answered.push(report.clone());
                    }
                }
                holdings.push(holding);
            }
        }
        Ok(ListHoldingsReply {
            holdings,
            unanswered: answer.unanswered,
            datasets: answer.datasets,
            watermark: Some(answer.watermark),
            next_cursor: answer.next_cursor,
            reports: answered,
        })
    }
}

/// Register the filings slice's handlers (W10.4, W10.6).
pub(super) fn serve(bus: &Arc<Bus>, lake: &Arc<Lake>) {
    serve_on(
        bus,
        lake,
        RECORD_FILINGS,
        (
            "meridian.v1.RecordFilingsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordFilingsRequest, envelope| {
            lake.record(
                "filings",
                request
                    .filings
                    .into_iter()
                    .map(Observation::Filing)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        bus,
        lake,
        LIST_FILINGS,
        (
            "meridian.v1.ListFilingsRequest",
            "meridian.v1.ListFilingsReply",
        ),
        |lake, request: ListFilingsRequest, envelope| lake.list_filings(request, envelope),
    );
    serve_on(
        bus,
        lake,
        RECORD_REPORTED_FACTS,
        (
            "meridian.v1.RecordReportedFactsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordReportedFactsRequest, envelope| {
            lake.record(
                "facts",
                request
                    .facts
                    .into_iter()
                    .map(Observation::ReportedFact)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        bus,
        lake,
        LIST_REPORTED_FACTS,
        (
            "meridian.v1.ListReportedFactsRequest",
            "meridian.v1.ListReportedFactsReply",
        ),
        |lake, request: ListReportedFactsRequest, envelope| {
            lake.list_reported_facts(request, envelope)
        },
    );
    serve_on(
        bus,
        lake,
        RECORD_RATING_ACTIONS,
        (
            "meridian.v1.RecordRatingActionsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordRatingActionsRequest, envelope| {
            lake.record(
                "actions",
                request
                    .actions
                    .into_iter()
                    .map(Observation::RatingAction)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        bus,
        lake,
        LIST_RATING_ACTIONS,
        (
            "meridian.v1.ListRatingActionsRequest",
            "meridian.v1.ListRatingActionsReply",
        ),
        |lake, request: ListRatingActionsRequest, envelope| {
            lake.list_rating_actions(request, envelope)
        },
    );
    // Holdings: the guardrail's second move-out (557).
    serve_on(
        bus,
        lake,
        RECORD_HOLDINGS_REPORTS,
        (
            "meridian.v1.RecordHoldingsReportsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordHoldingsReportsRequest, envelope| {
            lake.record(
                "reports",
                request
                    .reports
                    .into_iter()
                    .map(Observation::HoldingsReport)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        bus,
        lake,
        RECORD_HOLDINGS,
        (
            "meridian.v1.RecordHoldingsRequest",
            "meridian.v1.RecordObservationsReply",
        ),
        |lake, request: RecordHoldingsRequest, envelope| {
            lake.record(
                "holdings",
                request
                    .holdings
                    .into_iter()
                    .map(Observation::Holding)
                    .collect(),
                &request.want_id,
                envelope,
            )
        },
    );
    serve_on(
        bus,
        lake,
        LIST_HOLDINGS,
        (
            "meridian.v1.ListHoldingsRequest",
            "meridian.v1.ListHoldingsReply",
        ),
        |lake, request: ListHoldingsRequest, envelope| lake.list_holdings(request, envelope),
    );
}
