//! A number as the runtime holds it: an `i128` and its scale.
//!
//! The one hand-written type in this crate, and here rather than in a
//! component because every component that reads a value -- the sidecar that
//! refuses one out of range, the street store that keeps it -- must read it
//! the same way. `meridian.v1.Decimal` is its wire form (decisions/023).
//!
//! # What it is not
//!
//! Not arithmetic beyond sums. It compares, formats and parses; and since the
//! book of record (W9) sums movement lines, it adds and negates, exactly, at
//! the larger of the two scales, refusing a sum the wire could not carry
//! rather than rounding it. Nothing multiplies or divides. No constructor
//! takes a float and no conversion makes one.
//!
//! # Equal is numeric
//!
//! 1.5 and 1.50 are one number stated two ways, so they compare equal and
//! hash alike. The scale is kept -- a value is displayed and stored as it was
//! stated -- and never decides whether two values are the same.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use meridian_pb::v1::Decimal;

/// The most decimal places a value carries.
pub const MAX_SCALE: u32 = 18;

/// The integer's magnitude stays below this: at most 38 significant digits.
const LIMIT: u128 = 100_000_000_000_000_000_000_000_000_000_000_000_000;

/// 10^n for n up to [`MAX_SCALE`], which is all a comparison needs.
const fn ten_to(n: u32) -> u128 {
    10u128.pow(n)
}

/// Why a value cannot be an [`Exact`]. Says what is wrong with the value and
/// not which field held it; the caller names the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutOfRange {
    /// More decimal places than the wire carries.
    Scale(u32),
    /// An integer of 38 digits or more.
    Digits,
    /// Text that is not a plain decimal, such as `1e5` or `1.`.
    NotDecimal(String),
}

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutOfRange::Scale(scale) => write!(
                f,
                "has {scale} decimal places, and at most {MAX_SCALE} cross the wire"
            ),
            OutOfRange::Digits => write!(f, "has more than 38 digits"),
            OutOfRange::NotDecimal(text) => write!(f, "{text:?} is not a decimal"),
        }
    }
}

impl std::error::Error for OutOfRange {}

/// An exact decimal: `integer` times 10^-`scale`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Exact {
    integer: i128,
    scale: u32,
}

impl Exact {
    pub const ZERO: Self = Self {
        integer: 0,
        scale: 0,
    };

    /// Refused outside the range rather than clamped or rounded.
    pub fn new(integer: i128, scale: u32) -> Result<Self, OutOfRange> {
        if scale > MAX_SCALE {
            return Err(OutOfRange::Scale(scale));
        }
        if integer.unsigned_abs() >= LIMIT {
            return Err(OutOfRange::Digits);
        }
        Ok(Self { integer, scale })
    }

    /// From the wire: the integer's two halves put back together.
    pub fn from_wire(decimal: &Decimal) -> Result<Self, OutOfRange> {
        // `low` is unsigned, so widening it zero-extends, and the signed
        // `high` above it carries the sign: two's complement across 128 bits.
        let integer = ((decimal.high as i128) << 64) | (decimal.low as i128);
        Self::new(integer, decimal.scale)
    }

    /// To the wire, with the scale it was stated with.
    pub fn to_wire(self) -> Decimal {
        Decimal {
            high: (self.integer >> 64) as i64,
            low: self.integer as u64,
            scale: self.scale,
        }
    }

    pub fn integer(self) -> i128 {
        self.integer
    }

    pub fn scale(self) -> u32 {
        self.scale
    }

    pub fn is_zero(self) -> bool {
        self.integer == 0
    }

    /// The same number with its sign turned: a line reversed (W9.7).
    pub fn negated(self) -> Self {
        Self {
            integer: -self.integer,
            scale: self.scale,
        }
    }

    /// The exact sum, at the larger of the two scales: 12.5 and 2.50 is
    /// 15.00. Refused, never rounded, when it is outside what the wire
    /// carries.
    pub fn checked_add(self, other: Self) -> Result<Self, OutOfRange> {
        let scale = self.scale.max(other.scale);
        let widen = |value: Self| {
            value
                .integer
                .checked_mul(ten_to(scale - value.scale) as i128)
                .ok_or(OutOfRange::Digits)
        };
        let sum = widen(self)?
            .checked_add(widen(other)?)
            .ok_or(OutOfRange::Digits)?;
        Self::new(sum, scale)
    }

