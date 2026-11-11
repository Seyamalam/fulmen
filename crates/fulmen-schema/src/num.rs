//! Numeric internals: a small arbitrary-precision decimal representation
//! plus the comparison and divisibility primitives `Range`, `MultipleOf`
//! and `Decimal` validation need.
//!
//! `Decimal` cannot be a dependency (fulmen-schema is dependency-frozen —
//! only `regex` is approved), so decimals are kept as parsed literals:
//! sign, digit mantissa and base-10 exponent — `value = ±digits × 10^exp`.
//! Comparison is sign → magnitude → lexicographic, all on the digit text,
//! and `multiple_of` reduces to integer divisibility, computed by a
//! digit-walk modulo — so neither needs bignum arithmetic.
//!
//! [`LitNum`] unifies the three numeric input shapes: machine integers
//! (`i128` covers both `i64` and `u64` inputs), IEEE-754 doubles and
//! decimal literals (which also carry `BigInt` inputs that exceed `i128`).

use std::cmp::Ordering;

use crate::value::Value;

/// An integer parsed as `i128`, or `None` when the text is not
/// `[+-]?[0-9]+` or overflows `i128`.
pub(crate) fn int_from_text(text: &str) -> Option<i128> {
    let t = text.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: i128 = 0;
    for d in digits.bytes() {
        value = value.checked_mul(10)?.checked_add(i128::from(d - b'0'))?;
    }
    Some(if neg { -value } else { value })
}

/// A normalized decimal literal: `value = (neg ? -1 : 1) × digits × 10^exp`,
/// where `digits` holds only ASCII digits with no leading zeros. The empty
/// `digits` (`""`) is exactly zero and then `neg` is always `false`.
///
/// `0.5` is `{ neg: false, digits: "5", exp: -1 }`; `1200` is
/// `{ digits: "12", exp: 2 }` after normalization strips trailing zeros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecLit {
    /// Whether the value is negative (never `true` for zero).
    pub(crate) neg: bool,
    /// The mantissa digits, normalized: no leading or trailing zeros.
    pub(crate) digits: Box<str>,
    /// Base-10 exponent applied to the mantissa.
    pub(crate) exp: i64,
}

/// Maximum mantissa digits a `MultipleOf` bound may have. The divisibility
/// algorithm needs the bound as `u128`, which holds 38 nines.
pub(crate) const MAX_MOD_MANTISSA_DIGITS: usize = 38;

