//! One row of the lake under its envelope: a price or a bar (the 1a), a trade
//! or a quote (the 1b, contract v19), an event contract's outcome (the 1c,
//! contract v23), a rate (contract v27, the rates slice), a statistic and a
//! listed series, a derived series, a saved scenario and a declared corporate
//! action, and the filings slice's filings, reported facts, rating actions,
//! holdings reports and holdings (contract v28).

use std::collections::BTreeSet;

use meridian_domain::exact::Exact;
use meridian_domain::lake;
use meridian_domain::v1::{
    Bar, CorporateActionAnnounced, DerivedSeries, ExecutionCost, Filing, Holding, HoldingsReport,
    InstrumentType, ListedSeries, Money, ObservationMeta, Outcome, Price, Quote, Rate,
    RatingAction, ReportedFact, SavedScenario, Statistic, Trade,
};
use meridian_pb::v1::Decimal;
use prost::Message;

/// The lake's data types, as a store numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DataType {
    Price,
    Bar,
    /// Contract v19.
    Trade,
    Quote,
    /// Contract v23.
    Outcome,
    /// Contract v27 (W10.11): the analytics component's, account-scoped.
    ExecutionCost,
    /// Contract v27, the rates slice (W10.4).
    Rate,
    /// Contract v28 (G11, W10.4): open interest and volumes.
    Statistic,
    /// Contract v28 (ruling 561, W10.4): a series' terms, never minted.
    Series,
    /// Contract v28, track B (W10.4): a signal plugin's derived value.
    Derived,
    /// Contract v28 (W12.8): a saved scenario or scenario set.
    Scenario,
    /// Contract v28, the filings slice (W10.4): a declared cash dividend.
    CorporateAction,
    /// Contract v28, the filings slice (W10.4): a filing, keyed by accession.
    Filing,
    /// Contract v28, the filings slice (W10.4): a value a filer reported.
    ReportedFact,
    /// Contract v28, the filings slice (W10.4): a rating agency's action.
    RatingAction,
    /// Contract v28, the filings slice (W10.4; the guardrail's second
    /// move-out): a 13F, 13D or 13G report.
    HoldingsReport,
    /// Contract v28, the filings slice: one holding of a holdings report.
    Holding,
}

impl DataType {
    /// By its message's full name, as a priority, a want and a catalogue
    /// name it.
    pub fn named(name: &str) -> Option<DataType> {
        match name {
            lake::PRICE => Some(DataType::Price),
            lake::BAR => Some(DataType::Bar),
            lake::TRADE => Some(DataType::Trade),
            lake::QUOTE => Some(DataType::Quote),
            lake::OUTCOME => Some(DataType::Outcome),
            lake::EXECUTION_COST => Some(DataType::ExecutionCost),
            lake::RATE => Some(DataType::Rate),
            lake::STATISTIC => Some(DataType::Statistic),
            lake::LISTED_SERIES => Some(DataType::Series),
            lake::DERIVED_SERIES => Some(DataType::Derived),
            lake::SAVED_SCENARIO => Some(DataType::Scenario),
            lake::CORPORATE_ACTION => Some(DataType::CorporateAction),
            lake::FILING => Some(DataType::Filing),
            lake::REPORTED_FACT => Some(DataType::ReportedFact),
            lake::RATING_ACTION => Some(DataType::RatingAction),
            lake::HOLDINGS_REPORT => Some(DataType::HoldingsReport),
            lake::HOLDING => Some(DataType::Holding),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            DataType::Price => lake::PRICE,
            DataType::Bar => lake::BAR,
            DataType::Trade => lake::TRADE,
            DataType::Quote => lake::QUOTE,
            DataType::Outcome => lake::OUTCOME,
            DataType::ExecutionCost => lake::EXECUTION_COST,
            DataType::Rate => lake::RATE,
            DataType::Statistic => lake::STATISTIC,
            DataType::Series => lake::LISTED_SERIES,
            DataType::Derived => lake::DERIVED_SERIES,
            DataType::Scenario => lake::SAVED_SCENARIO,
            DataType::CorporateAction => lake::CORPORATE_ACTION,
            DataType::Filing => lake::FILING,
            DataType::ReportedFact => lake::REPORTED_FACT,
            DataType::RatingAction => lake::RATING_ACTION,
            DataType::HoldingsReport => lake::HOLDINGS_REPORT,
            DataType::Holding => lake::HOLDING,
        }
    }

    pub fn code(self) -> i16 {
        match self {
            DataType::Price => 1,
            DataType::Bar => 2,
            DataType::Trade => 3,
            DataType::Quote => 4,
            DataType::Outcome => 5,
            DataType::Rate => 6,
            DataType::ExecutionCost => 7,
            DataType::Statistic => 8,
            DataType::Series => 9,
            DataType::Derived => 10,
            DataType::Scenario => 11,
            DataType::CorporateAction => 12,
            DataType::Filing => 13,
            DataType::ReportedFact => 14,
            DataType::RatingAction => 15,
            DataType::HoldingsReport => 16,
            DataType::Holding => 17,
        }
    }

