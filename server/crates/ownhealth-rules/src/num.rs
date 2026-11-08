//! Python's number behaviour where the state depends on it: `format(Decimal, "f")`, `quantize` with
//! ROUND_HALF_EVEN, `Decimal(str(x))`, `sum()` of floats (Neumaier since Python 3.12) and float4
//! columns read the way psycopg reads them (the shortest printed form, parsed as a double).

use std::str::FromStr;

use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;

/// `format(d, "f")`: no exponent, the precision the value carries.
pub fn num(d: &Decimal) -> String {
    d.to_string()
}

/// `d.quantize(Decimal(1).scaleb(-places), rounding=ROUND_HALF_EVEN)`: exactly `places` decimals.
pub fn quantize(d: Decimal, places: u32) -> Decimal {
    let mut r = d.round_dp_with_strategy(places, RoundingStrategy::MidpointNearestEven);
    r.rescale(places);
    r
}

/// `Decimal(s)` for a printed number (plain or with an exponent).
pub fn parse(s: &str) -> Option<Decimal> {
    let s = s.trim();
    Decimal::from_str(s).ok().or_else(|| Decimal::from_scientific(s).ok())
}

/// `dec(value)` of `state/pack.py`: `Decimal(str(value))`, None for None.
pub fn dec(v: Option<&Value>) -> Option<Decimal> {
    match v? {
        Value::Null => None,
        Value::String(s) => parse(s),
        Value::Number(n) => parse(&n.to_string()),
        Value::Bool(b) => Some(Decimal::from(i64::from(*b))),
        _ => None,
    }
}

/// `Decimal(str(f))`: the shortest repr of the float.
pub fn from_f64(f: f64) -> Option<Decimal> {
    if !f.is_finite() {
        return None;
    }
    parse(&format!("{f}"))
}

/// A float4 column as psycopg returns it: the text Postgres prints, parsed as a double.
pub fn f32_as_py(v: f32) -> f64 {
    format!("{v}").parse().unwrap_or(f64::from(v))
}

/// `sum(values)` over floats, as CPython 3.12+ adds them (Neumaier's compensated sum).
pub fn py_sum(values: &[f64]) -> f64 {
    let mut total = 0.0f64;
    let mut c = 0.0f64;
    for &x in values {
        let t = total + x;
        if total.abs() >= x.abs() {
            c += (total - t) + x;
        } else {
            c += (x - t) + total;
        }
        total = t;
    }
    if c != 0.0 && c.is_finite() { total + c } else { total }
}

/// `float(Decimal)`.
pub fn to_f64(d: &Decimal) -> f64 {
    ownhealth_core::pyjson::decimal_to_f64(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_keeps_the_scale_and_rounds_half_even() {
        assert_eq!(num(&quantize(Decimal::from(118), 1)), "118.0");
        assert_eq!(num(&quantize(parse("0.125").unwrap_or_default(), 2)), "0.12");
        assert_eq!(num(&quantize(parse("0.135").unwrap_or_default(), 2)), "0.14");
        assert_eq!(num(&parse("0.90").unwrap_or_default()), "0.90");
    }

    #[test]
    fn neumaier_matches_python() {
        // Python 3.13: sum([0.1] * 10) == 1.0; the naive left fold gives 0.9999999999999999.
        assert_eq!(py_sum(&[0.1; 10]), 1.0);
        assert_eq!(py_sum(&[1e100, 1.0, -1e100]), 1.0);
    }

    #[test]
    fn float4_reads_as_its_printed_form() {
        assert_eq!(f32_as_py(54.3f32), 54.3f64);
        assert_eq!(from_f64(54.3).map(|d| num(&d)), Some("54.3".into()));
    }
}