impl DecLit {
    /// Parse `[+-]? (D+ [. D*] | . D+) ([eE] [+-]? D+)?` — the Python
    /// `Decimal` literal grammar, plus surrounding ASCII whitespace, which
    /// `Decimal(str)` also tolerates. `D` is ASCII digits.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let t = text.trim();
        let (neg, rest) = match t.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        let (mantissa, exp_text) = match rest.find(['e', 'E']) {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let exp: i64 = if exp_text.is_empty() {
            0
        } else {
            let (eneg, ed) = match exp_text.strip_prefix('-') {
                Some(d) => (true, d),
                None => (false, exp_text.strip_prefix('+').unwrap_or(exp_text)),
            };
            if ed.is_empty() || !ed.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut e: i64 = 0;
            for d in ed.bytes() {
                e = e.checked_mul(10)?.checked_add(i64::from(d - b'0'))?;
            }
            if eneg { -e } else { e }
        };
        let (int_part, frac_part) = match mantissa.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mantissa, ""),
        };
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        let mut digits: String = int_part.to_owned();
        digits.push_str(frac_part);
        let mut exp = exp - i64::try_from(frac_part.len()).ok()?;
        // Normalize: leading zeros carry no value.
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            return Some(Self {
                neg: false,
                digits: "".into(),
                exp: 0,
            });
        }
        // Trailing zeros fold into the exponent ("1200" → "12"·10²).
        let trimmed = digits.trim_end_matches('0');
        exp += i64::try_from(digits.len() - trimmed.len()).ok()?;
        Some(Self {
            neg,
            digits: trimmed.into(),
            exp,
        })
    }

    /// Whether this literal is exactly zero.
    pub(crate) fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }

    /// The closest `f64`. Overflow yields ±∞, which callers accept as a
    /// valid float (a `Decimal` too large for `f64` is still a number).
    pub(crate) fn to_f64(&self) -> f64 {
        // Reconstruct `0.<digits> e <exp+len>` so `str::parse` does the
        // correctly-rounded conversion for any mantissa length.
        let mut text = String::with_capacity(self.digits.len() + 24);
        if self.neg {
            text.push('-');
        }
        text.push_str("0.");
        text.push_str(&self.digits);
        text.push('e');
        let shift = i64::try_from(self.digits.len()).unwrap_or(i64::MAX);
        text.push_str(&self.exp.saturating_add(shift).to_string());
        text.parse().unwrap_or(if self.neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        })
    }

    /// Canonical decimal text in the style of `str(Decimal(x))`: fixed
    /// notation when the adjusted exponent is `≥ -6` and `exp ≤ 0`
    /// (`0.001`, `12.3`, `5`), scientific `D[.DDD]E±a` otherwise
    /// (`1.2E+3`, `1E-7`). Output length is bounded by the mantissa — a
    /// `1e999999999` input stays ~12 bytes instead of a gigabyte of zeros.
    /// `Decimal(text)` reads this back to an exactly equal value.
    pub(crate) fn to_canonical_string(&self) -> String {
        if self.is_zero() {
            return "0".to_owned();
        }
        // usize→i64 saturates: a mantissa can't exceed memory.
        let len = i64::try_from(self.digits.len()).unwrap_or(i64::MAX);
        let adjusted = self.exp + len - 1; // exponent of the leading digit
        let mut out = String::with_capacity(self.digits.len() + 24);
        if self.neg {
            out.push('-');
        }
        if self.exp <= 0 && adjusted >= -6 {
            // Fixed notation; `shift` = |exp| is bounded by the
            // `adjusted >= -6` guard to roughly `digits.len() + 6`.
            let shift = usize::try_from(self.exp.unsigned_abs()).unwrap_or(usize::MAX);
            if shift == 0 {
                out.push_str(&self.digits); // integral — no point
            } else if shift < self.digits.len() {
                let p = self.digits.len() - shift;
                out.push_str(&self.digits[..p]);
                out.push('.');
                out.push_str(&self.digits[p..]);
            } else {
                out.push_str("0.");
                for _ in 0..shift.saturating_sub(self.digits.len()) {
                    out.push('0');
                }
                out.push_str(&self.digits);
            }
        } else {
            let mut chars = self.digits.chars();
            if let Some(first) = chars.next() {
                out.push(first);
            }
            let rest = chars.as_str();
            if !rest.is_empty() {
                out.push('.');
                out.push_str(rest);
            }
            out.push('E');
            if adjusted >= 0 {
                out.push('+');
            }
            out.push_str(&adjusted.to_string());
        }
        out
    }

    /// The mantissa as `u128`, when it has at most 38 digits (always true
    /// after a `MAX_MOD_MANTISSA_DIGITS` compile check).
    fn mantissa_u128(&self) -> Option<u128> {
        if self.digits.len() > MAX_MOD_MANTISSA_DIGITS {
            return None;
        }
        let mut m: u128 = 0;
        for d in self.digits.bytes() {
            m = m.checked_mul(10)?.checked_add(u128::from(d - b'0'))?;
        }
        Some(m)
    }
}