    pub fn from_code(code: i16) -> Option<DataType> {
        match code {
            1 => Some(DataType::Price),
            2 => Some(DataType::Bar),
            3 => Some(DataType::Trade),
            4 => Some(DataType::Quote),
            5 => Some(DataType::Outcome),
            6 => Some(DataType::Rate),
            7 => Some(DataType::ExecutionCost),
            8 => Some(DataType::Statistic),
            9 => Some(DataType::Series),
            10 => Some(DataType::Derived),
            11 => Some(DataType::Scenario),
            12 => Some(DataType::CorporateAction),
            13 => Some(DataType::Filing),
            14 => Some(DataType::ReportedFact),
            15 => Some(DataType::RatingAction),
            16 => Some(DataType::HoldingsReport),
            17 => Some(DataType::Holding),
            _ => None,
        }
    }

    /// Whether its rows are keyed by UTC instants alone, with no business
    /// date: real-time data (contract v19; ruling 3 of 2026-10-09).
    pub fn real_time(self) -> bool {
        matches!(self, DataType::Trade | DataType::Quote)
    }

    /// Whether it is one of the filings slice's kinds (contract v28, W10.4):
    /// its rows name their subjects in roles, a company among them by its
    /// `ENT-` record, are dated by their own fields, never a business date,
    /// and are never conflated.
    pub fn filings(self) -> bool {
        matches!(
            self,
            DataType::CorporateAction
                | DataType::Filing
                | DataType::ReportedFact
                | DataType::RatingAction
                | DataType::HoldingsReport
                | DataType::Holding
        )
    }

    /// Whether its rows may name a business date: a daily value's, never a
    /// trade's, a quote's or an outcome's (contract v23, the lake plan's
    /// vL1c, items 3 and 11).
    pub fn dated(self) -> bool {
        // An execution cost is dated by its execution's trade date; a rate
        // by the business day it is for, in its dataset's zone.
        // A statistic by its trading date (contract v28, W10.4).
        matches!(
            self,
            DataType::Price
                | DataType::Bar
                | DataType::ExecutionCost
                | DataType::Rate
                | DataType::Statistic
                | DataType::Derived
        )
    }
}

/// A row: a price, a bar, a trade or a quote. Unboxed: rows live for one
/// batch of at most 500, and every one is decoded from and encoded to the
/// wire whole.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Observation {
    Price(Price),
    Bar(Bar),
    Trade(Trade),
    Quote(Quote),
    Outcome(Outcome),
    ExecutionCost(ExecutionCost),
    Rate(Rate),
    Statistic(Statistic),
    Series(ListedSeries),
    Derived(DerivedSeries),
    Scenario(SavedScenario),
    CorporateAction(CorporateActionAnnounced),
    Filing(Filing),
    ReportedFact(ReportedFact),
    RatingAction(RatingAction),
    HoldingsReport(HoldingsReport),
    Holding(Holding),
}

impl Observation {
    pub fn data_type(&self) -> DataType {
        match self {
            Observation::Price(_) => DataType::Price,
            Observation::Bar(_) => DataType::Bar,
            Observation::Trade(_) => DataType::Trade,
            Observation::Quote(_) => DataType::Quote,
            Observation::Outcome(_) => DataType::Outcome,
            Observation::ExecutionCost(_) => DataType::ExecutionCost,
            Observation::Rate(_) => DataType::Rate,
            Observation::Statistic(_) => DataType::Statistic,
            Observation::Series(_) => DataType::Series,
            Observation::Derived(_) => DataType::Derived,
            Observation::Scenario(_) => DataType::Scenario,
            Observation::CorporateAction(_) => DataType::CorporateAction,
            Observation::Filing(_) => DataType::Filing,
            Observation::ReportedFact(_) => DataType::ReportedFact,
            Observation::RatingAction(_) => DataType::RatingAction,
            Observation::HoldingsReport(_) => DataType::HoldingsReport,
            Observation::Holding(_) => DataType::Holding,
        }
    }

    pub fn meta(&self) -> &ObservationMeta {
        static NONE: std::sync::OnceLock<ObservationMeta> = std::sync::OnceLock::new();
        let meta = match self {
            Observation::Price(price) => price.meta.as_ref(),
            Observation::Bar(bar) => bar.meta.as_ref(),
            Observation::Trade(trade) => trade.meta.as_ref(),
            Observation::Quote(quote) => quote.meta.as_ref(),
            Observation::Outcome(outcome) => outcome.meta.as_ref(),
            Observation::ExecutionCost(cost) => cost.meta.as_ref(),
            Observation::Rate(rate) => rate.meta.as_ref(),
            Observation::Statistic(statistic) => statistic.meta.as_ref(),
            Observation::Series(series) => series.meta.as_ref(),
            Observation::Derived(value) => value.meta.as_ref(),
            Observation::Scenario(saved) => saved.meta.as_ref(),
            Observation::CorporateAction(action) => action.meta.as_ref(),
            Observation::Filing(filing) => filing.meta.as_ref(),
            Observation::ReportedFact(fact) => fact.meta.as_ref(),
            Observation::RatingAction(action) => action.meta.as_ref(),
            Observation::HoldingsReport(report) => report.meta.as_ref(),
            Observation::Holding(holding) => holding.meta.as_ref(),
        };
        meta.unwrap_or_else(|| NONE.get_or_init(ObservationMeta::default))
    }

