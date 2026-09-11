//! Quantities and money, as exact integers.
//!
//! The wire carries an `i64` scaled by 1e8 and so does the store, so there is no
//! conversion anywhere to get wrong. No float appears in this crate, and none
//! can: the type below has no constructor from one and no conversion into one.
//!
//! # Why a newtype over a plain i64
//!
//! The one mistake an `i64` will not stop you making is adding a quantity to a
//! market value. They are both scaled integers and both compile. This does not,
//! which costs a wrapper and buys the class of bug that is invisible in review
//! and produces a plausible wrong number.
//!
//! # Why the arithmetic is checked
//!
//! A wrapped add on a position is a holding that silently becomes its own
//! negative. At 1e8 scale an `i64` holds about ninety-two billion units, which
//! is a great deal of any real instrument and not a great deal of a hyperinflated
//! currency or a mis-scaled feed. An error is recoverable; a wrap is a wrong
//! number nobody questions.

use std::fmt;

/// The scale every quantity and amount on the wire uses.
pub const SCALE: i64 = 100_000_000;

#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
#[error("{what} overflowed: {left} {operation} {right}")]
pub struct Overflow {
    pub what: &'static str,
    pub operation: &'static str,
    pub left: i64,
    pub right: i64,
}

/// A number of units, scaled by 1e8.
///
/// Negative is meaningful: a short position, or a debit. The fixture says so
/// explicitly, which is why nothing here rejects one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Quantity(i64);

/// An amount of money, scaled by 1e8, in some currency this type does not name.
///
/// The currency travels beside it. Putting it inside would invite adding two
/// amounts in different currencies and getting a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Money(i64);

macro_rules! scaled {
    ($name:ident, $what:literal) => {
        impl $name {
            pub const ZERO: Self = Self(0);

            /// From the wire, which carries the scaled integer directly.
            pub const fn from_scaled(scaled: i64) -> Self {
                Self(scaled)
            }

            /// To the wire. The same integer; there is no conversion.
            pub const fn scaled(self) -> i64 {
                self.0
            }

            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            pub fn checked_add(self, other: Self) -> Result<Self, Overflow> {
                self.0.checked_add(other.0).map(Self).ok_or(Overflow {
                    what: $what,
                    operation: "+",
                    left: self.0,
                    right: other.0,
                })
            }

            pub fn checked_sub(self, other: Self) -> Result<Self, Overflow> {
                self.0.checked_sub(other.0).map(Self).ok_or(Overflow {
                    what: $what,
                    operation: "-",
                    left: self.0,
                    right: other.0,
                })
            }
        }

        impl fmt::Display for $name {
            /// Whole units and eight places, always. A trimmed representation
            /// invites somebody to parse it back as a float.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let sign = if self.0 < 0 { "-" } else { "" };
                let magnitude = self.0.unsigned_abs();
                let units = magnitude / SCALE as u64;
                let fraction = magnitude % SCALE as u64;
                write!(f, "{sign}{units}.{fraction:08}")
            }
        }
    };
}

scaled!(Quantity, "a quantity");
scaled!(Money, "an amount");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_value_survives_the_round_trip() {
        assert_eq!(Quantity::from_scaled(1_250_000_000).scaled(), 1_250_000_000);
        assert_eq!(
            Money::from_scaled(281_250_000_000).scaled(),
            281_250_000_000
        );
    }

    #[test]
    fn a_negative_quantity_is_a_short_position_and_not_an_error() {
        // The fixture says so in as many words.
        let short = Quantity::from_scaled(-500_000_000);
        assert!(short < Quantity::ZERO);
        assert_eq!(short.to_string(), "-5.00000000");
    }

    #[test]
    fn twelve_and_a_half_reads_as_twelve_and_a_half() {
        assert_eq!(
            Quantity::from_scaled(1_250_000_000).to_string(),
            "12.50000000"
        );
        assert_eq!(
            Money::from_scaled(281_250_000_000).to_string(),
            "2812.50000000"
        );
    }

    #[test]
    fn a_fraction_keeps_its_leading_zeros() {
        // 0.00000001 and 0.1 differ by seven orders of magnitude, and a
        // formatter that drops the padding renders both as "0.1".
        assert_eq!(Quantity::from_scaled(1).to_string(), "0.00000001");
        assert_eq!(Quantity::from_scaled(10_000_000).to_string(), "0.10000000");
    }

    #[test]
    fn adding_is_exact_where_a_float_would_not_be() {
        // 0.1 + 0.2 in binary floating point is not 0.3. Here it is.
        let tenth = Quantity::from_scaled(10_000_000);
        let fifth = Quantity::from_scaled(20_000_000);
        assert_eq!(
            tenth.checked_add(fifth).unwrap(),
            Quantity::from_scaled(30_000_000)
        );
    }

    #[test]
    fn an_overflow_is_an_error_rather_than_a_wrap() {
        // A wrapped add turns a large holding into its own negative, and
        // nothing downstream questions a negative: it means a short.
        let huge = Quantity::from_scaled(i64::MAX);
        let failed = huge.checked_add(Quantity::from_scaled(1)).unwrap_err();
        assert_eq!(failed.what, "a quantity");
        assert!(failed.to_string().contains("overflowed"));
    }

    #[test]
    fn subtracting_past_the_floor_is_an_error_too() {
        let least = Money::from_scaled(i64::MIN);
        assert!(least.checked_sub(Money::from_scaled(1)).is_err());
    }
}