/// Compare two decimal literals exactly: sign, then magnitude
/// (`digits.len() + exp`, the position of the most significant digit),
/// then the digit text padded right to equal length.
pub(crate) fn cmp_dec(a: &DecLit, b: &DecLit) -> Ordering {
    if a.neg != b.neg {
        return if a.neg {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let ord = if a.is_zero() || b.is_zero() {
        // A zero mantissa is empty; any nonzero mantissa is longer.
        a.digits.len().cmp(&b.digits.len())
    } else {
        let mag_a = a.exp + i64::try_from(a.digits.len()).unwrap_or(i64::MAX);
        let mag_b = b.exp + i64::try_from(b.digits.len()).unwrap_or(i64::MAX);
        mag_a.cmp(&mag_b).then_with(|| {
            let len = a.digits.len().max(b.digits.len());
            let da = a.digits.bytes().chain(std::iter::repeat(b'0'));
            let db = b.digits.bytes().chain(std::iter::repeat(b'0'));
            da.zip(db)
                .take(len)
                .find_map(|(x, y)| match x.cmp(&y) {
                    Ordering::Equal => None,
                    ord => Some(ord),
                })
                .unwrap_or(Ordering::Equal)
        })
    };
    if a.neg { ord.reverse() } else { ord }
}

/// A numeric value under validation — machine integer, float or decimal.
#[derive(Debug, Clone)]
pub(crate) enum LitNum {
    /// Any integer that fits `i128` (covers `i64` and `u64` inputs).
    Int(i128),
    /// IEEE-754 double.
    Float(f64),
    /// A decimal literal: `Decimal` inputs, `BigInt` integers beyond
    /// `i128`, and decimal bounds.
    Dec(DecLit),
}

impl LitNum {
    /// The numeric view of a [`Value`], when it is a number variant.
    /// `BigInt` becomes `Int` when it fits `i128`, `Dec` otherwise — so
    /// range checks stay exact beyond machine integers.
    pub(crate) fn of_value(v: &Value<'_>) -> Option<Self> {
        match v {
            Value::Int(i) => Some(Self::Int(i128::from(*i))),
            Value::UInt(u) => Some(Self::Int(i128::from(*u))),
            Value::BigInt(t) => Some(
                int_from_text(t)
                    .map(Self::Int)
                    .or_else(|| DecLit::parse(t).map(Self::Dec))?,
            ),
            Value::Float(f) => Some(Self::Float(*f)),
            _ => None,
        }
    }

    /// Exact three-way compare. Mixed `Float` pairs degrade to `f64`
    /// comparison (documented: comparing huge decimals through `f64` loses
    /// precision beyond the 53-bit mantissa); `Int`/`Dec` pairs stay exact.
    /// `NaN` compares equal to everything — callers reject it first when
    /// the distinction matters.
    pub(crate) fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => a.cmp(b),
            (Self::Float(a), Self::Float(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Self::Dec(a), Self::Dec(b)) => cmp_dec(a, b),
            (Self::Int(a), Self::Dec(b)) => cmp_dec(&int_to_dec(*a), b),
            (Self::Dec(a), Self::Int(b)) => cmp_dec(a, &int_to_dec(*b)),
            (Self::Float(a), b) => a.partial_cmp(&b.to_f64()).unwrap_or(Ordering::Equal),
            (a, Self::Float(b)) => a.to_f64().partial_cmp(b).unwrap_or(Ordering::Equal),
        }
    }

    /// The closest `f64`.
    #[expect(
        clippy::cast_precision_loss,
        reason = "approximation is the documented contract of this method"
    )]
    pub(crate) fn to_f64(&self) -> f64 {
        match self {
            Self::Int(i) => *i as f64,
            Self::Float(f) => *f,
            Self::Dec(d) => d.to_f64(),
        }
    }

    /// The bound rendered as wire text, for error messages.
    pub(crate) fn wire_text(&self) -> String {
        match self {
            Self::Int(i) => i.to_string(),
            Self::Float(f) => format!("{f:?}"),
            Self::Dec(d) => d.to_canonical_string(),
        }
    }

    /// The bound as an error `limit` literal (§11.7 shows `"limit": 1`).
    /// Decimals travel as their canonical text — the §11.7 `limit` field
    /// is a string for bounds JSON can't express exactly.
    pub(crate) fn wire_literal(&self) -> crate::value::LiteralValue {
        use crate::value::LiteralValue;
        match self {
            Self::Int(i) => {
                if let Ok(v) = i64::try_from(*i) {
                    LiteralValue::Int(v)
                } else if let Ok(v) = u64::try_from(*i) {
                    LiteralValue::UInt(v)
                } else {
                    LiteralValue::BigInt(i.to_string().into())
                }
            }
            Self::Float(f) => LiteralValue::Float(*f),
            Self::Dec(d) => LiteralValue::Str(d.to_canonical_string().into()),
        }
    }

    /// Whether `self % bound == 0`.
    ///
    /// - `Float` anywhere → `f64` remainder (`0.3 % 0.1` is *not* `0` in
    ///   IEEE-754 — a documented precision limit of float `MultipleOf`).
    /// - `Int % Int` → `i128` remainder.
    /// - `Dec`/`Int` pairs → exact integer divisibility after scaling both
    ///   sides to integers; see [`dec_multiple_of`]. A zero bound divides
    ///   nothing (the compiler also rejects it).
    pub(crate) fn is_multiple_of(&self, bound: &Self) -> bool {
        match (self, bound) {
            (Self::Float(a), b) => {
                let m = b.to_f64();
                m != 0.0 && a % m == 0.0
            }
            (a, Self::Float(b)) => *b != 0.0 && a.to_f64() % b == 0.0,
            (Self::Int(a), Self::Int(b)) => *b != 0 && a % b == 0,
            (a, Self::Dec(b)) => dec_multiple_of(&to_scaled_int(a), b),
            (Self::Dec(_), Self::Int(b)) => {
                *b != 0 && dec_multiple_of(&to_scaled_int(self), &int_to_dec(*b))
            }
        }
    }
}