    pub fn meta_mut(&mut self) -> &mut ObservationMeta {
        match self {
            Observation::Price(price) => price.meta.get_or_insert_with(Default::default),
            Observation::Bar(bar) => bar.meta.get_or_insert_with(Default::default),
            Observation::Trade(trade) => trade.meta.get_or_insert_with(Default::default),
            Observation::Quote(quote) => quote.meta.get_or_insert_with(Default::default),
            Observation::Outcome(outcome) => outcome.meta.get_or_insert_with(Default::default),
            Observation::ExecutionCost(cost) => cost.meta.get_or_insert_with(Default::default),
            Observation::Rate(rate) => rate.meta.get_or_insert_with(Default::default),
            Observation::Statistic(statistic) => {
                statistic.meta.get_or_insert_with(Default::default)
            }
            Observation::Series(series) => series.meta.get_or_insert_with(Default::default),
            Observation::Derived(value) => value.meta.get_or_insert_with(Default::default),
            Observation::Scenario(saved) => saved.meta.get_or_insert_with(Default::default),
            Observation::CorporateAction(action) => {
                action.meta.get_or_insert_with(Default::default)
            }
            Observation::Filing(filing) => filing.meta.get_or_insert_with(Default::default),
            Observation::ReportedFact(fact) => fact.meta.get_or_insert_with(Default::default),
            Observation::RatingAction(action) => action.meta.get_or_insert_with(Default::default),
            Observation::HoldingsReport(report) => report.meta.get_or_insert_with(Default::default),
            Observation::Holding(holding) => holding.meta.get_or_insert_with(Default::default),
        }
    }

    /// A price's kind, or from contract v27 a rate's, from contract v28 a
    /// statistic's, a corporate action's, a filing's, a rating action's
    /// action and a holdings report's; 0 for the other data types.
    pub fn kind(&self) -> i32 {
        match self {
            Observation::Price(price) => price.kind,
            Observation::Rate(rate) => rate.kind,
            Observation::Statistic(statistic) => statistic.kind,
            Observation::CorporateAction(action) => action.kind,
            Observation::Filing(filing) => filing.kind,
            Observation::RatingAction(action) => action.action,
            Observation::HoldingsReport(report) => report.kind,
            _ => 0,
        }
    }

    /// The date a read by dates is on (contract v28, W10.6): its business
    /// date, or for the filings slice's kinds the kind's date -- a cash
    /// dividend's ex-date, the date a holder's entitlement turns on; a
    /// filing's filed date; a fact's period end; a rating action's date,
    /// its valid time's UTC day; a holdings report's report date. A
    /// holding's is its report's, read by ListHoldings beside it.
    pub fn date(&self) -> String {
        match self {
            Observation::CorporateAction(action) => action.ex_date.clone(),
            Observation::Filing(filing) => filing.filed_date.clone(),
            Observation::ReportedFact(fact) => fact.period_end.clone(),
            Observation::RatingAction(_) => {
                meridian_domain::date::Date::of_utc_instant(self.meta().valid_from_ns).to_string()
            }
            Observation::HoldingsReport(report) => report.report_date.clone(),
            _ => self.meta().business_date.clone(),
        }
    }

    /// Its series key as one text (contract v28, ruling 561), empty for a
    /// row keyed by no series.
    pub fn series(&self) -> String {
        lake::series_key_text(&self.meta().series_key)
    }

    /// A rate's point and convention (contract v27, W10.5, W10.6): its
    /// tenor or maturity and its convention, what its latest is chosen and
    /// its deliveries conflated per beside the kind; empty for the other
    /// data types.
    ///
    /// From contract v28 a row keyed by a series has the series in its point
    /// too: a chain's rows under one underlying are chosen and conflated per
    /// series (W10.5, W10.6).
    pub fn point(&self) -> String {
        let point = match self {
            Observation::Rate(rate) => lake::rate_point(rate),
            // A derived value's latest is per its series (contract v28).
            Observation::Derived(value) => value.series.clone(),
            _ => String::new(),
        };
        let series = self.series();
        if series.is_empty() {
            point
        } else {
            format!("{point}#{series}")
        }
    }

    /// What a read of the latest chooses per besides its subject,
    /// dataset, kind, venue and length -- the row's asset and point -- as
    /// one text, which the Postgres store keeps beside the row to choose
    /// the latest in SQL.
    pub fn latest_key(&self) -> String {
        format!("{}\u{1f}{}", self.asset(), self.point())
    }

    /// What a listed series' row is listed by (contract v28, amended
    /// 2026-10-28; ruling 856): its expiry, as a plain date where it states
    /// one, and its listed type as recorded -- a future where it names a
    /// product code, else an option, an option on a future record told at
    /// read by its underlying. `None` for every other row. The Postgres
    /// store keeps both beside the row, so an expirations read never reads
    /// a series row whole.
    pub fn listing(&self) -> Option<(Option<String>, InstrumentType)> {
        match self {
            Observation::Series(series) => Some((
                crate::read::expires(series).map(|date| date.to_string()),
                crate::read::listed_type(series),
            )),
            _ => None,
        }
    }

