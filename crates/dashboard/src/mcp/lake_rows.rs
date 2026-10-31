//! The lake's rows as `dashboard__query_lake` answers them (contract v26,
//! W10.10): each field by its proto name, a decimal as a string, a time in
//! nanoseconds as a decimal string, an enum by its value's name; a field
//! the query did not name, or the instance may not read, absent.

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    Aggressor, Bar, Compounding, DayCount, ExecutionCost, LiquidityIndicator, Money,
    ObservationMeta, Outcome, OutcomeDetermination, OutcomeStage, Price, PriceBasis, PriceKind,
    QualityReason, QualityState, Quote, QuoteCharacteristic, QuotedYield, Rate, RateKind, Side,
    TcaBenchmark, TcaUnanswered, TenorUnit, TimePrecision, Trade, TradeCharacteristic, ValueUnit,
    YieldKind,
};
use serde_json::{json, Map, Value};

fn decimal(value: Option<&meridian_pb::v1::Decimal>) -> Value {
    value
        .and_then(|d| Exact::from_wire(d).ok())
        .map_or(Value::Null, |e| Value::String(e.to_string()))
}

fn money(value: Option<&Money>) -> Value {
    match value {
        None => Value::Null,
        Some(m) => json!({
            "amount": decimal(m.amount.as_ref()),
            "currency_code": m.currency_code,
            "instrument_id": m.instrument_id,
        }),
    }
}

fn meta(meta: Option<&ObservationMeta>) -> Value {
    let Some(m) = meta else {
        return Value::Null;
    };
    let mut out = json!({
        "row_key": m.row_key,
        "subjects": m.subjects.iter().map(|s| s.entity_id.clone()).collect::<Vec<_>>(),
        "dataset": m.source.as_ref().map(|s| s.dataset.clone()).unwrap_or_default(),
        "business_date": m.business_date,
        "valid_from_ns": m.valid_from_ns.to_string(),
        "valid_until_ns": m.valid_until_ns.to_string(),
        "recorded_at_ns": m.recorded_at_ns.to_string(),
        "version": m.version,
    });
    // Contract v28 (W10.4, W10.6; ruling 619): the row's effective state
    // and its reasons, absent on a row read as valid that names none.
    let quality = named(m.quality, QualityState::try_from, |e| e.as_str_name());
    if !quality.is_null() {
        out["quality"] = quality;
    }
    if !m.quality_reasons.is_empty() {
        out["quality_reasons"] = Value::Array(
            m.quality_reasons
                .iter()
                .map(|r| named(*r, QualityReason::try_from, |e| e.as_str_name()))
                .collect(),
        );
    }
    out
}

/// A row's fields, the empty and unset left out.
fn kept(pairs: Vec<(&str, Value)>) -> Value {
    let mut out = Map::new();
    for (name, value) in pairs {
        let empty = match &value {
            Value::Null => true,
            Value::String(text) => text.is_empty(),
            Value::Array(items) => items.is_empty(),
            Value::Bool(b) => !b,
            Value::Object(fields) => fields.is_empty(),
            _ => false,
        };
        if !empty {
            out.insert(name.to_string(), value);
        }
    }
    Value::Object(out)
}

fn named<E>(
    value: i32,
    of: impl Fn(i32) -> Result<E, prost::UnknownEnumValue>,
    name: impl Fn(E) -> &'static str,
) -> Value {
    if value == 0 {
        return Value::Null;
    }
    of(value).map_or(Value::Null, |e| Value::String(name(e).to_string()))
}

/// A yield as its source quotes it (contract v29, ruling 685): its value in
/// percent, its convention and its kind, and the date a yield to a call or a
/// put is to.
fn quoted_yield(value: Option<&QuotedYield>) -> Value {
    let Some(y) = value else {
        return Value::Null;
    };
    let convention = y.convention.as_ref().map_or(Value::Null, |c| {
        kept(vec![
            (
                "day_count",
                named(c.day_count, DayCount::try_from, |e| e.as_str_name()),
            ),
            (
                "compounding",
                named(c.compounding, Compounding::try_from, |e| e.as_str_name()),
            ),
            (
                "periods_per_year",
                if c.periods_per_year == 0 {
                    Value::Null
                } else {
                    json!(c.periods_per_year)
                },
            ),
        ])
    });
    kept(vec![
        (
            "value",
            decimal(y.value.as_ref().and_then(|v| v.value.as_ref())),
        ),
        ("convention", convention),
        (
            "kind",
            named(y.kind, YieldKind::try_from, |e| e.as_str_name()),
        ),
        ("to_date", json!(y.to_date)),
    ])
}

