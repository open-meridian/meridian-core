//! A signed integer of any size: the exact fallback past 256 bits, and the
//! parts of a rational (historical VaR and ES divide, ENG Q11).
//!
//! Our own, as the engine is (the first release, "Our own engine in Rust"):
//! sign and magnitude, the magnitude in 32-bit limbs, least significant
//! first, with no high zero limb, so zero is the empty magnitude and is
//! never negative. Long division is Knuth's algorithm D (TAOCP vol. 2,
//! 4.3.1), as Hacker's Delight writes it.

use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Big {
    negative: bool,
    magnitude: Vec<u32>,
}

const BASE: u64 = 1 << 32;

fn trim(magnitude: &mut Vec<u32>) {
    while magnitude.last() == Some(&0) {
        magnitude.pop();
    }
}

fn magnitude_of(mut value: u128) -> Vec<u32> {
    let mut out = Vec::new();
    while value != 0 {
        out.push(value as u32);
        value >>= 32;
    }
    out
}

fn compare_magnitudes(a: &[u32], b: &[u32]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for (x, y) in a.iter().rev().zip(b.iter().rev()) {
        if x != y {
            return x.cmp(y);
        }
    }
    Ordering::Equal
}

fn add_magnitudes(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for (i, &x) in long.iter().enumerate() {
        let sum = x as u64 + short.get(i).copied().unwrap_or(0) as u64 + carry;
        out.push(sum as u32);
        carry = sum >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b`, where `a >= b`.
fn subtract_magnitudes(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let mut difference = x as i64 - borrow - b.get(i).copied().unwrap_or(0) as i64;
        if difference < 0 {
            difference += BASE as i64;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(difference as u32);
    }
    trim(&mut out);
    out
}

fn multiply_magnitudes(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let product = x as u64 * y as u64 + out[i + j] as u64 + carry;
            out[i + j] = product as u32;
            carry = product >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    trim(&mut out);
    out
}

/// The quotient and remainder of `a` by one limb.
fn divide_by_limb(a: &[u32], divisor: u32) -> (Vec<u32>, u32) {
    let mut quotient = vec![0u32; a.len()];
    let mut remainder = 0u64;
    for i in (0..a.len()).rev() {
        let current = (remainder << 32) | a[i] as u64;
        quotient[i] = (current / divisor as u64) as u32;
        remainder = current % divisor as u64;
    }
    trim(&mut quotient);
    (quotient, remainder as u32)
}

fn shift_left(a: &[u32], bits: u32, extra: usize) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + extra);
    if bits == 0 {
        out.extend_from_slice(a);
        out.resize(a.len() + extra, 0);
        return out;
    }
    let mut carry = 0u32;
    for &x in a {
        out.push((x << bits) | carry);
        carry = x >> (32 - bits);
    }
    out.push(carry);
    out.resize(a.len() + extra.max(1), 0);
    out
}

/// Knuth's algorithm D: the quotient and remainder of `u` by `v`, `v` of two
/// limbs or more and `u >= v`.
fn divide_magnitudes(u: &[u32], v: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let n = v.len();
    let m = u.len() - n;
    let shift = v[n - 1].leading_zeros();
    let vn = shift_left(v, shift, 0);
    let vn = &vn[..n];
    let mut un = shift_left(u, shift, 1);
    un.truncate(u.len() + 1);
    let mut quotient = vec![0u32; m + 1];
    for j in (0..=m).rev() {
        let numerator = ((un[j + n] as u64) << 32) | un[j + n - 1] as u64;
        let mut qhat = numerator / vn[n - 1] as u64;
        let mut rhat = numerator % vn[n - 1] as u64;
        while qhat >= BASE || qhat * vn[n - 2] as u64 > ((rhat << 32) | un[j + n - 2] as u64) {
            qhat -= 1;
            rhat += vn[n - 1] as u64;
            if rhat >= BASE {
                break;
            }
        }
        let mut k: i64 = 0;
        for i in 0..n {
            let product = qhat * vn[i] as u64;
            let t = un[i + j] as i64 - k - (product & 0xFFFF_FFFF) as i64;
            un[i + j] = t as u32;
            k = (product >> 32) as i64 - (t >> 32);
        }
        let t = un[j + n] as i64 - k;
        un[j + n] = t as u32;
        if t < 0 {
            qhat -= 1;
            let mut carry = 0u64;
            for i in 0..n {
                let sum = un[i + j] as u64 + vn[i] as u64 + carry;
                un[i + j] = sum as u32;
                carry = sum >> 32;
            }
            un[j + n] = un[j + n].wrapping_add(carry as u32);
        }
        quotient[j] = qhat as u32;
    }
    trim(&mut quotient);
    let mut remainder = vec![0u32; n];
    if shift == 0 {
        remainder.copy_from_slice(&un[..n]);
    } else {
        for i in 0..n {
            remainder[i] = (un[i] >> shift) | (un[i + 1] << (32 - shift));
        }
    }
    trim(&mut remainder);
    (quotient, remainder)
}

impl Big {
    pub fn zero() -> Big {
        Big::default()
    }

    pub fn from_i128(value: i128) -> Big {
        Big {
            negative: value < 0,
            magnitude: magnitude_of(value.unsigned_abs()),
        }
    }

    pub fn from_u128(value: u128) -> Big {
        Big {
            negative: false,
            magnitude: magnitude_of(value),
        }
    }

    fn signed(negative: bool, mut magnitude: Vec<u32>) -> Big {
        trim(&mut magnitude);
        Big {
            negative: negative && !magnitude.is_empty(),
            magnitude,
        }
    }

    /// A value from its sign and its magnitude's little-endian 64-bit
    /// words: for the 256-bit form's conversion.
    pub(crate) fn from_parts(negative: bool, words: &[u64]) -> Big {
        let mut magnitude = Vec::with_capacity(words.len() * 2);
        for &word in words {
            magnitude.push(word as u32);
            magnitude.push((word >> 32) as u32);
        }
        Big::signed(negative, magnitude)
    }

    /// The magnitude as little-endian 64-bit words, and the sign.
    pub(crate) fn to_words(&self) -> (bool, Vec<u64>) {
        let mut words = Vec::with_capacity(self.magnitude.len().div_ceil(2));
        for pair in self.magnitude.chunks(2) {
            let low = pair[0] as u64;
            let high = pair.get(1).copied().unwrap_or(0) as u64;
            words.push(low | (high << 32));
        }
        (self.negative, words)
    }

    pub fn is_zero(&self) -> bool {
        self.magnitude.is_empty()
    }

    pub fn is_negative(&self) -> bool {
        self.negative
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

    /// Whether the value is odd.
    pub fn is_odd(&self) -> bool {
        self.magnitude.first().is_some_and(|limb| limb & 1 == 1)
    }

    pub fn to_i128(&self) -> Option<i128> {
        if self.magnitude.len() > 4 {
            return None;
        }
        let mut magnitude = 0u128;
        for (i, &limb) in self.magnitude.iter().enumerate() {
            magnitude |= (limb as u128) << (32 * i);
        }
        if self.negative {
            if magnitude <= i128::MAX as u128 + 1 {
                Some((magnitude as i128).wrapping_neg())
            } else {
                None
            }
        } else {
            i128::try_from(magnitude).ok()
        }
    }

    pub fn negated(&self) -> Big {
        Big::signed(!self.negative, self.magnitude.clone())
    }

    pub fn abs(&self) -> Big {
        Big::signed(false, self.magnitude.clone())
    }

    pub fn add(&self, other: &Big) -> Big {
        if self.negative == other.negative {
            return Big::signed(
                self.negative,
                add_magnitudes(&self.magnitude, &other.magnitude),
            );
        }
        match compare_magnitudes(&self.magnitude, &other.magnitude) {
            Ordering::Equal => Big::zero(),
            Ordering::Greater => Big::signed(
                self.negative,
                subtract_magnitudes(&self.magnitude, &other.magnitude),
            ),
            Ordering::Less => Big::signed(
                other.negative,
                subtract_magnitudes(&other.magnitude, &self.magnitude),
            ),
        }
    }

    pub fn sub(&self, other: &Big) -> Big {
        self.add(&other.negated())
    }

    pub fn mul(&self, other: &Big) -> Big {
        Big::signed(
            self.negative != other.negative,
            multiply_magnitudes(&self.magnitude, &other.magnitude),
        )
    }

    pub fn mul_small(&self, factor: u32) -> Big {
        self.mul(&Big::from_u128(factor as u128))
    }

    /// 10^n.
    pub fn pow10(n: u32) -> Big {
        let mut out = Big::from_u128(1);
        let mut left = n;
        while left > 0 {
            let step = left.min(38);
            out = out.mul(&Big::from_u128(10u128.pow(step)));
            left -= step;
        }
        out
    }

    /// The quotient and remainder truncated toward zero, the remainder taking
    /// the dividend's sign. `divisor` is not zero.
    fn divide_truncated(&self, divisor: &Big) -> (Big, Big) {
        assert!(!divisor.is_zero(), "division by zero");
        let (quotient, remainder) =
            if compare_magnitudes(&self.magnitude, &divisor.magnitude) == Ordering::Less {
                (Vec::new(), self.magnitude.clone())
            } else if divisor.magnitude.len() == 1 {
                let (quotient, remainder) = divide_by_limb(&self.magnitude, divisor.magnitude[0]);
                (
                    quotient,
                    if remainder == 0 {
                        Vec::new()
                    } else {
                        vec![remainder]
                    },
                )
            } else {
                divide_magnitudes(&self.magnitude, &divisor.magnitude)
            };
        (
            Big::signed(self.negative != divisor.negative, quotient),
            Big::signed(self.negative, remainder),
        )
    }

    /// Floor division by a positive divisor: the quotient rounded toward
    /// minus infinity, and a remainder in `0 .. divisor`, as Python's divmod.
    pub fn divmod_floor(&self, divisor: &Big) -> (Big, Big) {
        assert!(
            !divisor.negative && !divisor.is_zero(),
            "a floor division here is by a positive divisor"
        );
        let (quotient, remainder) = self.divide_truncated(divisor);
        if remainder.negative {
            (quotient.sub(&Big::from_u128(1)), remainder.add(divisor))
        } else {
            (quotient, remainder)
        }
    }

    /// The decimal digits of the magnitude, "0" for zero.
    pub fn magnitude_digits(&self) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        let mut chunks = Vec::new();
        let mut rest = self.magnitude.clone();
        while !rest.is_empty() {
            let (quotient, remainder) = divide_by_limb(&rest, 1_000_000_000);
            chunks.push(remainder);
            rest = quotient;
        }
        let mut out = chunks.pop().map(|c| c.to_string()).unwrap_or_default();
        while let Some(chunk) = chunks.pop() {
            out.push_str(&format!("{chunk:09}"));
        }
        out
    }

    /// Parse unsigned decimal digits.
    pub fn from_digits(digits: &str) -> Option<Big> {
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut out = Big::zero();
        for chunk in digits.as_bytes().chunks(18) {
            let text = std::str::from_utf8(chunk).ok()?;
            let value: u64 = text.parse().ok()?;
            out = out
                .mul(&Big::from_u128(10u128.pow(chunk.len() as u32)))
                .add(&Big::from_u128(value as u128));
        }
        Some(out)
    }
}

impl PartialOrd for Big {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Big {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => compare_magnitudes(&self.magnitude, &other.magnitude),
            (true, true) => compare_magnitudes(&other.magnitude, &self.magnitude),
        }
    }
}

impl std::fmt::Display for Big {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.negative {
            write!(f, "-")?;
        }
        write!(f, "{}", self.magnitude_digits())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic sequence, so a failure reproduces.
    struct Draws(u64);

    impl Draws {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn big(&mut self, limbs: usize) -> Big {
            let words: Vec<u64> = (0..limbs).map(|_| self.next()).collect();
            Big::from_parts(self.next() & 1 == 1, &words)
        }
    }

    #[test]
    fn small_values_agree_with_i128() {
        let values = [
            0i128,
            1,
            -1,
            7,
            -7,
            1 << 70,
            -(1 << 90),
            i128::MAX / 3,
            i128::MIN / 5,
        ];
        for &a in &values {
            for &b in &values {
                let (x, y) = (Big::from_i128(a), Big::from_i128(b));
                if let Some(sum) = a.checked_add(b) {
                    assert_eq!(x.add(&y).to_i128(), Some(sum), "{a} + {b}");
                }
                if let Some(product) = a.checked_mul(b) {
                    assert_eq!(x.mul(&y).to_i128(), Some(product), "{a} * {b}");
                }
                assert_eq!(x.cmp(&y), a.cmp(&b));
                if b > 0 {
                    let (q, r) = x.divmod_floor(&y);
                    assert_eq!(q.to_i128(), Some(a.div_euclid(b)), "{a} div {b}");
                    assert_eq!(r.to_i128(), Some(a.rem_euclid(b)), "{a} mod {b}");
                }
            }
        }
    }

    #[test]
    fn division_inverts_multiplication_at_any_size() {
        let mut draws = Draws(0x9E37_79B9_7F4A_7C15);
        for round in 0..400 {
            let a = draws.big(1 + round % 9);
            let mut b = draws.big(1 + (round * 7) % 6).abs();
            if b.is_zero() {
                b = Big::from_u128(3);
            }
            let (q, r) = a.divmod_floor(&b);
            assert!(r >= Big::zero() && r < b, "remainder out of range");
            assert_eq!(q.mul(&b).add(&r), a, "q*b + r is a");
        }
    }

    #[test]
    fn digits_read_and_write_back() {
        // 2^200, from Python.
        let two_200 = "1606938044258990275541962092341162602522202993782792835301376";
        let mut power = Big::from_u128(1);
        for _ in 0..200 {
            power = power.mul_small(2);
        }
        assert_eq!(power.to_string(), two_200);
        assert_eq!(Big::from_digits(two_200), Some(power.clone()));
        assert_eq!(power.negated().to_string(), format!("-{two_200}"));
        assert_eq!(Big::pow10(40).to_string(), format!("1{}", "0".repeat(40)));
        assert_eq!(Big::zero().to_string(), "0");
    }
}