    /// The exact difference, as [`Exact::checked_add`] of the negation.
    pub fn checked_sub(self, other: Self) -> Result<Self, OutOfRange> {
        self.checked_add(other.negated())
    }

    /// Below zero.
    pub fn is_negative(self) -> bool {
        self.integer < 0
    }

    /// Whole units and the fraction, each as an unsigned magnitude, the
    /// fraction widened to eighteen places. Two values compare by these
    /// without either being multiplied up, which at 38 digits and a scale
    /// difference of 18 would overflow even an `i128`.
    fn parts(self) -> (u128, u128) {
        let magnitude = self.integer.unsigned_abs();
        let unit = ten_to(self.scale);
        (
            magnitude / unit,
            (magnitude % unit) * ten_to(MAX_SCALE - self.scale),
        )
    }

    /// The same number at the least scale that states it, for hashing.
    fn trimmed(self) -> (i128, u32) {
        let (mut integer, mut scale) = (self.integer, self.scale);
        while scale > 0 && integer % 10 == 0 {
            integer /= 10;
            scale -= 1;
        }
        (integer, scale)
    }
}

impl PartialEq for Exact {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Exact {}

impl PartialOrd for Exact {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Exact {
    fn cmp(&self, other: &Self) -> Ordering {
        let sign = |value: &Exact| value.integer.signum();
        match sign(self).cmp(&sign(other)) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        let magnitudes = self.parts().cmp(&other.parts());
        if self.integer < 0 {
            magnitudes.reverse()
        } else {
            magnitudes
        }
    }
}

impl Hash for Exact {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.trimmed().hash(state);
    }
}

impl fmt::Display for Exact {
    /// As it was stated: 150 at scale 2 is `1.50`, and never `1.5` or `1.5e0`.
    /// What logs, pages and Postgres read.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.integer < 0 { "-" } else { "" };
        let digits = self.integer.unsigned_abs().to_string();
        let scale = self.scale as usize;
        if scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        // Padded so a fraction keeps its leading zeros: 1 at scale 9 is
        // 0.000000001, where dropping the padding would print 0.1.
        let digits = format!("{digits:0>width$}", width = scale + 1);
        let (whole, fraction) = digits.split_at(digits.len() - scale);
        write!(f, "{sign}{whole}.{fraction}")
    }
}

impl FromStr for Exact {
    type Err = OutOfRange;

    /// A plain decimal, `-12.50`, as Postgres writes a `numeric` and a person
    /// writes a number. The scale is the number of places written.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let not_decimal = || OutOfRange::NotDecimal(text.to_string());
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let plain = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty()
            || !plain(whole)
            || !plain(fraction)
            || (unsigned.contains('.') && fraction.is_empty())
        {
            return Err(not_decimal());
        }
        let scale = u32::try_from(fraction.len()).map_err(|_| not_decimal())?;
        if scale > MAX_SCALE {
            return Err(OutOfRange::Scale(scale));
        }
        let digits = format!("{whole}{fraction}");
        let significant = digits.trim_start_matches('0');
        if significant.len() > 38 {
            return Err(OutOfRange::Digits);
        }
        let magnitude: i128 = if significant.is_empty() {
            0
        } else {
            significant.parse().map_err(|_| not_decimal())?
        };
        Self::new(if negative { -magnitude } else { magnitude }, scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(text: &str) -> Exact {
        text.parse().unwrap()
    }

    #[test]
    fn a_value_reads_back_as_it_was_stated() {
        for text in [
            "0",
            "1.5",
            "1.50",
            "-5",
            "0.000000001",
            "100000000000",
            "100000000000.000000000000000001",
            "-0.10",
        ] {
            assert_eq!(exact(text).to_string(), text);
        }
    }

    #[test]
    fn the_wire_form_survives_the_round_trip_at_every_width() {
        for text in [
            "12.5",
            "-12.5",
            "0.000000001",
            "100000000000",
            "100000000000.000000000000000001",
            "-99999999999999999999.999999999999999999",
            "99999999999999999999999999999999999999",
        ] {
            let value = exact(text);
            let wire = value.to_wire();
            let back = Exact::from_wire(&wire).unwrap();
            assert_eq!(back.to_string(), text);
            assert_eq!(back.scale(), value.scale());
        }
    }

    #[test]
    fn a_negative_integer_carries_its_sign_in_the_high_half() {
        let wire = exact("-1").to_wire();
        assert_eq!((wire.high, wire.low), (-1, u64::MAX));
    }

    #[test]
    fn one_and_a_half_equals_one_point_five_zero_but_keeps_its_scale() {
        assert_eq!(exact("1.5"), exact("1.50"));
        assert_eq!(exact("1.50").scale(), 2);
        let hash = |value: Exact| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            value.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(exact("1.5")), hash(exact("1.500")));
    }