    /// What its delivery is conflated per (W10.5; decisions/024), as the
    /// readers' sidecars conflate it: a price per subject, dataset, kind,
    /// venue and asset; a bar per subject, dataset, venue and interval
    /// start; a quote per subject, dataset, venue and asset; a rate per
    /// subject, dataset, kind and point; a statistic per subject, dataset,
    /// kind and business date -- each per series too where it is keyed by
    /// one (ruling 561). `None` for a row every version of which is
    /// delivered: a trade, an outcome, the filings slice's kinds.
    pub fn conflation_key(&self) -> Option<String> {
        let what = match self {
            Observation::Price(_) => "price",
            Observation::Bar(_) => "bar",
            Observation::Quote(_) => "quote",
            Observation::Rate(_) => "rate",
            Observation::Statistic(_) => "statistic",
            _ => return None,
        };
        let meta = self.meta();
        let start = match self {
            Observation::Bar(_) => meta.valid_from_ns,
            _ => 0,
        };
        let date = match self {
            Observation::Statistic(_) => meta.business_date.as_str(),
            _ => "",
        };
        Some(format!(
            "{what}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{start}\u{1f}{date}\u{1f}{}",
            self.subject(),
            self.dataset(),
            self.kind(),
            self.venue(),
            self.latest_key(),
        ))
    }

    /// A bar's length; 0 for the other data types.
    pub fn interval_ns(&self) -> i64 {
        match self {
            Observation::Bar(_) => {
                let meta = self.meta();
                (meta.valid_until_ns - meta.valid_from_ns).max(0)
            }
            _ => 0,
        }
    }

    /// The asset its amounts are in, by cash instrument (contract v19): what
    /// a price's and a quote's latest is chosen per (W10.6) and their
    /// deliveries conflated per (W10.5). A bar's is its close's, and empty
    /// for a row with no amount.
    pub fn asset(&self) -> String {
        match self {
            Observation::Price(price) => lake::asset_key(price.price.as_ref()),
            Observation::Bar(bar) => lake::asset_key(
                bar.close
                    .as_ref()
                    .or(bar.open.as_ref())
                    .or(bar.high.as_ref())
                    .or(bar.low.as_ref()),
            ),
            Observation::Trade(trade) => lake::asset_key(trade.price.as_ref()),
            Observation::Quote(quote) => lake::quote_asset(quote),
            Observation::Outcome(outcome) => lake::asset_key(outcome.settlement_value.as_ref()),
            Observation::ExecutionCost(cost) => lake::asset_key(cost.execution_price.as_ref()),
            // A rate is no amount: its unit is in its point's key; nor is a
            // statistic or a series' terms (contract v28).
            Observation::Rate(_)
            | Observation::Statistic(_)
            | Observation::Series(_)
            | Observation::Derived(_)
            | Observation::Scenario(_)
            // Never conflated nor chosen per asset (contract v28, W10.5).
            | Observation::CorporateAction(_)
            | Observation::Filing(_)
            | Observation::ReportedFact(_)
            | Observation::RatingAction(_)
            | Observation::HoldingsReport(_)
            | Observation::Holding(_) => String::new(),
        }
    }

    /// Its first subject: what a store keys its subject's sequence by.
    pub fn subject(&self) -> &str {
        self.meta()
            .subjects
            .first()
            .map(|s| s.entity_id.as_str())
            .unwrap_or_default()
    }

    pub fn subjects(&self) -> Vec<String> {
        self.meta()
            .subjects
            .iter()
            .map(|s| s.entity_id.clone())
            .collect()
    }

    pub fn dataset(&self) -> &str {
        self.meta()
            .source
            .as_ref()
            .map(|s| s.dataset.as_str())
            .unwrap_or_default()
    }

    pub fn venue(&self) -> &str {
        self.meta()
            .source
            .as_ref()
            .map(|s| s.venue_id.as_str())
            .unwrap_or_default()
    }

