//! Exact arithmetic for v26's figures (ENG Q11; the one-API spec's Q8).
//!
//! A decimal is an integer and a scale. The integer is held in the
//! narrowest form it fits: an `i128` (the fast path, checked at every
//! step), a 256-bit [`wide::Wide`], and past that an unbounded
//! [`big::Big`], so no value is ever rounded or refused for its size before
//! the one rounding a figure gets, half to even, at its definition's scale.
//! Historical VaR and ES divide (a return is P_d / P_{d-1}), so they are
//! taken as [`Ratio`]s and rounded the same way, once.
//!
//! No float anywhere here, but [`Dec::to_f64`], which the historical
//! measures use only to choose which days to compute exactly (and bound
//! their error so the choice is never wrong).

pub mod big;
pub mod wide;

use std::cmp::Ordering;

use big::Big;
use wide::Wide;

/// 10^n for n up to 38, the most an i128 holds.
const fn ten(n: u32) -> i128 {
    10i128.pow(n)
}

/// An integer in the narrowest form it fits.
#[derive(Clone, Debug)]
pub enum Num {
    /// The fast path.
    Small(i128),
    /// 256 bits.
    Wide(Wide),
    /// Unbounded.
    Big(Big),
}

impl Num {
    fn as_wide(&self) -> Option<Wide> {
        match self {
            Num::Small(value) => Some(Wide::from_i128(*value)),
            Num::Wide(value) => Some(*value),
            Num::Big(_) => None,
        }
    }

    pub fn to_big(&self) -> Big {
        match self {
            Num::Small(value) => Big::from_i128(*value),
            Num::Wide(value) => value.to_big(),
            Num::Big(value) => value.clone(),
        }
    }

    /// The narrowest form of an unbounded value.
    pub fn from_big(value: Big) -> Num {
        if let Some(small) = value.to_i128() {
            return Num::Small(small);
        }
        match Wide::from_big(&value) {
            Some(wide) => Num::Wide(wide),
            None => Num::Big(value),
        }
    }

    pub fn add(&self, other: &Num) -> Num {
        if let (Num::Small(a), Num::Small(b)) = (self, other) {
            if let Some(sum) = a.checked_add(*b) {
                return Num::Small(sum);
            }
        }
        if let (Some(a), Some(b)) = (self.as_wide(), other.as_wide()) {
            if let Some(sum) = a.checked_add(&b) {
                return Num::Wide(sum);
            }
        }
        Num::Big(self.to_big().add(&other.to_big()))
    }

    pub fn mul(&self, other: &Num) -> Num {
        if let (Num::Small(a), Num::Small(b)) = (self, other) {
            if let Some(product) = a.checked_mul(*b) {
                return Num::Small(product);
            }
        }
        if let (Some(a), Some(b)) = (self.as_wide(), other.as_wide()) {
            if let Some(product) = a.checked_mul(&b) {
                return Num::Wide(product);
            }
        }
        Num::Big(self.to_big().mul(&other.to_big()))
    }

    pub fn negated(&self) -> Num {
        match self {
            Num::Small(value) => match value.checked_neg() {
                Some(negated) => Num::Small(negated),
                None => Num::Wide(Wide::from_i128(*value).negated()),
            },
            Num::Wide(value) => Num::Wide(value.negated()),
            Num::Big(value) => Num::Big(value.negated()),
        }
    }

    pub fn signum(&self) -> i32 {
        match self {
            Num::Small(value) => value.signum() as i32,
            Num::Wide(value) => value.signum(),
            Num::Big(value) => value.signum(),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.signum() == 0
    }

    /// Times 10^n.
    pub fn mul_pow10(&self, n: u32) -> Num {
        let mut out = self.clone();
        let mut left = n;
        while left > 0 {
            let step = left.min(38);
            out = out.mul(&Num::Small(ten(step)));
            left -= step;
        }
        out
    }

    fn cmp_num(&self, other: &Num) -> Ordering {
        match (self, other) {
            (Num::Small(a), Num::Small(b)) => a.cmp(b),
            _ => match (self.as_wide(), other.as_wide()) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => self.to_big().cmp(&other.to_big()),
            },
        }
    }

    /// The decimal digits of the magnitude.
    fn magnitude_digits(&self) -> String {
        match self {
            Num::Small(value) => value.unsigned_abs().to_string(),
            other => other.to_big().magnitude_digits(),
        }
    }
}

