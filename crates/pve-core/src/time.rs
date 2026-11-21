//! Exact rational time. Everything in the core is measured in seconds as a fraction.

use num_rational::Ratio;
use num_traits::{One, Signed, ToPrimitive, Zero};

/// Seconds (or any exact quantity) as a reduced fraction.
pub type R = Ratio<i128>;

pub fn r(n: i128, d: i128) -> R {
    R::new(n, d)
}

pub fn int(n: i128) -> R {
    R::from_integer(n)
}

pub fn zero() -> R {
    R::zero()
}

pub fn to_f64(t: &R) -> f64 {
    // Exact for the magnitudes used here (numerator and denominator well under 2^53).
    t.numer().to_f64().unwrap_or(f64::NAN) / t.denom().to_f64().unwrap_or(f64::NAN)
}

/// "5", "-3", "7.5", "1e-3" or "1001/30000" (also "4:3" for ratios when `colon` is set).
pub fn parse(text: &str) -> Option<R> {
    let s = text.trim();
    if let Some((n, d)) = s.split_once('/') {
        let (n, d) = (parse_decimal(n.trim())?, parse_decimal(d.trim())?);
        return if d.is_zero() { None } else { Some(n / d) };
    }
    parse_decimal(s)
}

pub fn parse_ratio(text: &str) -> Option<R> {
    parse(&text.replace(':', "/"))
}

fn parse_decimal(s: &str) -> Option<R> {
    if s.is_empty() {
        return None;
    }
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], s[i + 1..].parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (neg, digits) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa.strip_prefix('+').unwrap_or(mantissa)),
    };
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if (whole.is_empty() && frac.is_empty()) || !whole.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n: i128 = 0;
    for b in whole.bytes().chain(frac.bytes()) {
        n = n.checked_mul(10)?.checked_add((b - b'0') as i128)?;
    }
    let scale = exp - frac.len() as i32;
    let ten = |p: u32| 10i128.checked_pow(p);
    let v = if scale >= 0 { R::from_integer(n.checked_mul(ten(scale as u32)?)?) } else { R::new(n, ten((-scale) as u32)?) };
    Some(if neg { -v } else { v })
}

/// The project-file form: "n" or "n/d".
pub fn to_string(t: &R) -> String {
    if t.denom().is_one() {
        t.numer().to_string()
    } else {
        format!("{}/{}", t.numer(), t.denom())
    }
}

/// Round to the nearest integer, halves to even.
pub fn round_half_even(t: &R) -> i128 {
    let floor = t.floor().to_integer();
    let frac = *t - R::from_integer(floor);
    let half = R::new(1, 2);
    if frac > half || (frac == half && floor % 2 != 0) {
        floor + 1
    } else {
        floor
    }
}

/// Nearest even integer (halves to even), at least `minimum`.
pub fn even(t: &R, minimum: i128) -> i128 {
    (2 * round_half_even(&(*t / int(2)))).max(minimum)
}

/// Seconds with microsecond precision, as ffmpeg options want them.
pub fn seconds(t: &R) -> String {
    format!("{:.6}", to_f64(t))
}

pub fn abs(t: &R) -> R {
    t.abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints() {
        assert_eq!(parse("7.5"), Some(r(15, 2)));
        assert_eq!(parse("1001/30000"), Some(r(1001, 30000)));
        assert_eq!(parse("-0.25"), Some(r(-1, 4)));
        assert_eq!(parse("1e-3"), Some(r(1, 1000)));
        assert_eq!(parse("5"), Some(int(5)));
        assert_eq!(parse_ratio("4:3"), Some(r(4, 3)));
        assert_eq!(parse("x"), None);
        assert_eq!(parse("1/0"), None);
        assert_eq!(to_string(&r(93, 4)), "93/4");
        assert_eq!(to_string(&int(20)), "20");
        assert_eq!(seconds(&r(59, 15)), "3.933333");
    }

    #[test]
    fn rounds_half_to_even() {
        assert_eq!(round_half_even(&r(5, 2)), 2);
        assert_eq!(round_half_even(&r(7, 2)), 4);
        assert_eq!(round_half_even(&r(-5, 2)), -2);
        assert_eq!(even(&r(1215, 2), 2), 608);
        assert_eq!(even(&int(455), 2), 456);
        assert_eq!(even(&int(0), 0), 0);
        assert_eq!(even(&int(0), 2), 2);
    }
}
