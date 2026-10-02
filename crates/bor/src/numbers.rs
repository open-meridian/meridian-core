//! Quantities and amounts as the book holds them: exact, from the wire and
//! back, summed and never rounded (decisions/023).

use meridian_domain::exact::Exact;
use meridian_domain::v1::Money;
use meridian_pb::v1::Decimal;

use crate::store::StoreError;

/// A quantity the wire carried, refused naming its field when it is unset or
/// outside the range.
pub fn required(field: &str, wire: Option<&Decimal>) -> Result<Exact, StoreError> {
    let wire =
        wire.ok_or_else(|| StoreError::Invalid(format!("{field} is required; unset is not zero")))?;
    read(field, wire)
}

/// A quantity the wire may leave unset, which is unknown and never zero.
pub fn optional(field: &str, wire: Option<&Decimal>) -> Result<Option<Exact>, StoreError> {
    wire.map(|wire| read(field, wire)).transpose()
}

fn read(field: &str, wire: &Decimal) -> Result<Exact, StoreError> {
    Exact::from_wire(wire).map_err(|out| StoreError::Invalid(format!("{field} {out}")))
}

/// The exact sum, or the refusal naming what overflowed.
pub fn add(field: &str, a: Exact, b: Exact) -> Result<Exact, StoreError> {
    a.checked_add(b)
        .map_err(|out| StoreError::Invalid(format!("{field} {out} once summed")))
}

pub fn wire(value: Exact) -> Option<Decimal> {
    Some(value.to_wire())
}

/// An amount's number, refused naming its field.
pub fn amount(field: &str, money: &Money) -> Result<Exact, StoreError> {
    required(&format!("{field}.amount"), money.amount.as_ref())
}

/// Two amounts summed, in one currency or refused.
pub fn add_money(field: &str, a: &Money, b: &Money) -> Result<Money, StoreError> {
    if a.currency_code != b.currency_code {
        return Err(StoreError::Invalid(format!(
            "{field} is in {} and the change in {}; an amount is summed only in its own currency",
            a.currency_code, b.currency_code
        )));
    }
    Ok(Money {
        amount: wire(add(field, amount(field, a)?, amount(field, b)?)?),
        currency_code: a.currency_code.clone(),
    })
}
