//! The lake's rows as `dashboard__query_lake` answers them (contract v26,
//! W10.10): each field by its proto name, a decimal as a string, a time in
//! nanoseconds as a decimal string, an enum by its value's name; a field
//! the query did not name, or the instance may not read, absent.

use meridian_domain::exact::Exact;
use meridian_domain::v1::{
    Aggressor, Bar, Money, ObservationMeta, Outcome, OutcomeDetermination, OutcomeStage, Price,
    PriceBasis, PriceKind, Quote, QuoteCharacteristic, Trade,
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
    json!({
        "row_key": m.row_key,
        "subjects": m.subjects.iter().map(|s| s.entity_id.clone()).collect::<Vec<_>>(),
        "dataset": m.source.as_ref().map(|s| s.dataset.clone()).unwrap_or_default(),
        "business_date": m.business_date,
        "valid_from_ns": m.valid_from_ns.to_string(),
        "valid_until_ns": m.valid_until_ns.to_string(),
        "recorded_at_ns": m.recorded_at_ns.to_string(),
        "version": m.version,
    })
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
    ])
}

pub fn outcome(o: &Outcome) -> Value {
    kept(vec![
        ("meta", meta(o.meta.as_ref())),
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
