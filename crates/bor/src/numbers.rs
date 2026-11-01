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
        instrument_id: String::new(),
    })
}

/// An amount to the cent, half-even (contract v29, ruling 993: a pool's or a
/// tranche's fill's cash rounds at booking by the book's money rule): exact
/// where it ends within two places, else the nearer cent, exactly half to the
/// even one.
pub fn cents(field: &str, value: Exact) -> Result<Exact, StoreError> {
    let scale = value.scale();
    if scale <= 2 {
        return Ok(value);
    }
    let power = 10i128
        .checked_pow(scale - 2)
        .ok_or_else(|| StoreError::Invalid(format!("{field} {value} does not round")))?;
    let integer = value.integer();
    let (n, d) = (integer.unsigned_abs(), power.unsigned_abs());
    let (mut q, r) = (n / d, n % d);
    if r * 2 > d || (r * 2 == d && q % 2 == 1) {
        q += 1;
    }
    let q = i128::try_from(q)
        .map_err(|_| StoreError::Invalid(format!("{field} {value} does not round")))?;
    Exact::new(if integer < 0 { -q } else { q }, 2)
        .map_err(|out| StoreError::Invalid(format!("{field} {out}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pools_cash_rounds_to_the_cent_half_even() {
        let e = |t: &str| t.parse::<Exact>().unwrap();
        assert_eq!(cents("cash", e("247450.0000000")).unwrap(), e("247450.00"));
        assert_eq!(cents("cash", e("8421.1375")).unwrap(), e("8421.14"));
        assert_eq!(cents("cash", e("0.125")).unwrap(), e("0.12"));
        assert_eq!(cents("cash", e("0.135")).unwrap(), e("0.14"));
        assert_eq!(cents("cash", e("-0.125")).unwrap(), e("-0.12"));
        assert_eq!(cents("cash", e("-1.2351")).unwrap(), e("-1.24"));
        assert_eq!(cents("cash", e("12.5")).unwrap(), e("12.5"));
    }
}
