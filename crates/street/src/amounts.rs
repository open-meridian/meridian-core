//! Quantities and money, as exact decimals.
//!
//! The wire carries a `meridian.v1.Decimal`, an integer and its own scale, and
//! a `meridian.v1.Money`, one of those and its currency (decisions/023). The
//! store keeps them as Postgres `numeric`, which is the same thing in a column,
//! so a value is kept at the scale it was stated with and read back as stated.
//! No float appears in this crate, and none can: [`Exact`] has no constructor
//! from one and no conversion into one.
//!
//! # Why a quantity is its own type
//!
//! The one mistake two bare numbers will not stop you making is putting a
//! quantity where a market value belongs. This does not compile, which costs a
//! wrapper and buys the class of bug that is invisible in review and produces
//! a plausible wrong number.
//!
//! # Why money carries its currency
//!
//! An amount without its currency is a number nobody can compare, and two
//! amounts side by side with their currencies beside them can be paired wrong.
//! So the currency is inside, as it is on the wire.
//!
//! # Refused, never rounded
//!
//! A value from the wire is checked as it is read -- 18 decimal places and 38
//! digits at most -- and refused naming its field. The sidecar has already
//! refused one from a plugin; this is every other way a message can arrive.

use std::fmt;
use std::str::FromStr;

use meridian_domain::v1::{Decimal, Money as WireMoney};

pub use meridian_domain::exact::{Exact, OutOfRange};

/// A value on its way in that the wire form does not allow, and which field it
/// was in.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field} {why}; it is refused rather than rounded")]
pub struct Refused {
    pub field: &'static str,
    pub why: OutOfRange,
}

/// A number of units, at the scale it was stated with.
///
/// Negative is meaningful: a short position, or a debit. The fixture says so
/// explicitly, which is why nothing here rejects one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Quantity(Exact);

impl Quantity {
    pub const ZERO: Self = Self(Exact::ZERO);

    pub const fn new(value: Exact) -> Self {
        Self(value)
    }

    pub fn exact(self) -> Exact {
        self.0
    }

    pub fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// From the wire. Unset is zero, which is what an unset quantity has
    /// always meant here.
    pub fn from_wire(field: &'static str, wire: Option<&Decimal>) -> Result<Self, Refused> {
        match wire {
            None => Ok(Self::ZERO),
            Some(wire) => Exact::from_wire(wire)
                .map(Self)
                .map_err(|why| Refused { field, why }),
        }
    }

    pub fn to_wire(self) -> Option<Decimal> {
        Some(self.0.to_wire())
    }
}

impl FromStr for Quantity {
    type Err = OutOfRange;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse().map(Self)
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// An amount and the currency it is in.
///
/// Two amounts are equal when the numbers are and the currencies are: 2812.5
/// USD is 2812.50 USD, and is not 2812.50 EUR.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Money {
    pub amount: Exact,

    /// ISO 4217. Empty where the rail stated no value, which is recorded as a
    /// zero in no currency rather than invented.
    pub currency: String,
}

impl Money {
    pub fn new(amount: Exact, currency: impl Into<String>) -> Self {
        Self {
            amount,
            currency: currency.into(),
        }
    }

    /// From the wire. Unset is zero in no currency, as an unset value and an
    /// empty currency were before the currency moved inside the amount.
    pub fn from_wire(field: &'static str, wire: Option<&WireMoney>) -> Result<Self, Refused> {
        let Some(wire) = wire else {
            return Ok(Self::default());
        };
        let amount = match &wire.amount {
            None => Exact::ZERO,
            Some(amount) => Exact::from_wire(amount).map_err(|why| Refused { field, why })?,
        };
        Ok(Self::new(amount, wire.currency_code.clone()))
    }

    pub fn to_wire(&self) -> Option<WireMoney> {
        Some(WireMoney {
            amount: Some(self.amount.to_wire()),
            currency_code: self.currency.clone(),
        })
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.amount, self.currency)
    }
}

/// Numbers as the tests write them: the decimal a person reads, put on the
/// wire the way a sender would, and read back off it.
#[cfg(test)]
pub(crate) mod testing {
    use meridian_domain::v1::{Decimal, Money as WireMoney};

    use super::{Exact, Money};

    pub(crate) fn quantity(text: &str) -> Option<Decimal> {
        Some(text.parse::<Exact>().unwrap().to_wire())
    }

    pub(crate) fn usd(text: &str) -> Option<WireMoney> {
        Money::new(text.parse().unwrap(), "USD").to_wire()
    }

    pub(crate) fn read(wire: &Option<Decimal>) -> String {
        Exact::from_wire(wire.as_ref().expect("a quantity"))
            .unwrap()
            .to_string()
    }

    pub(crate) fn read_money(wire: &Option<WireMoney>) -> String {
        Money::from_wire("market_value", wire.as_ref())
            .unwrap()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(text: &str) -> Exact {
        text.parse().unwrap()
    }

    #[test]
    fn the_wire_value_survives_the_round_trip_at_its_own_scale() {
        let quantity: Quantity = "12.50".parse().unwrap();
        let back = Quantity::from_wire("quantity", quantity.to_wire().as_ref()).unwrap();
        assert_eq!(back.to_string(), "12.50");

        let value = Money::new(exact("2812.50"), "USD");
        let back = Money::from_wire("market_value", value.to_wire().as_ref()).unwrap();
        assert_eq!(back, value);
        assert_eq!(back.to_string(), "2812.50 USD");
    }

    #[test]
    fn a_negative_quantity_is_a_short_position_and_not_an_error() {
        // The fixture says so in as many words.
        let short: Quantity = "-5".parse().unwrap();
        assert!(short < Quantity::ZERO);
        assert_eq!(short.to_string(), "-5");
    }

    #[test]
    fn a_fraction_keeps_its_leading_zeros() {
        // 0.000000001 and 0.1 differ by eight orders of magnitude, and a
        // formatter that drops the padding renders both as "0.1".
        let tiny: Quantity = "0.000000001".parse().unwrap();
        assert_eq!(tiny.to_string(), "0.000000001");
    }

    #[test]
    fn the_same_amount_in_another_currency_is_another_amount() {
        assert_eq!(
            Money::new(exact("2812.5"), "USD"),
            Money::new(exact("2812.50"), "USD")
        );
        assert_ne!(
            Money::new(exact("2812.50"), "USD"),
            Money::new(exact("2812.50"), "EUR")
        );
    }

    #[test]
    fn a_value_the_wire_does_not_carry_is_refused_naming_its_field() {
        let nineteenth = Decimal {
            high: 0,
            low: 1,
            scale: 19,
        };
        let refused = Quantity::from_wire("quantity", Some(&nineteenth)).unwrap_err();
        assert_eq!(refused.field, "quantity");
        assert!(
            refused
                .to_string()
                .starts_with("quantity has 19 decimal places"),
            "{refused}"
        );

        let wide = WireMoney {
            amount: Some(Decimal {
                high: i64::MAX,
                low: 0,
                scale: 0,
            }),
            currency_code: "USD".into(),
        };
        let refused = Money::from_wire("market_value", Some(&wide)).unwrap_err();
        assert_eq!(refused.why, OutOfRange::Digits);
    }

    #[test]
    fn unset_is_zero_as_it_always_was() {
        assert_eq!(
            Quantity::from_wire("quantity", None).unwrap(),
            Quantity::ZERO
        );
        assert_eq!(
            Money::from_wire("market_value", None).unwrap(),
            Money::default()
        );
    }
}