    /// Its size encoded, without encoding it.
    pub fn encoded_len(&self) -> usize {
        match self {
            Observation::Price(price) => price.encoded_len(),
            Observation::Bar(bar) => bar.encoded_len(),
            Observation::Trade(trade) => trade.encoded_len(),
            Observation::Quote(quote) => quote.encoded_len(),
            Observation::Outcome(outcome) => outcome.encoded_len(),
            Observation::ExecutionCost(cost) => cost.encoded_len(),
            Observation::Rate(rate) => rate.encoded_len(),
            Observation::Statistic(statistic) => statistic.encoded_len(),
            Observation::Series(series) => series.encoded_len(),
            Observation::Derived(value) => value.encoded_len(),
            Observation::Scenario(saved) => saved.encoded_len(),
            Observation::CorporateAction(action) => action.encoded_len(),
            Observation::Filing(filing) => filing.encoded_len(),
            Observation::ReportedFact(fact) => fact.encoded_len(),
            Observation::RatingAction(action) => action.encoded_len(),
            Observation::HoldingsReport(report) => report.encoded_len(),
            Observation::Holding(holding) => holding.encoded_len(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            Observation::Price(price) => price.encode_to_vec(),
            Observation::Bar(bar) => bar.encode_to_vec(),
            Observation::Trade(trade) => trade.encode_to_vec(),
            Observation::Quote(quote) => quote.encode_to_vec(),
            Observation::Outcome(outcome) => outcome.encode_to_vec(),
            Observation::ExecutionCost(cost) => cost.encode_to_vec(),
            Observation::Rate(rate) => rate.encode_to_vec(),
            Observation::Statistic(statistic) => statistic.encode_to_vec(),
            Observation::Series(series) => series.encode_to_vec(),
            Observation::Derived(value) => value.encode_to_vec(),
            Observation::Scenario(saved) => saved.encode_to_vec(),
            Observation::CorporateAction(action) => action.encode_to_vec(),
            Observation::Filing(filing) => filing.encode_to_vec(),
            Observation::ReportedFact(fact) => fact.encode_to_vec(),
            Observation::RatingAction(action) => action.encode_to_vec(),
            Observation::HoldingsReport(report) => report.encode_to_vec(),
            Observation::Holding(holding) => holding.encode_to_vec(),
        }
    }

    pub fn decode(data_type: DataType, bytes: &[u8]) -> Result<Observation, prost::DecodeError> {
        Ok(match data_type {
            DataType::Price => Observation::Price(Price::decode(bytes)?),
            DataType::Bar => Observation::Bar(Bar::decode(bytes)?),
            DataType::Trade => Observation::Trade(Trade::decode(bytes)?),
            DataType::Quote => Observation::Quote(Quote::decode(bytes)?),
            DataType::Outcome => Observation::Outcome(Outcome::decode(bytes)?),
            DataType::ExecutionCost => Observation::ExecutionCost(ExecutionCost::decode(bytes)?),
            DataType::Rate => Observation::Rate(Rate::decode(bytes)?),
            DataType::Statistic => Observation::Statistic(Statistic::decode(bytes)?),
            DataType::Series => Observation::Series(ListedSeries::decode(bytes)?),
            DataType::Derived => Observation::Derived(DerivedSeries::decode(bytes)?),
            DataType::Scenario => Observation::Scenario(SavedScenario::decode(bytes)?),
            DataType::CorporateAction => {
                Observation::CorporateAction(CorporateActionAnnounced::decode(bytes)?)
            }
            DataType::Filing => Observation::Filing(Filing::decode(bytes)?),
            DataType::ReportedFact => Observation::ReportedFact(ReportedFact::decode(bytes)?),
            DataType::RatingAction => Observation::RatingAction(RatingAction::decode(bytes)?),
            DataType::HoldingsReport => Observation::HoldingsReport(HoldingsReport::decode(bytes)?),
            DataType::Holding => Observation::Holding(Holding::decode(bytes)?),
        })
    }

    /// Every amount it carries, with its field's name.
    pub fn moneys_mut(&mut self) -> Vec<(&'static str, &mut Money)> {
        match self {
            Observation::Price(price) => price
                .price
                .as_mut()
                .map(|m| vec![("price", m)])
                .unwrap_or_default(),
            Observation::Bar(bar) => {
                let mut out = Vec::new();
                if let Some(m) = bar.open.as_mut() {
                    out.push(("open", m));
                }
                if let Some(m) = bar.high.as_mut() {
                    out.push(("high", m));
                }
                if let Some(m) = bar.low.as_mut() {
                    out.push(("low", m));
                }
                if let Some(m) = bar.close.as_mut() {
                    out.push(("close", m));
                }
                if let Some(m) = bar.vwap.as_mut() {
                    out.push(("vwap", m));
                }
                out
            }
            Observation::Trade(trade) => {
                let mut out = Vec::new();
                if let Some(m) = trade.price.as_mut() {
                    out.push(("price", m));
                }
                // Contract v28 (G9): the quote in force at the print.
                if let Some(quote) = trade.quote_at_trade.as_mut() {
                    if let Some(m) = quote.bid.as_mut() {
                        out.push(("quote_at_trade.bid", m));
                    }
                    if let Some(m) = quote.ask.as_mut() {
                        out.push(("quote_at_trade.ask", m));
                    }
                }
                out
            }
            Observation::Quote(quote) => {
                let mut out = Vec::new();
                if let Some(m) = quote.bid.as_mut() {
                    out.push(("bid", m));
                }
                if let Some(m) = quote.ask.as_mut() {
                    out.push(("ask", m));
                }
                out
            }
            Observation::Outcome(outcome) => outcome
                .settlement_value
                .as_mut()
                .map(|m| vec![("settlement_value", m)])
                .unwrap_or_default(),
            Observation::ExecutionCost(cost) => {
                let mut out = Vec::new();
                if let Some(m) = cost.benchmark_price.as_mut() {
                    out.push(("benchmark_price", m));
                }
                if let Some(m) = cost.execution_price.as_mut() {
                    out.push(("execution_price", m));
                }
                if let Some(m) = cost.notional.as_mut() {
                    out.push(("notional", m));
                }
                out
            }
            Observation::Rate(rate) => rate
                .volume
                .as_mut()
                .map(|m| vec![("volume", m)])
                .unwrap_or_default(),
            Observation::CorporateAction(action) => action
                .amount_per_share
                .as_mut()
                .map(|m| vec![("amount_per_share", m)])
                .unwrap_or_default(),
            // In the currency the filer reported (contract v28, W10.4).
            Observation::ReportedFact(fact) => fact
                .amount
                .as_mut()
                .map(|m| vec![("amount", m)])
                .unwrap_or_default(),
            Observation::Holding(holding) => holding
                .market_value
                .as_mut()
                .map(|m| vec![("market_value", m)])
                .unwrap_or_default(),
            Observation::Statistic(_)
            | Observation::Series(_)
            | Observation::Derived(_)
            | Observation::Scenario(_)
            | Observation::Filing(_)
            | Observation::RatingAction(_)
            | Observation::HoldingsReport(_) => Vec::new(),
        }
    }

    /// Whether two rows say the same thing (Q31): every value the source
    /// gave, decimals compared by value so 764.2 is 764.20, and none of what
    /// the lake decides or the sidecar stamps.
    pub fn same_value(&self, other: &Observation) -> bool {
        self.same_but_raw(other) && self.meta().raw == other.meta().raw
    }

    /// Whether two rows say the same thing but for their raw reference: a
    /// rate's confirmation (contract v27, W10.4; research-kinds' shared
    /// addition 2), recorded and not counted as a restatement.
    ///
    /// From contract v28 a reported fact a later filing repeats is a
    /// confirmation too (W10.4, a-fact-confirmed): the same value for the
    /// same concept, unit, dimensions and period, from another filing --
    /// its accession, its published time, its valid time and its raw
    /// reference its own -- recorded as the next version naming that filing,
    /// so first published and latest are two reads of one history.
    pub fn confirms(&self, other: &Observation) -> bool {
        match (self, other) {
            (Observation::ReportedFact(a), Observation::ReportedFact(b)) => {
                let restated = |fact: &ReportedFact| {
                    let mut fact = fact.clone();
                    fact.filing = None;
                    let meta = fact.meta.get_or_insert_with(Default::default);
                    meta.raw = None;
                    meta.source_times.clear();
                    meta.valid_from_ns = 0;
                    meta.valid_until_ns = 0;
                    Observation::ReportedFact(fact)
                };
                (a.filing != b.filing || self.meta().raw != other.meta().raw)
                    && restated(a).same_but_raw(&restated(b))
            }
            _ => {
                self.data_type() == DataType::Rate
                    && self.meta().raw != other.meta().raw
                    && self.same_but_raw(other)
            }
        }
    }

    fn same_but_raw(&self, other: &Observation) -> bool {
        let said = |meta: &ObservationMeta| {
            let source = meta.source.clone().unwrap_or_default();
            (
                meta.row_key.clone(),
                meta.subjects.clone(),
                source.dataset,
                source.venue_id,
                meta.valid_from_ns,
                meta.valid_until_ns,
                meta.business_date.clone(),
                meta.source_times.clone(),
                meta.unconverted.clone(),
                // Contract v28 (ruling 561).
                lake::series_key_text(&meta.series_key),
            )
        };
        if said(self.meta()) != said(other.meta()) {
            return false;
        }
        match (self, other) {
            (Observation::Price(a), Observation::Price(b)) => {
                a.kind == b.kind
                    && a.basis == b.basis
                    && same_money(&a.price, &b.price)
                    // Contract 28, amended 2026-10-31 (ruling 895).
                    && same_decimal(&a.confidence, &b.confidence)
                    // Contract v29 (ruling 685).
                    && same_yield(&a.r#yield, &b.r#yield)
                    && same_decimal(&a.spread_bps, &b.spread_bps)
                    && a.benchmark_instrument_id == b.benchmark_instrument_id
            }
            (Observation::Bar(a), Observation::Bar(b)) => {
                same_money(&a.open, &b.open)
                    && same_money(&a.high, &b.high)
                    && same_money(&a.low, &b.low)
                    && same_money(&a.close, &b.close)
                    && same_decimal(&a.volume, &b.volume)
                    && same_money(&a.vwap, &b.vwap)
                    && a.trade_count == b.trade_count
            }
            (Observation::Trade(a), Observation::Trade(b)) => {
                same_money(&a.price, &b.price)
                    && same_decimal(&a.quantity, &b.quantity)
                    && a.attributes == b.attributes
                    && a.aggressor == b.aggressor
                    && a.source_sequence == b.source_sequence
                    && a.cancelled == b.cancelled
                    && same_quote_at_trade(&a.quote_at_trade, &b.quote_at_trade)
                    // Contract v29 (ruling 685).
                    && a.basis == b.basis
                    && same_yield(&a.r#yield, &b.r#yield)
                    && same_decimal(&a.spread_bps, &b.spread_bps)
                    && a.benchmark_instrument_id == b.benchmark_instrument_id
                    && a.quantity_capped == b.quantity_capped
            }
            (Observation::Quote(a), Observation::Quote(b)) => {
                same_money(&a.bid, &b.bid)
                    && same_money(&a.ask, &b.ask)
                    && same_decimal(&a.bid_quantity, &b.bid_quantity)
                    && same_decimal(&a.ask_quantity, &b.ask_quantity)
                    && a.characteristics == b.characteristics
                    // Contract v29 (rulings 685 and 900).
                    && (a.bid_size_capped, a.ask_size_capped, a.basis)
                        == (b.bid_size_capped, b.ask_size_capped, b.basis)
                    && same_yield(&a.bid_yield, &b.bid_yield)
                    && same_yield(&a.ask_yield, &b.ask_yield)
                    && same_decimal(&a.bid_spread_bps, &b.bid_spread_bps)
                    && same_decimal(&a.ask_spread_bps, &b.ask_spread_bps)
                    && a.benchmark_instrument_id == b.benchmark_instrument_id
            }
            (Observation::Outcome(a), Observation::Outcome(b)) => {
                a.determination == b.determination
                    && a.stage == b.stage
                    && same_money(&a.settlement_value, &b.settlement_value)
                    && a.settled_at_ns == b.settled_at_ns
                    && a.determination_as_reported == b.determination_as_reported
                    // Contract v27 (W10.4): the venue's challenge deadline.
                    && a.challenge_ends_at_ns == b.challenge_ends_at_ns
            }
            (Observation::ExecutionCost(a), Observation::ExecutionCost(b)) => {
                same_money(&a.benchmark_price, &b.benchmark_price)
                    && same_money(&a.execution_price, &b.execution_price)
                    && same_money(&a.notional, &b.notional)
                    && same_decimal(&a.cost_bps, &b.cost_bps)
                    && same_decimal(&a.fees_bps, &b.fees_bps)
                    && same_decimal(&a.financing_bps, &b.financing_bps)
                    && ExecutionCost {
                        meta: None,
                        benchmark_price: None,
                        execution_price: None,
                        notional: None,
                        cost_bps: None,
                        fees_bps: None,
                        financing_bps: None,
                        ..a.clone()
                    } == ExecutionCost {
                        meta: None,
                        benchmark_price: None,
                        execution_price: None,
                        notional: None,
                        cost_bps: None,
                        fees_bps: None,
                        financing_bps: None,
                        ..b.clone()
                    }
            }
            (Observation::Rate(a), Observation::Rate(b)) => {
                let value = |rate: &Rate| rate.value.clone().unwrap_or_default();
                let (va, vb) = (value(a), value(b));
                a.kind == b.kind
                    && same_decimal(&va.value, &vb.value)
                    && (va.unit, &va.index_base, &va.ucum_code)
                        == (vb.unit, &vb.index_base, &vb.ucum_code)
                    && a.value.is_some() == b.value.is_some()
                    && a.convention == b.convention
                    && a.tenor == b.tenor
                    && a.maturity_date == b.maturity_date
                    && a.preliminary == b.preliminary
                    && same_money(&a.volume, &b.volume)
                    && a.transaction_count == b.transaction_count
                    && a.percentiles.len() == b.percentiles.len()
                    && a.percentiles.iter().zip(&b.percentiles).all(|(x, y)| {
                        x.percentile == y.percentile && same_decimal(&x.value, &y.value)
                    })
            }
            (Observation::Statistic(a), Observation::Statistic(b)) => {
                let value = |statistic: &Statistic| statistic.value.clone().unwrap_or_default();
                let (va, vb) = (value(a), value(b));
                a.kind == b.kind
                    && a.value.is_some() == b.value.is_some()
                    && same_decimal(&va.value, &vb.value)
                    && (va.unit, &va.index_base, &va.ucum_code)
                        == (vb.unit, &vb.index_base, &vb.ucum_code)
            }
            (Observation::Series(a), Observation::Series(b)) => {
                a.template_id == b.template_id
                    && a.template_version == b.template_version
                    && a.fields == b.fields
            }
            (Observation::Derived(a), Observation::Derived(b)) => {
                let value = |row: &DerivedSeries| row.value.clone().unwrap_or_default();
                let (va, vb) = (value(a), value(b));
                a.series == b.series
                    && a.value.is_some() == b.value.is_some()
                    && same_decimal(&va.value, &vb.value)
                    && (va.unit, &va.index_base, &va.ucum_code)
                        == (vb.unit, &vb.index_base, &vb.ucum_code)
                    && a.rule == b.rule
                    && a.inputs == b.inputs
                    // Contract 28, amended 2026-10-31 (ruling 955).
                    && a.published_by == b.published_by
                    // Contract v29 (638): an amount of money.
                    && same_money(&a.money_value, &b.money_value)
            }
            (Observation::Scenario(a), Observation::Scenario(b)) => {
                a.scenario == b.scenario
                    && a.scenario_set == b.scenario_set
                    && a.description == b.description
            }
            (Observation::CorporateAction(a), Observation::CorporateAction(b)) => {
                a.kind == b.kind
                    && same_money(&a.amount_per_share, &b.amount_per_share)
                    && (
                        &a.declared_date,
                        &a.ex_date,
                        &a.record_date,
                        &a.payment_date,
                    ) == (
                        &b.declared_date,
                        &b.ex_date,
                        &b.record_date,
                        &b.payment_date,
                    )
                    && a.rule == b.rule
                    && a.filing == b.filing
            }
            (Observation::Filing(a), Observation::Filing(b)) => {
                a.filing == b.filing
                    && a.kind == b.kind
                    && a.items == b.items
                    && a.period_of_report == b.period_of_report
                    && a.filed_date == b.filed_date
            }
            (Observation::ReportedFact(a), Observation::ReportedFact(b)) => {
                a.concept == b.concept
                    && same_money(&a.amount, &b.amount)
                    && same_unit_value(&a.value, &b.value)
                    && (&a.period_start, &a.period_end) == (&b.period_start, &b.period_end)
                    && a.dimensions == b.dimensions
                    && a.filing == b.filing
            }
            (Observation::RatingAction(a), Observation::RatingAction(b)) => {
                (&a.rating, &a.rating_class, a.action) == (&b.rating, &b.rating_class, b.action)
            }
            (Observation::HoldingsReport(a), Observation::HoldingsReport(b)) => {
                (a.kind, &a.report_date, a.complete, &a.filing)
                    == (b.kind, &b.report_date, b.complete, &b.filing)
            }
            (Observation::Holding(a), Observation::Holding(b)) => {
                a.report_row_key == b.report_row_key
                    && same_unit_value(&a.quantity, &b.quantity)
                    && same_money(&a.market_value, &b.market_value)
                    && same_unit_value(&a.percent_of_class, &b.percent_of_class)
            }
            _ => false,
        }
    }

    /// The fields a reader may not read, removed (W10.1: the receiving
    /// sidecar strips its plugin's deliveries, the lake its replies): each
    /// field entry not in `allowed`, an empty set allowing every field. The
    /// envelope is never removed. Answers the entries it removed.
    pub fn strip(&mut self, allowed: &BTreeSet<String>) -> Vec<&'static str> {
        match self {
            Observation::Price(price) => lake::strip_price(price, allowed),
            Observation::Bar(bar) => lake::strip_bar(bar, allowed),
            Observation::Trade(trade) => lake::strip_trade(trade, allowed),
            Observation::Quote(quote) => lake::strip_quote(quote, allowed),
            Observation::Outcome(outcome) => lake::strip_outcome(outcome, allowed),
            // Read within the reader's account scope, never by entitlement.
            Observation::ExecutionCost(_) => Vec::new(),
            Observation::Rate(rate) => lake::strip_rate(rate, allowed),
            Observation::Statistic(statistic) => lake::strip_statistic(statistic, allowed),
            Observation::Series(series) => lake::strip_series(series, allowed),
            // A signal plugin's own rows, read whole by whoever reads them.
            Observation::Derived(_) | Observation::Scenario(_) => Vec::new(),
            Observation::CorporateAction(action) => lake::strip_corporate_action(action, allowed),
            Observation::Filing(filing) => lake::strip_filing(filing, allowed),
            Observation::ReportedFact(fact) => lake::strip_reported_fact(fact, allowed),
            Observation::RatingAction(action) => lake::strip_rating_action(action, allowed),
            Observation::HoldingsReport(report) => lake::strip_holdings_report(report, allowed),
            Observation::Holding(holding) => lake::strip_holding(holding, allowed),
        }
    }
}

/// Two values with their units the same, decimals compared by value.
fn same_unit_value(
    a: &Option<meridian_domain::v1::UnitValue>,
    b: &Option<meridian_domain::v1::UnitValue>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            same_decimal(&a.value, &b.value)
                && (a.unit, &a.index_base, &a.ucum_code) == (b.unit, &b.index_base, &b.ucum_code)
        }
        _ => false,
    }
}