/// An exact decimal: `integer` × 10^-`scale`.
#[derive(Clone, Debug)]
pub struct Dec {
    integer: Num,
    scale: u32,
}

impl Dec {
    pub fn zero() -> Dec {
        Dec::from_int(0)
    }

    pub fn from_int(value: i128) -> Dec {
        Dec {
            integer: Num::Small(value),
            scale: 0,
        }
    }

    pub fn new(integer: Num, scale: u32) -> Dec {
        Dec { integer, scale }
    }

    /// The same value held in the 256-bit form from the start: for the
    /// benchmark comparing the fast path with the wide one.
    pub fn widened(&self) -> Dec {
        match self.integer.as_wide() {
            Some(wide) => Dec::new(Num::Wide(wide), self.scale),
            None => self.clone(),
        }
    }

    pub fn integer(&self) -> &Num {
        &self.integer
    }

    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// A plain decimal's text: an optional sign, digits, and optionally a
    /// point and more digits. No exponent, no spaces.
    pub fn parse(text: &str) -> Result<Dec, String> {
        let bad = || format!("{text:?} is not a plain decimal");
        let (negative, body) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (whole, places) = match body.split_once('.') {
            Some((whole, places)) => (whole, places),
            None => (body, ""),
        };
        if (whole.is_empty() && places.is_empty())
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !places.bytes().all(|b| b.is_ascii_digit())
            || (body.contains('.') && places.is_empty())
        {
            return Err(bad());
        }
        let digits = format!("{whole}{places}");
        let digits = if digits.is_empty() {
            "0".to_string()
        } else {
            digits
        };
        let integer = if digits.len() <= 38 {
            Num::Small(digits.parse::<i128>().map_err(|_| bad())?)
        } else {
            Num::from_big(Big::from_digits(&digits).ok_or_else(bad)?)
        };
        let integer = if negative { integer.negated() } else { integer };
        Ok(Dec {
            integer,
            scale: places.len() as u32,
        })
    }

    fn aligned(&self, other: &Dec) -> (Num, Num, u32) {
        match self.scale.cmp(&other.scale) {
            Ordering::Equal => (self.integer.clone(), other.integer.clone(), self.scale),
            Ordering::Less => (
                self.integer.mul_pow10(other.scale - self.scale),
                other.integer.clone(),
                other.scale,
            ),
            Ordering::Greater => (
                self.integer.clone(),
                other.integer.mul_pow10(self.scale - other.scale),
                self.scale,
            ),
        }
    }

    pub fn add(&self, other: &Dec) -> Dec {
        if self.scale == other.scale {
            return Dec::new(self.integer.add(&other.integer), self.scale);
        }
        let (a, b, scale) = self.aligned(other);
        Dec::new(a.add(&b), scale)
    }

    pub fn sub(&self, other: &Dec) -> Dec {
        self.add(&other.negated())
    }

    pub fn mul(&self, other: &Dec) -> Dec {
        Dec::new(self.integer.mul(&other.integer), self.scale + other.scale)
    }

    pub fn negated(&self) -> Dec {
        Dec::new(self.integer.negated(), self.scale)
    }

    pub fn abs(&self) -> Dec {
        if self.signum() < 0 {
            self.negated()
        } else {
            self.clone()
        }
    }

    /// Half of the value, exactly: times 5 at one more place.
    pub fn half(&self) -> Dec {
        Dec::new(self.integer.mul(&Num::Small(5)), self.scale + 1)
    }

    pub fn signum(&self) -> i32 {
        self.integer.signum()
    }

    pub fn is_zero(&self) -> bool {
        self.integer.is_zero()
    }

    /// Rounded once, half to even, to `scale` places (Q11).
    pub fn round_half_even(&self, scale: u32) -> Dec {
        if self.scale <= scale {
            return Dec::new(self.integer.mul_pow10(scale - self.scale), scale);
        }
        let drop = self.scale - scale;
        if let (Num::Small(value), true) = (&self.integer, drop <= 38) {
            let divisor = ten(drop);
            let mut whole = value.div_euclid(divisor);
            let rest = value.rem_euclid(divisor) as u128;
            let twice = 2 * rest;
            if twice > divisor as u128 || (twice == divisor as u128 && whole % 2 != 0) {
                whole += 1;
            }
            return Dec::new(Num::Small(whole), scale);
        }
        let rounded = round_quotient(&self.integer.to_big(), &Big::pow10(drop));
        Dec::new(Num::from_big(rounded), scale)
    }