pub fn price(p: &Price) -> Value {
    kept(vec![
        ("meta", meta(p.meta.as_ref())),
        (
            "kind",
            named(p.kind, PriceKind::try_from, |e| e.as_str_name()),
        ),
        ("price", money(p.price.as_ref())),
        (
            "basis",
            named(p.basis, PriceBasis::try_from, |e| e.as_str_name()),
        ),
        // Contract v29 (ruling 685): a yield beside it, a spread over its
        // benchmark.
        ("yield", quoted_yield(p.r#yield.as_ref())),
        ("spread_bps", decimal(p.spread_bps.as_ref())),
        ("benchmark_instrument_id", json!(p.benchmark_instrument_id)),
    ])
}

pub fn bar(b: &Bar) -> Value {
    kept(vec![
        ("meta", meta(b.meta.as_ref())),
        ("open", money(b.open.as_ref())),
        ("high", money(b.high.as_ref())),
        ("low", money(b.low.as_ref())),
        ("close", money(b.close.as_ref())),
        ("volume", decimal(b.volume.as_ref())),
        ("vwap", money(b.vwap.as_ref())),
        (
            "trade_count",
            b.trade_count.map_or(Value::Null, |n| json!(n)),
        ),
    ])
}

pub fn trade(t: &Trade) -> Value {
    kept(vec![
        ("meta", meta(t.meta.as_ref())),
        ("price", money(t.price.as_ref())),
        ("quantity", decimal(t.quantity.as_ref())),
        (
            "aggressor",
            named(t.aggressor, Aggressor::try_from, |e| e.as_str_name()),
        ),
        (
            "source_sequence",
            if t.source_sequence == 0 {
                Value::Null
            } else {
                json!(t.source_sequence)
            },
        ),
        ("cancelled", json!(t.cancelled)),
        // Contract v29 (rulings 685 and 900): the characteristics, the basis,
        // a yield beside the price, a spread, and a capped size read as at
        // least.
        (
            "attributes",
            t.attributes.as_ref().map_or(Value::Null, |a| {
                kept(vec![(
                    "characteristics",
                    Value::Array(
                        a.characteristics
                            .iter()
                            .map(|c| named(*c, TradeCharacteristic::try_from, |e| e.as_str_name()))
                            .collect(),
                    ),
                )])
            }),
        ),
        (
            "basis",
            named(t.basis, PriceBasis::try_from, |e| e.as_str_name()),
        ),
        ("yield", quoted_yield(t.r#yield.as_ref())),
        ("spread_bps", decimal(t.spread_bps.as_ref())),
        ("benchmark_instrument_id", json!(t.benchmark_instrument_id)),
        ("quantity_capped", json!(t.quantity_capped)),
    ])
}

pub fn quote(q: &Quote) -> Value {
    kept(vec![
        ("meta", meta(q.meta.as_ref())),
        ("bid", money(q.bid.as_ref())),
        ("ask", money(q.ask.as_ref())),
        ("bid_quantity", decimal(q.bid_quantity.as_ref())),
        ("ask_quantity", decimal(q.ask_quantity.as_ref())),
        (
            "characteristics",
            Value::Array(
                q.characteristics
                    .iter()
                    .map(|c| named(*c, QuoteCharacteristic::try_from, |e| e.as_str_name()))
                    .collect(),
            ),
        ),
        // Contract v29 (rulings 685 and 900): a bucketed size read as at
        // least, the basis, each side's yield and spread, and the benchmark.
        ("bid_size_capped", json!(q.bid_size_capped)),
        ("ask_size_capped", json!(q.ask_size_capped)),
        (
            "basis",
            named(q.basis, PriceBasis::try_from, |e| e.as_str_name()),
        ),
        ("bid_yield", quoted_yield(q.bid_yield.as_ref())),
        ("bid_spread_bps", decimal(q.bid_spread_bps.as_ref())),
        ("ask_yield", quoted_yield(q.ask_yield.as_ref())),
        ("ask_spread_bps", decimal(q.ask_spread_bps.as_ref())),
        ("benchmark_instrument_id", json!(q.benchmark_instrument_id)),
    ])
}

pub fn outcome(o: &Outcome) -> Value {
    kept(vec![
        ("meta", meta(o.meta.as_ref())),
        // Contract v27 (W10.4): the venue's challenge deadline, 0 unstated.
        (
            "challenge_ends_at_ns",
            if o.challenge_ends_at_ns == 0 {
                Value::Null
            } else {
                json!(o.challenge_ends_at_ns.to_string())
            },
        ),
        (
            "determination",
            named(o.determination, OutcomeDetermination::try_from, |e| {
                e.as_str_name()
            }),
        ),
        (
            "stage",
            named(o.stage, OutcomeStage::try_from, |e| e.as_str_name()),
        ),
        ("settlement_value", money(o.settlement_value.as_ref())),
        (
            "settled_at_ns",
            if o.settled_at_ns == 0 {
                Value::Null
            } else {
                json!(o.settled_at_ns.to_string())
            },
        ),
    ])
}

/// A rate (contract v27, the rates slice): its value with its unit, its
/// convention and its tenor or maturity.
pub fn rate(r: &Rate) -> Value {
    let value = r.value.as_ref().map_or(Value::Null, |v| {
        kept(vec![
            ("value", decimal(v.value.as_ref())),
            (
                "unit",
                named(v.unit, ValueUnit::try_from, |e| e.as_str_name()),
            ),
            ("index_base", json!(v.index_base)),
            ("ucum_code", json!(v.ucum_code)),
        ])
    });
    let convention = r.convention.as_ref().map_or(Value::Null, |c| {
        kept(vec![
            (
                "day_count",
                named(c.day_count, DayCount::try_from, |e| e.as_str_name()),
            ),
            (
                "compounding",
                named(c.compounding, Compounding::try_from, |e| e.as_str_name()),
            ),
            (
                "periods_per_year",
                if c.periods_per_year == 0 {
                    Value::Null
                } else {
                    json!(c.periods_per_year)
                },
            ),
        ])
    });
    let tenor = r.tenor.as_ref().map_or(Value::Null, |t| {
        json!({
            "count": t.count,
            "unit": named(t.unit, TenorUnit::try_from, |e| e.as_str_name()),
        })
    });
    kept(vec![
        ("meta", meta(r.meta.as_ref())),
        (
            "kind",
            named(r.kind, RateKind::try_from, |e| e.as_str_name()),
        ),
        ("value", value),
        ("convention", convention),
        ("tenor", tenor),
        ("maturity_date", json!(r.maturity_date)),
        ("preliminary", json!(r.preliminary)),
        ("volume", money(r.volume.as_ref())),
        (
            "transaction_count",
            r.transaction_count
                .map_or(Value::Null, |n| json!(n.to_string())),
        ),
        (
            "percentiles",
            Value::Array(
                r.percentiles
                    .iter()
                    .map(
                        |p| json!({"percentile": p.percentile, "value": decimal(p.value.as_ref())}),
                    )
                    .collect(),
            ),
        ),
    ])
}

/// An execution cost (contract v27, W10.11) as `dashboard__list_execution_costs`
/// answers it.
pub fn execution_cost(c: &ExecutionCost) -> Value {
    let ns = |value: i64| {
        if value == 0 {
            Value::Null
        } else {
            json!(value.to_string())
        }
    };
    kept(vec![
        ("meta", meta(c.meta.as_ref())),
        ("execution_id", json!(c.execution_id)),
        ("order_id", json!(c.order_id)),
        ("placement_id", json!(c.placement_id)),
        ("account_id", json!(c.account_id)),
        (
            "benchmark",
            named(c.benchmark, TcaBenchmark::try_from, |e| e.as_str_name()),
        ),
        ("benchmark_price", money(c.benchmark_price.as_ref())),
        ("benchmark_at_ns", ns(c.benchmark_at_ns)),
        ("benchmark_dataset", json!(c.benchmark_dataset)),
        ("benchmark_row_key", json!(c.benchmark_row_key)),
        ("fixing_id", json!(c.fixing_id)),
        ("execution_price", money(c.execution_price.as_ref())),
        ("side", named(c.side, Side::try_from, |e| e.as_str_name())),
        ("notional", money(c.notional.as_ref())),
        ("cost_bps", decimal(c.cost_bps.as_ref())),
        ("fees_bps", decimal(c.fees_bps.as_ref())),
        ("financing_bps", decimal(c.financing_bps.as_ref())),
        (
            "liquidity_indicator",
            named(c.liquidity_indicator, LiquidityIndicator::try_from, |e| {
                e.as_str_name()
            }),
        ),
        ("venue_id", json!(c.venue_id)),
        (
            "unanswered",
            named(c.unanswered, TcaUnanswered::try_from, |e| e.as_str_name()),
        ),
        ("staleness_bound_ns", ns(c.staleness_bound_ns)),
        (
            "time_precision",
            named(c.time_precision, TimePrecision::try_from, |e| {
                e.as_str_name()
            }),
        ),
        (
            "reject_count",
            if c.reject_count == 0 {
                Value::Null
            } else {
                json!(c.reject_count)
            },
        ),
        ("time_to_fill_ns", ns(c.time_to_fill_ns)),
    ])
}