/// An integer expressed as decimal mantissa × 10^exp, for the
/// divisibility math in [`dec_multiple_of`]. The mantissa is unsigned:
/// divisibility ignores sign.
struct ScaledInt {
    /// The mantissa digits (no sign, possibly empty for zero).
    digits: String,
    /// The base-10 exponent.
    exp: i64,
}

fn to_scaled_int(n: &LitNum) -> ScaledInt {
    match n {
        LitNum::Int(i) => ScaledInt {
            digits: i.unsigned_abs().to_string(),
            exp: 0,
        },
        LitNum::Dec(d) => ScaledInt {
            digits: d.digits.to_string(),
            exp: d.exp,
        },
        // Floats exit before reaching here (`is_multiple_of` arms above).
        LitNum::Float(_) => ScaledInt {
            digits: String::new(),
            exp: 0,
        },
    }
}

/// Whether `a` (mantissa × 10^exp) is an integer multiple of `b`.
///
/// Strip the dividend's `t` trailing zeros first: `A = A′·10^t`, so with
/// `k = a.exp − b.exp + t` the quotient is `A′·10^k / B`. When `k < 0` the
/// bound effectively needs `10^{-k}` more zeros than `A` had — `A′` ends
/// in a non-zero digit so the quotient cannot be an integer. With `k ≥ 0`
/// split `B = 2^a₂ · 5^a₅ · B′` (`B′` coprime to 10): `A′·10^k` is
/// divisible by `B` iff
///
/// - `B′` divides `A′` (the `10^k` factor only supplies 2s and 5s), and
/// - `v₂(A′) + k ≥ a₂` and `v₅(A′) + k ≥ a₅`, where `v_p(x)` counts the
///   factors of `p` in `x` — checked as `A′ ≡ 0 mod p^need`, all on the
///   digit text, so `A′` may be arbitrarily long. `need ≤ 127`/`≤ 54`
///   always holds because `k ≥ 0` and `B` fit `u128`.
///
/// `factor` must fit `u128` (≤38 mantissa digits); a larger bound makes
/// the check return `false`, but the compiler rejects such bounds anyway.
fn dec_multiple_of(value: &ScaledInt, factor: &DecLit) -> bool {
    // Split `A = A′·10^t`.
    let zeros = value
        .digits
        .bytes()
        .rev()
        .take_while(|&d| d == b'0')
        .count();
    let a_clean = &value.digits[..value.digits.len() - zeros];
    // A zero dividend (empty mantissa, or one that was only zeros) is a
    // multiple of every non-zero bound.
    if a_clean.is_empty() || a_clean.bytes().all(|d| d == b'0') {
        return true;
    }
    let Some(mut b) = factor.mantissa_u128() else {
        return false;
    };
    if b == 0 {
        return false;
    }
    let t = i64::try_from(zeros).unwrap_or(i64::MAX);
    let k = value.exp - factor.exp + t;
    if k < 0 {
        // `B·10^{-k} | A′` — impossible: `A′` ends in a non-zero digit.
        return false;
    }
    let mut a2: i64 = 0;
    while b % 2 == 0 {
        b /= 2;
        a2 += 1;
    }
    let mut a5: i64 = 0;
    while b % 5 == 0 {
        b /= 5;
        a5 += 1;
    }
    // `b` is now B′, the part of B coprime to 10.
    if b > 1 && digit_mod(a_clean, b) != 0 {
        return false;
    }
    // v2(A′) ≥ need2 iff A′ ≡ 0 mod 2^need2, and likewise for 5.
    // `need2 ≤ a2 ≤ 127` and `need5 ≤ a5 ≤ 54` because `k ≥ 0` and `B` fit
    // `u128`, so both moduli fit `u128` whenever the bound did.
    let need2 = a2 - k;
    let need5 = a5 - k;
    let v2_ok = need2 <= 0 || (need2 < 127 && digit_mod(a_clean, 1u128 << need2) == 0);
    let v5_ok = need5 <= 0 || pow5(need5).is_some_and(|p| digit_mod(a_clean, p) == 0);
    v2_ok && v5_ok
}