/// Two quoted yields the same (contract v29, ruling 685): the value by its
/// decimal and unit, the convention, the kind and the date it is to.
fn same_yield(
    a: &Option<meridian_domain::v1::QuotedYield>,
    b: &Option<meridian_domain::v1::QuotedYield>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            same_unit_value(&a.value, &b.value)
                && a.convention == b.convention
                && a.kind == b.kind
                && a.to_date == b.to_date
        }
        _ => false,
    }
}

fn same_quote_at_trade(
    a: &Option<meridian_domain::v1::QuoteAtTrade>,
    b: &Option<meridian_domain::v1::QuoteAtTrade>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            same_money(&a.bid, &b.bid)
                && same_money(&a.ask, &b.ask)
                && same_decimal(&a.bid_quantity, &b.bid_quantity)
                && same_decimal(&a.ask_quantity, &b.ask_quantity)
                && a.source_time_ns == b.source_time_ns
                && a.as_last_received == b.as_last_received
        }
        _ => false,
    }
}

fn exact(decimal: &Option<Decimal>) -> Option<Option<Exact>> {
    match decimal {
        None => Some(None),
        Some(d) => Exact::from_wire(d).ok().map(Some),
    }
}

fn same_decimal(a: &Option<Decimal>, b: &Option<Decimal>) -> bool {
    match (exact(a), exact(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

fn same_money(a: &Option<Money>, b: &Option<Money>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.currency_code == b.currency_code
                && a.instrument_id == b.instrument_id
                && same_decimal(&a.amount, &b.amount)
        }
        _ => false,
    }
}