    #[test]
    fn comparison_is_numeric_across_scales_at_the_widest_values() {
        // Aligning these by multiplying the smaller scale up would overflow
        // an i128; comparing whole units and fractions does not.
        let wide = exact("99999999999999999999999999999999999999");
        let fine = exact("0.000000000000000001");
        assert!(wide > fine);
        assert!(exact("-0.000000000000000001") < exact("0"));
        assert!(exact("-2") < exact("-1.999999999999999999"));
        assert!(exact("100000000000.000000000000000001") > exact("100000000000"));
        assert!(exact("0.1") > exact("0.09"));
    }

    #[test]
    fn a_nineteenth_place_is_refused_rather_than_rounded() {
        assert_eq!(
            "0.0000000000000000001".parse::<Exact>(),
            Err(OutOfRange::Scale(19))
        );
        let wire = Decimal {
            high: 0,
            low: 1,
            scale: 19,
        };
        assert_eq!(Exact::from_wire(&wire), Err(OutOfRange::Scale(19)));
    }

    #[test]
    fn a_thirty_ninth_digit_is_refused() {
        assert_eq!(
            "100000000000000000000000000000000000000".parse::<Exact>(),
            Err(OutOfRange::Digits)
        );
        let at_limit = Decimal {
            high: (LIMIT >> 64) as i64,
            low: LIMIT as u64,
            scale: 0,
        };
        assert_eq!(Exact::from_wire(&at_limit), Err(OutOfRange::Digits));
        let least = Decimal {
            high: i64::MIN,
            low: 0,
            scale: 0,
        };
        assert_eq!(Exact::from_wire(&least), Err(OutOfRange::Digits));
    }

    #[test]
    fn text_that_is_not_a_plain_decimal_is_refused() {
        for text in ["", "-", "1e5", "1.", ".5", "1.2.3", " 1", "+1", "NaN"] {
            assert!(
                matches!(text.parse::<Exact>(), Err(OutOfRange::NotDecimal(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn leading_zeros_are_not_digits() {
        assert_eq!(
            exact("000000000000000000000000000000000000000001"),
            exact("1")
        );
    }

    #[test]
    fn a_sum_is_exact_at_the_larger_scale() {
        let sum = exact("12.5").checked_add(exact("2.50")).unwrap();
        assert_eq!(sum.to_string(), "15.00");
        assert_eq!(
            exact("1000.00").checked_sub(exact("1000")).unwrap(),
            Exact::ZERO
        );
        assert_eq!(exact("-2.5").negated().to_string(), "2.5");
        assert!(exact("-0.1").is_negative());
    }

    #[test]
    fn a_sum_the_wire_cannot_carry_is_refused_not_rounded() {
        let most = exact("99999999999999999999999999999999999999");
        assert_eq!(most.checked_add(exact("1")), Err(OutOfRange::Digits));
        // Widening to the larger scale can itself overflow the 38 digits.
        assert!(most.checked_add(exact("0.000000000000000001")).is_err());
    }
}