    /// The decimal's stated text: its sign, its integer digits ("0" if
    /// none) and exactly `scale` places; never minus zero.
    pub fn text(&self) -> String {
        let digits = self.integer.magnitude_digits();
        let places = self.scale as usize;
        let body = if places == 0 {
            digits
        } else {
            let padded = format!("{digits:0>width$}", width = places + 1);
            let (whole, fraction) = padded.split_at(padded.len() - places);
            format!("{whole}.{fraction}")
        };
        if self.signum() < 0 {
            format!("-{body}")
        } else {
            body
        }
    }

    /// The nearest double: only to choose which historical days to compute
    /// exactly, never as a figure.
    pub fn to_f64(&self) -> f64 {
        match (&self.integer, self.scale) {
            // The integer, then its scale: within two roundings of the value.
            (Num::Small(value), scale) if scale <= 22 => *value as f64 / 10f64.powi(scale as i32),
            _ => self.text().parse::<f64>().unwrap_or(f64::NAN),
        }
    }

    pub fn to_ratio(&self) -> Ratio {
        Ratio {
            numerator: self.integer.to_big(),
            denominator: Big::pow10(self.scale),
        }
    }
}

impl PartialEq for Dec {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Dec {}

impl PartialOrd for Dec {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Dec {
    fn cmp(&self, other: &Self) -> Ordering {
        let (a, b, _) = self.aligned(other);
        a.cmp_num(&b)
    }
}

/// `numerator / denominator` rounded half to even to a whole number, the
/// denominator positive.
fn round_quotient(numerator: &Big, denominator: &Big) -> Big {
    let (mut whole, rest) = numerator.divmod_floor(denominator);
    let twice = rest.mul_small(2);
    match twice.cmp(denominator) {
        Ordering::Greater => whole = whole.add(&Big::from_u128(1)),
        Ordering::Equal if whole.is_odd() => whole = whole.add(&Big::from_u128(1)),
        _ => {}
    }
    whole
}

/// An exact rational, its denominator positive. Not reduced: compared by
/// cross-multiplication, and rounded once.
#[derive(Clone, Debug)]
pub struct Ratio {
    numerator: Big,
    denominator: Big,
}

impl Ratio {
    pub fn zero() -> Ratio {
        Ratio {
            numerator: Big::zero(),
            denominator: Big::from_u128(1),
        }
    }

    pub fn new(numerator: Big, denominator: Big) -> Ratio {
        assert!(!denominator.is_zero(), "a ratio over zero");
        if denominator.is_negative() {
            Ratio {
                numerator: numerator.negated(),
                denominator: denominator.negated(),
            }
        } else {
            Ratio {
                numerator,
                denominator,
            }
        }
    }

    pub fn add(&self, other: &Ratio) -> Ratio {
        if self.denominator == other.denominator {
            return Ratio {
                numerator: self.numerator.add(&other.numerator),
                denominator: self.denominator.clone(),
            };
        }
        Ratio {
            numerator: self
                .numerator
                .mul(&other.denominator)
                .add(&other.numerator.mul(&self.denominator)),
            denominator: self.denominator.mul(&other.denominator),
        }
    }

    pub fn sub(&self, other: &Ratio) -> Ratio {
        self.add(&other.negated())
    }

    pub fn mul(&self, other: &Ratio) -> Ratio {
        Ratio {
            numerator: self.numerator.mul(&other.numerator),
            denominator: self.denominator.mul(&other.denominator),
        }
    }

    /// Divided by `other`, which is not zero.
    pub fn div(&self, other: &Ratio) -> Ratio {
        Ratio::new(
            self.numerator.mul(&other.denominator),
            self.denominator.mul(&other.numerator),
        )
    }

    pub fn div_small(&self, divisor: u32) -> Ratio {
        Ratio {
            numerator: self.numerator.clone(),
            denominator: self.denominator.mul_small(divisor),
        }
    }

    pub fn negated(&self) -> Ratio {
        Ratio {
            numerator: self.numerator.negated(),
            denominator: self.denominator.clone(),
        }
    }

    pub fn signum(&self) -> i32 {
        self.numerator.signum()
    }

    /// Rounded once, half to even, to `scale` places.
    pub fn round_half_even(&self, scale: u32) -> Dec {
        let shifted = self.numerator.mul(&Big::pow10(scale));
        Dec::new(
            Num::from_big(round_quotient(&shifted, &self.denominator)),
            scale,
        )
    }

