//! The bits of Python's behaviour the importers depend on: truthiness and `str()` of JSON values,
//! `repr(float)`, `Decimal(str(v))`, `round()`, `json.dumps(sort_keys=True)`, `datetime.fromisoformat`
//! and `splitlines`, so the Rust importers read a file exactly as the Python ones did.

use std::str::FromStr;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use rust_decimal::Decimal;
use serde_json::{Map, Value};

/// Python truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `d.get(key)` when the value is truthy (`d.get(key) or ...`).
pub fn get_truthy<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    let x = v?.get(key)?;
    truthy(Some(x)).then_some(x)
}

/// `(d.get(key) or {})` as an object.
pub fn obj<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a Map<String, Value>> {
    v?.get(key)?.as_object()
}

/// `(d.get(key) or [])` as a list.
pub fn list<'a>(v: Option<&'a Value>, key: &str) -> &'a [Value] {
    v.and_then(|x| x.get(key))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// A truthy string field.
pub fn s<'a>(v: Option<&'a Value>, key: &str) -> Option<&'a str> {
    v?.get(key)?.as_str().filter(|x| !x.is_empty())
}

/// `repr(float)` (and `str(float)`): the shortest round-trip digits, fixed notation for decimal
/// exponents in [-4, 16), else `d.ddde±XX`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..16).contains(&exp) {
        let fixed = format!("{f}");
        if fixed.contains('.') {
            fixed
        } else {
            format!("{fixed}.0")
        }
    } else {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    }
}

/// `str(v)` of a JSON value as Python printed the loaded object.
pub fn py_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => number_str(n),
        Some(other) => other.to_string(),
    }
}

fn number_str(n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        n.to_string()
    } else {
        float_repr(n.as_f64().unwrap_or(f64::NAN))
    }
}

fn decimal_from_text(t: &str) -> Option<Decimal> {
    let t = t.trim();
    Decimal::from_str(t).ok().or_else(|| Decimal::from_scientific(t).ok())
}

/// `Decimal(str(v))` for a number or a string; None for None, a bool, or text that is not a number
/// (the `_dec` helper of the FHIR mapping).
pub fn dec(v: Option<&Value>) -> Option<Decimal> {
    match v? {
        Value::Number(n) => decimal_from_text(&number_str(n)),
        Value::String(s) => decimal_from_text(s),
        _ => None,
    }
}

/// `format(Decimal(str(v)), "f")` for an int or float, `str(v)` for anything else (`_num_text`).
pub fn num_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Number(n)) if n.is_f64() => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            // Rust prints the shortest digits in fixed notation; Python keeps a ".0" on integral floats
            // below 1e16 (`Decimal("100.0")`), and expands exponents (`Decimal("1e-05")` -> 0.00001).
            if f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        other => py_str(other),
    }
}

/// Python's `round(x, n)`: correctly rounded, ties to even on the exact binary value.
pub fn round(x: f64, n: usize) -> f64 {
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// `json.dumps(v, sort_keys=True)`: `", "` and `": "` separators, ASCII only.
pub fn dumps_sorted(v: &Value) -> String {
    let mut out = String::new();
    dump(v, true, &mut out);
    out
}

/// `json.dumps(v)`: keys in insertion order.
pub fn dumps(v: &Value) -> String {
    let mut out = String::new();
    dump(v, false, &mut out);
    out
}

fn dump(v: &Value, sort: bool, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                let f = n.as_f64().unwrap_or(f64::NAN);
                out.push_str(&match f {
                    f if f.is_nan() => "NaN".to_string(),
                    f if f.is_infinite() && f > 0.0 => "Infinity".to_string(),
                    f if f.is_infinite() => "-Infinity".to_string(),
                    f => float_repr(f),
                });
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => ascii_str(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                dump(item, sort, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            if sort {
                keys.sort();
            }
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                ascii_str(k, out);
                out.push_str(": ");
                dump(&map[k.as_str()], sort, out);
            }
            out.push('}');
        }
    }
}

fn ascii_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `bytes.splitlines()` / `str.splitlines()` for `\n`, `\r\n` and `\r`.
pub fn splitlines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                out.push(&data[start..i]);
                start = i + 1;
            }
            b'\r' => {
                out.push(&data[start..i]);
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

fn digits(s: &str) -> Option<u32> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

/// `date.fromisoformat`: `YYYY-MM-DD` or `YYYYMMDD`.
pub fn fromisoformat_date(s: &str) -> Option<NaiveDate> {
    let (y, m, d) = match s.len() {
        10 if s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' => (s.get(0..4)?, s.get(5..7)?, s.get(8..10)?),
        8 => (s.get(0..4)?, s.get(4..6)?, s.get(6..8)?),
        _ => return None,
    };
    NaiveDate::from_ymd_opt(digits(y)? as i32, digits(m)?, digits(d)?)
}

fn parse_time(t: &str) -> Option<NaiveTime> {
    let (clock, frac) = match t.find(['.', ',']) {
        Some(i) => (&t[..i], Some(&t[i + 1..])),
        None => (t, None),
    };
    let parts: Vec<&str> = if clock.contains(':') {
        clock.split(':').collect()
    } else {
        clock
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect()
    };
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
        return None;
    }
    let h = digits(parts[0])?;
    let m = parts.get(1).map_or(Some(0), |p| digits(p))?;
    let sec = parts.get(2).map_or(Some(0), |p| digits(p))?;
    let micros = match frac {
        None => 0,
        Some(f) => {
            if parts.len() != 3 || f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let six: String = f.chars().chain(std::iter::repeat('0')).take(6).collect();
            six.parse().ok()?
        }
    };
    NaiveTime::from_hms_micro_opt(h, m, sec, micros)
}

