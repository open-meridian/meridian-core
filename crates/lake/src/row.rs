//! One row of the lake under its envelope: a price or a bar (the 1a), a trade
//! or a quote (the 1b, contract v19), an event contract's outcome (the 1c,
//! contract v23).

use std::collections::BTreeSet;

use meridian_domain::exact::Exact;
use meridian_domain::lake;
use meridian_domain::v1::{
    Bar, ExecutionCost, Money, ObservationMeta, Outcome, Price, Quote, Trade,
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
        }
    }

    pub fn code(self) -> i16 {
        match self {
            DataType::Price => 1,
            DataType::Bar => 2,
            DataType::Trade => 3,
            DataType::Quote => 4,
            DataType::Outcome => 5,
            // 6 is left for the rates slice's Rate.
            DataType::ExecutionCost => 7,
        }
    }

    pub fn from_code(code: i16) -> Option<DataType> {
        match code {
            1 => Some(DataType::Price),
            2 => Some(DataType::Bar),
            3 => Some(DataType::Trade),
            4 => Some(DataType::Quote),
            5 => Some(DataType::Outcome),
            7 => Some(DataType::ExecutionCost),
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
        // An execution cost is dated by its execution's trade date.
        matches!(
            self,
            DataType::Price | DataType::Bar | DataType::ExecutionCost
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
        }
    }

    /// A price's kind; 0 for the other data types.
    pub fn kind(&self) -> i32 {
        match self {
            Observation::Price(price) => price.kind,
            _ => 0,
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
            Observation::Trade(trade) => trade
                .price
                .as_mut()
                .map(|m| vec![("price", m)])
                .unwrap_or_default(),
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
        }
    }

    /// Whether two rows say the same thing (Q31): every value the source
    /// gave, decimals compared by value so 764.2 is 764.20, and none of what
    /// the lake decides or the sidecar stamps.
    pub fn same_value(&self, other: &Observation) -> bool {
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
                meta.raw.clone(),
                meta.unconverted.clone(),
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
        }
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
