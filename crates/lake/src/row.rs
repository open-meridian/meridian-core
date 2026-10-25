//! One row of the lake under its envelope: a price or a bar (the 1a), a trade
//! or a quote (the 1b, contract v19), an event contract's outcome (the 1c,
//! contract v23), a rate (contract v27, the rates slice), a statistic and a
//! listed series, a derived series, a saved scenario and a declared corporate
//! action (contract v28).

use std::collections::BTreeSet;

use meridian_domain::exact::Exact;
use meridian_domain::lake;
use meridian_domain::v1::{
    Bar, CorporateActionAnnounced, DerivedSeries, ExecutionCost, ListedSeries, Money,
    ObservationMeta, Outcome, Price, Quote, Rate, SavedScenario, Statistic, Trade,
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
            _ => None,
        }
    }

    /// Whether its rows are keyed by UTC instants alone, with no business
    /// date: real-time data (contract v19; ruling 3 of 2026-10-09).
    pub fn real_time(self) -> bool {
        matches!(self, DataType::Trade | DataType::Quote)
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
        }
    }

    /// A price's kind, or from contract v27 a rate's, from contract v28 a
    /// statistic's and a corporate action's; 0 for the other data types.
    pub fn kind(&self) -> i32 {
        match self {
            Observation::Price(price) => price.kind,
            Observation::Rate(rate) => rate.kind,
            Observation::Statistic(statistic) => statistic.kind,
            Observation::CorporateAction(action) => action.kind,
            _ => 0,
        }
    }

    /// The date a read by dates is on (contract v28, W10.6): its business
    /// date, or for a corporate action its kind's date -- a cash dividend's
    /// ex-date, the date a holder's entitlement turns on.
    pub fn date(&self) -> &str {
        match self {
            Observation::CorporateAction(action) => &action.ex_date,
            _ => &self.meta().business_date,
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
            | Observation::CorporateAction(_) => String::new(),
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
            Observation::Statistic(_)
            | Observation::Series(_)
            | Observation::Derived(_)
            | Observation::Scenario(_) => Vec::new(),
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
    pub fn confirms(&self, other: &Observation) -> bool {
        self.data_type() == DataType::Rate
            && self.meta().raw != other.meta().raw
            && self.same_but_raw(other)
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
                a.kind == b.kind && a.basis == b.basis && same_money(&a.price, &b.price)
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
            }
            (Observation::Quote(a), Observation::Quote(b)) => {
                same_money(&a.bid, &b.bid)
                    && same_money(&a.ask, &b.ask)
                    && same_decimal(&a.bid_quantity, &b.bid_quantity)
                    && same_decimal(&a.ask_quantity, &b.ask_quantity)
                    && a.characteristics == b.characteristics
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
        }
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