    /// Rounded once, half away from zero, to `scale` places: 31 CFR 356
    /// Appendix B's rounding of a dirty price and its accrued interest
    /// (contract v29).
    pub fn round_half_up(&self, scale: u32) -> Dec {
        self.toward(scale, true)
    }

    /// Cut toward zero at `scale` places: MSRB Rule G-33's yield, truncated
    /// to four places before it is rounded to three (contract v29).
    pub fn truncate(&self, scale: u32) -> Dec {
        self.toward(scale, false)
    }

    fn toward(&self, scale: u32, half_up: bool) -> Dec {
        let shifted = self.numerator.mul(&Big::pow10(scale)).abs();
        let (mut whole, rest) = shifted.divmod_floor(&self.denominator);
        if half_up && rest.mul_small(2).cmp(&self.denominator) != Ordering::Less {
            whole = whole.add(&Big::from_u128(1));
        }
        if self.numerator.is_negative() {
            whole = whole.negated();
        }
        Dec::new(Num::from_big(whole), scale)
    }

    /// The nearest double, for a model's floating-point inputs (contract
    /// v29): never a figure.
    pub fn to_f64(&self) -> f64 {
        self.round_half_even(18).to_f64()
    }
}

impl PartialEq for Ratio {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ratio {}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ratio {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numerator
            .mul(&other.denominator)
            .cmp(&other.numerator.mul(&self.denominator))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(text: &str) -> Dec {
        Dec::parse(text).unwrap()
    }

    #[test]
    fn text_is_the_stated_form() {
        for (text, out) in [
            ("1.50", "1.50"),
            ("-0.005", "-0.005"),
            ("0", "0"),
            ("-0.00", "0.00"),
            (".5", "0.5"),
            ("+7", "7"),
            ("00012.300", "12.300"),
        ] {
            assert_eq!(d(text).text(), out, "{text}");
        }
        for bad in ["", "1.", "1e5", "1.2.3", " 1", "-", "abc"] {
            assert!(Dec::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rounding_is_once_and_half_to_even() {
        for (value, scale, out) in [
            ("2.345", 2, "2.34"),
            ("2.355", 2, "2.36"),
            ("-2.345", 2, "-2.34"),
            ("-2.355", 2, "-2.36"),
            ("-0.004", 2, "0.00"),
            ("0.125", 2, "0.12"),
            ("0.1251", 2, "0.13"),
            ("7", 2, "7.00"),
            ("1.5", 0, "2"),
            ("2.5", 0, "2"),
            ("-2.5", 0, "-2"),
            ("-3.5", 0, "-4"),
        ] {
            assert_eq!(d(value).round_half_even(scale).text(), out, "{value}");
        }
        // Past 38 digits the same rule holds, in the unbounded form.
        let long = format!("{}5", "1".repeat(60));
        let rounded = Dec::parse(&format!("{}.{}", &long[..40], &long[40..]))
            .unwrap()
            .round_half_even(2);
        assert_eq!(rounded.text(), format!("{}.{}", &long[..40], "11"));
    }

    #[test]
    fn the_fast_path_the_wide_and_the_unbounded_agree() {
        let a = d("123456789012345678.123456789012345678");
        let b = d("987654321098765432.987654321098765432");
        let narrow = a.mul(&b).mul(&a);
        let wide = a.widened().mul(&b.widened()).mul(&a.widened());
        assert_eq!(narrow.text(), wide.text());
        assert!(matches!(narrow.integer(), Num::Big(_)));
        let sum = narrow.add(&narrow.negated());
        assert!(sum.is_zero());
        assert_eq!(d("1.10"), d("1.1"));
        assert!(d("-1") < d("0.5"));
    }

    #[test]
    fn a_ratio_rounds_as_python_does() {
        // 1/3, 2/3, -1/8 at two places; -1/8 is -0.125, half to even -0.12.
        let third = Ratio::new(Big::from_i128(1), Big::from_i128(3));
        assert_eq!(third.round_half_even(2).text(), "0.33");
        assert_eq!(third.add(&third).round_half_even(2).text(), "0.67");
        let eighth = Ratio::new(Big::from_i128(-1), Big::from_i128(8));
        assert_eq!(eighth.round_half_even(2).text(), "-0.12");
        assert!(eighth < third);
        assert_eq!(
            d("1.5").to_ratio().div_small(3).round_half_even(1).text(),
            "0.5"
        );
    }
}