/// `5^n` in `u128`, or `None` when `n < 0` or it overflows
/// (`5^55 > u128::MAX` — a bound needing that many 5-factors can never be
/// satisfied, so `None` correctly fails the check).
fn pow5(n: i64) -> Option<u128> {
    let mut p: u128 = 1;
    for _ in 0..n {
        p = p.checked_mul(5)?;
    }
    Some(p)
}

/// `digits mod m` for a digit string of arbitrary length, one digit at a
/// time. Overflow-safe for any `m ≤ u128::MAX` via repeated [`addmod`].
fn digit_mod(digits: &str, m: u128) -> u128 {
    debug_assert!(m > 0);
    let mut rem: u128 = 0;
    for d in digits.bytes() {
        // rem·10 + d mod m — never multiply; fold rem into itself 9 times.
        let mut r = rem;
        for _ in 0..9 {
            r = addmod(r, rem, m);
        }
        rem = addmod(r, u128::from(d - b'0'), m);
    }
    rem
}

/// `(a + b) mod m` without overflow, for `a, b < m`.
fn addmod(a: u128, b: u128, m: u128) -> u128 {
    let b = b % m;
    if a >= m - b { a - (m - b) } else { a + b }
}

/// `i128` → [`DecLit`] (`1200` → `"12"·10²`).
fn int_to_dec(i: i128) -> DecLit {
    let neg = i < 0;
    let digits = i.unsigned_abs().to_string();
    let trimmed = digits.trim_end_matches('0');
    DecLit {
        neg: neg && !trimmed.is_empty(),
        digits: trimmed.into(),
        exp: i64::try_from(digits.len() - trimmed.len()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_text() {
        assert_eq!(int_from_text("42"), Some(42));
        assert_eq!(int_from_text("-42"), Some(-42));
        assert_eq!(int_from_text("+42"), Some(42));
        assert_eq!(int_from_text(" 42 "), Some(42));
        assert_eq!(int_from_text("007"), Some(7));
        assert_eq!(
            int_from_text("170141183460469231731687303715884105728"),
            None
        );
        assert_eq!(int_from_text(""), None);
        assert_eq!(int_from_text("4.2"), None);
        assert_eq!(int_from_text("--4"), None);
    }

    #[test]
    fn dec_parse() {
        let d = DecLit::parse("1.5").unwrap();
        assert_eq!((d.neg, &*d.digits, d.exp), (false, "15", -1));
        let d = DecLit::parse("1200").unwrap();
        assert_eq!((d.neg, &*d.digits, d.exp), (false, "12", 2));
        let d = DecLit::parse("0.001").unwrap();
        assert_eq!((d.neg, &*d.digits, d.exp), (false, "1", -3));
        let d = DecLit::parse("-1.5e3").unwrap();
        assert_eq!((d.neg, &*d.digits, d.exp), (true, "15", 2));
        let d = DecLit::parse(".5").unwrap();
        assert_eq!((d.neg, &*d.digits, d.exp), (false, "5", -1));
        let d = DecLit::parse("0.0").unwrap();
        assert!(d.is_zero() && !d.neg);
        let d = DecLit::parse("-0").unwrap();
        assert!(d.is_zero() && !d.neg);
        let d = DecLit::parse(" 2.5 ").unwrap();
        assert_eq!((&*d.digits, d.exp), ("25", -1));
        assert!(DecLit::parse("").is_none());
        assert!(DecLit::parse(".").is_none());
        assert!(DecLit::parse("e5").is_none());
        assert!(DecLit::parse("1.2.3").is_none());
        assert!(DecLit::parse("1e99999999999999999999").is_none());
    }

    #[test]
    fn dec_canonical_string() {
        let p = |s: &str| DecLit::parse(s).unwrap().to_canonical_string();
        assert_eq!(p("1.5"), "1.5");
        assert_eq!(p("1E+3"), "1E+3");
        assert_eq!(p("0.001"), "0.001");
        assert_eq!(p("-0.5"), "-0.5");
        assert_eq!(p("0.0"), "0");
        assert_eq!(p("-0"), "0");
        assert_eq!(p("12.30"), "12.3");
        assert_eq!(p("+5"), "5");
        assert_eq!(p("1e-5"), "0.00001");
        assert_eq!(p("1e-7"), "1E-7");
        assert_eq!(p(".25"), "0.25");
        // A giant exponent stays small — no gigabyte of zeros.
        assert_eq!(p("1e999999999"), "1E+999999999");
    }

    #[test]
    fn dec_compare() {
        let ord =
            |a: &str, b: &str| cmp_dec(&DecLit::parse(a).unwrap(), &DecLit::parse(b).unwrap());
        assert_eq!(ord("1.5", "1.5"), Ordering::Equal);
        assert_eq!(ord("1.50", "1.5"), Ordering::Equal);
        assert_eq!(ord("12", "12.3"), Ordering::Less);
        assert_eq!(ord("0.5", "0.06"), Ordering::Greater);
        assert_eq!(ord("-1", "0"), Ordering::Less);
        assert_eq!(ord("-2", "-1"), Ordering::Less);
        assert_eq!(ord("9e2", "899"), Ordering::Greater); // 900 > 899
        assert_eq!(ord("1e-3", "0.001"), Ordering::Equal);
        assert_eq!(
            ord("99999999999999999999999999999999999999", "0.00001"),
            Ordering::Greater
        );
    }

    #[test]
    fn multiple_of() {
        let mo = |a: LitNum, b: LitNum| a.is_multiple_of(&b);
        assert!(mo(LitNum::Int(10), LitNum::Int(5)));
        assert!(!mo(LitNum::Int(7), LitNum::Int(5)));
        assert!(!mo(LitNum::Int(1), LitNum::Int(0)));

        let dec = |s: &str| LitNum::Dec(DecLit::parse(s).unwrap());
        assert!(mo(dec("1.5"), dec("0.5")));
        assert!(mo(dec("1.50"), dec("0.25")));
        assert!(!mo(dec("1.5"), dec("0.4")));
        assert!(mo(dec("10"), dec("2.5")));
        assert!(!mo(dec("10"), dec("3")));
        assert!(mo(dec("0"), dec("3")));
        assert!(mo(dec("7e2"), dec("7"))); // 700 % 7
        assert!(mo(dec("7e2"), dec("0.07"))); // 700 / 0.07 = 10000
        assert!(!mo(dec("7e2"), dec("0.06")));
        // Int input vs decimal bound and decimal input vs int bound.
        assert!(mo(LitNum::Int(10), dec("2.5")));
        assert!(mo(LitNum::Int(10), dec("0.4"))); // 10 / 0.4 = 25
        assert!(!mo(LitNum::Int(7), dec("0.4"))); // 7 / 0.4 = 17.5
        assert!(!mo(dec("10.5"), LitNum::Int(3))); // 10.5 / 3 = 3.5
        assert!(!mo(dec("10.5"), LitNum::Int(4)));
        assert!(mo(dec("15.0"), LitNum::Int(5))); // decimal that is integral
        // Huge dividend (39 digits, beyond u128) via digit-walk.
        assert!(mo(
            dec("123456789012345678901234567890123456789"),
            dec("0.000000001")
        ));
    }

    #[test]
    fn digit_mod_big() {
        assert_eq!(digit_mod("1234", 7), 1234 % 7);
        // 1 followed by 150 zeros: 10^150 mod 8 = 0 (any exponent ≥ 3).
        let big = format!("1{}", "0".repeat(150));
        assert_eq!(digit_mod(&big, 8), 0);
        // 7^0-free check on a huge modulus path (m > u128::MAX/10):
        assert_eq!(digit_mod("5", u128::MAX), 5);
        assert_eq!(digit_mod("1", 1), 0);
    }
}
