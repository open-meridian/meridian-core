//! A signed 256-bit integer, checked: the engine's wide intermediate (ENG
//! Q11, "256-bit intermediates with a checked 128-bit fast path"). A
//! quantity at 18 places times a price at 18 places needs 36 places, past
//! 128 bits for a large holding; 256 bits hold 77 digits.
//!
//! Sign and magnitude, the magnitude in four 64-bit words, least
//! significant first. Every operation is checked: one that would not fit
//! answers None, and the caller carries on exactly in [`super::big::Big`].
//! Fixed size, so it never allocates.

use std::cmp::Ordering;

use super::big::Big;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Wide {
    negative: bool,
    magnitude: [u64; 4],
}

fn compare(a: &[u64; 4], b: &[u64; 4]) -> Ordering {
    for i in (0..4).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

fn add(a: &[u64; 4], b: &[u64; 4]) -> Option<[u64; 4]> {
    let mut out = [0u64; 4];
    let mut carry = 0u128;
    for i in 0..4 {
        let sum = a[i] as u128 + b[i] as u128 + carry;
        out[i] = sum as u64;
        carry = sum >> 64;
    }
    (carry == 0).then_some(out)
}

/// `a - b`, where `a >= b`.
fn subtract(a: &[u64; 4], b: &[u64; 4]) -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut borrow = false;
    for i in 0..4 {
        let (step, first) = a[i].overflowing_sub(b[i]);
        let (step, second) = step.overflowing_sub(borrow as u64);
        out[i] = step;
        borrow = first || second;
    }
    out
}

fn multiply(a: &[u64; 4], b: &[u64; 4]) -> Option<[u64; 4]> {
    let mut out = [0u64; 8];
    for i in 0..4 {
        if a[i] == 0 {
            continue;
        }
        let mut carry = 0u128;
        for j in 0..4 {
            let product = a[i] as u128 * b[j] as u128 + out[i + j] as u128 + carry;
            out[i + j] = product as u64;
            carry = product >> 64;
        }
        out[i + 4] = carry as u64;
    }
    if out[4..].iter().any(|&word| word != 0) {
        return None;
    }
    Some([out[0], out[1], out[2], out[3]])
}

impl Wide {
    pub fn from_i128(value: i128) -> Wide {
        let magnitude = value.unsigned_abs();
        Wide {
            negative: value < 0,
            magnitude: [magnitude as u64, (magnitude >> 64) as u64, 0, 0],
        }
    }

    fn signed(negative: bool, magnitude: [u64; 4]) -> Wide {
        Wide {
            negative: negative && magnitude != [0; 4],
            magnitude,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.magnitude == [0; 4]
    }

    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// The value as an i128, where it fits.
    pub fn to_i128(&self) -> Option<i128> {
        if self.magnitude[2] != 0 || self.magnitude[3] != 0 {
            return None;
        }
        let magnitude = self.magnitude[0] as u128 | (self.magnitude[1] as u128) << 64;
        if self.negative {
            (magnitude <= i128::MAX as u128 + 1).then(|| (magnitude as i128).wrapping_neg())
        } else {
            i128::try_from(magnitude).ok()
        }
    }

    pub fn to_big(&self) -> Big {
        Big::from_parts(self.negative, &self.magnitude)
    }

    /// A value of any size as a Wide, where it fits.
    pub fn from_big(value: &Big) -> Option<Wide> {
        let (negative, words) = value.to_words();
        if words.len() > 4 {
            return None;
        }
        let mut magnitude = [0u64; 4];
        magnitude[..words.len()].copy_from_slice(&words);
        Some(Wide::signed(negative, magnitude))
    }

    pub fn negated(&self) -> Wide {
        Wide::signed(!self.negative, self.magnitude)
    }

    pub fn checked_add(&self, other: &Wide) -> Option<Wide> {
        if self.negative == other.negative {
            return add(&self.magnitude, &other.magnitude)
                .map(|magnitude| Wide::signed(self.negative, magnitude));
        }
        Some(match compare(&self.magnitude, &other.magnitude) {
            Ordering::Equal => Wide::default(),
            Ordering::Greater => {
                Wide::signed(self.negative, subtract(&self.magnitude, &other.magnitude))
            }
            Ordering::Less => {
                Wide::signed(other.negative, subtract(&other.magnitude, &self.magnitude))
            }
        })
    }

    pub fn checked_mul(&self, other: &Wide) -> Option<Wide> {
        multiply(&self.magnitude, &other.magnitude)
            .map(|magnitude| Wide::signed(self.negative != other.negative, magnitude))
    }

    pub fn signum(&self) -> i32 {
        if self.is_zero() {
            0
        } else if self.negative {
            -1
        } else {
            1
        }
    }
}

impl PartialOrd for Wide {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Wide {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => compare(&self.magnitude, &other.magnitude),
            (true, true) => compare(&other.magnitude, &self.magnitude),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_agrees_with_the_unbounded_form_until_it_does_not_fit() {
        let ten_38 = Wide::from_i128(10i128.pow(38));
        let square = ten_38.checked_mul(&ten_38).expect("10^76 fits in 256 bits");
        assert_eq!(square.to_big(), Big::pow10(76));
        // 10^77 fits (2^256 is about 1.16 * 10^77); 10^78 does not.
        let ten = Wide::from_i128(10);
        let seventy_seven = square.checked_mul(&ten).expect("10^77 fits");
        assert_eq!(seventy_seven.to_big(), Big::pow10(77));
        assert!(seventy_seven.checked_mul(&ten).is_none());
        let sum = square.checked_add(&square.negated()).unwrap();
        assert!(sum.is_zero() && !sum.is_negative());
        assert_eq!(
            Wide::from_big(&Big::pow10(70)).unwrap().to_big(),
            Big::pow10(70)
        );
        assert!(Wide::from_big(&Big::pow10(80)).is_none());
        assert_eq!(Wide::from_i128(-5).to_i128(), Some(-5));
        assert_eq!(Wide::from_i128(i128::MIN).to_i128(), Some(i128::MIN));
        assert!(Wide::from_i128(-3) < Wide::from_i128(2));
    }
}