fn parse_offset(z: &str) -> Option<i32> {
    if z == "Z" || z == "z" {
        return Some(0);
    }
    let sign = match z.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = &z[1..];
    let (clock, frac) = match body.find('.') {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let parts: Vec<&str> = if clock.contains(':') {
        clock.split(':').collect()
    } else {
        clock
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect()
    };
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) || frac.is_some() {
        return None;
    }
    let h = digits(parts[0])? as i32;
    let m = parts.get(1).map_or(Some(0), |p| digits(p))? as i32;
    let sec = parts.get(2).map_or(Some(0), |p| digits(p))? as i32;
    if h > 23 || m > 59 || sec > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + sec))
}

/// `datetime.fromisoformat(s)` of an aware datetime; None when it does not parse or has no offset.
pub fn fromisoformat_aware(s: &str) -> Option<DateTime<FixedOffset>> {
    let (date_part, rest) = if s.len() > 10 && s.as_bytes().get(4) == Some(&b'-') {
        (s.get(..10)?, s.get(10..)?)
    } else {
        (s.get(..8)?, s.get(8..)?)
    };
    let date = fromisoformat_date(date_part)?;
    let mut chars = rest.chars();
    let sep = chars.next()?;
    if sep.is_ascii_digit() {
        return None;
    }
    let rest = chars.as_str();
    let tz_at = rest.find(['+', '-', 'Z', 'z'])?;
    let time = parse_time(&rest[..tz_at])?;
    let offset = FixedOffset::east_opt(parse_offset(&rest[tz_at..])?)?;
    NaiveDateTime::new(date, time).and_local_timezone(offset).single()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn floats_print_like_python() {
        for (f, want) in [
            (5.9, "5.9"),
            (100.0, "100.0"),
            (1e-05, "1e-05"),
            (1.5e16, "1.5e+16"),
            (0.0001, "0.0001"),
            (123456789.0, "123456789.0"),
        ] {
            assert_eq!(float_repr(f), want);
        }
        assert_eq!(num_text(Some(&json!(100.0))), "100.0");
        assert_eq!(num_text(Some(&json!(1e-05))), "0.00001");
        assert_eq!(num_text(Some(&json!(68))), "68");
        assert_eq!(dec(Some(&json!(5.9))).map(|d| d.to_string()), Some("5.9".into()));
        assert_eq!(dec(Some(&json!(true))), None);
        assert_eq!(dec(Some(&json!("abc"))), None);
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(64.45, 1), 64.5);
    }

    #[test]
    fn dumps_like_python() {
        // python3 -c 'import json; print(json.dumps({"b": [1, 2.5, None], "a": "é\n\x7f", "c": True}, sort_keys=True))'
        let v = json!({"b": [1, 2.5, null], "a": "é\n\u{7f}", "c": true});
        assert_eq!(
            dumps_sorted(&v),
            r#"{"a": "\u00e9\n\u007f", "b": [1, 2.5, null], "c": true}"#
        );
    }

    #[test]
    fn isoformat_like_python() {
        let p = |s: &str| fromisoformat_aware(s).map(|d| d.to_rfc3339());
        assert_eq!(p("2026-05-12T09:00:00+02:00"), Some("2026-05-12T09:00:00+02:00".into()));
        assert_eq!(p("2026-05-12T09:00+02:00"), Some("2026-05-12T09:00:00+02:00".into()));
        assert_eq!(p("2026-05-12T09+02:00"), Some("2026-05-12T09:00:00+02:00".into()));
        assert_eq!(
            p("2026-05-12T09:00:00.123456789+00:00"),
            Some("2026-05-12T09:00:00.123456+00:00".into())
        );
        assert_eq!(p("2026-05-12T09:00:00+0200"), Some("2026-05-12T09:00:00+02:00".into()));
        assert_eq!(p("20260512T090000+00:00"), Some("2026-05-12T09:00:00+00:00".into()));
        assert_eq!(p("2026-05-12 09:00:00"), None);
        assert_eq!(p("2026-05-12T24:00:00+00:00"), None);
        assert_eq!(splitlines(b"a\r\nb\rc\n\nd"), vec![&b"a"[..], b"b", b"c", b"", b"d"]);
    }
}
