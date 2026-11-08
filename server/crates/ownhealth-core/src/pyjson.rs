//! How the Python app turned values into text and JSON, so Rust writes the same bytes.
//!
//! * `py_decimal_str`: `str(Decimal)` (the canonical JSON of the state hash keeps decimals as text).
//! * `py_isoformat*`: `datetime.isoformat()` / `date.isoformat()`.
//! * `pydantic_datetime`: pydantic v2 JSON mode (`Z` for UTC), used in review payloads.
//! * `decimal_json`: FastAPI's `jsonable_encoder` for a Decimal (int when the exponent is >= 0).
//! * `py_num`: `core.fhir._num` (int when the value is integral, else float).
//! * `row_to_json`: a `select *` row as the dict FastAPI would have sent.
//!
//! Timestamps come back from Postgres in UTC and print with `+00:00`, as psycopg prints them on a
//! server whose TimeZone is UTC (the Postgres image default).

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, Offset, Timelike, Utc};
use rust_decimal::Decimal;
use serde_json::{Map, Number, Value};
use sqlx::postgres::{PgRow, PgValueRef};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use uuid::Uuid;

use crate::error::{Error, Result};

/// The coefficient digits and exponent of a decimal: value = digits * 10^exponent.
fn digits_exponent(d: &Decimal) -> (bool, String, i64) {
    let negative = d.is_sign_negative();
    let digits = d.mantissa().unsigned_abs().to_string();
    (negative, digits, -(d.scale() as i64))
}

/// Python's `str(Decimal)` (the "to-scientific-string" rule): `Decimal("0.90")` -> `"0.90"`,
/// `Decimal("0.0000001")` -> `"1E-7"`.
pub fn py_decimal_str(d: &Decimal) -> String {
    let (negative, digits, exp) = digits_exponent(d);
    let n = digits.len() as i64;
    let adjusted = exp + n - 1;
    let body = if exp <= 0 && adjusted >= -6 {
        if exp == 0 {
            digits
        } else if n > -exp {
            let cut = (n + exp) as usize;
            format!("{}.{}", &digits[..cut], &digits[cut..])
        } else {
            format!("0.{}{}", "0".repeat((-exp - n) as usize), digits)
        }
    } else {
        let (head, tail) = digits.split_at(1);
        let frac = if tail.is_empty() {
            String::new()
        } else {
            format!(".{tail}")
        };
        let sign = if adjusted > 0 { "+" } else { "" };
        format!("{head}{frac}E{sign}{adjusted}")
    };
    if negative { format!("-{body}") } else { body }
}

fn finite_number(f: f64) -> Value {
    Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

/// A decimal as the float Python would make of it (`float(Decimal)`).
pub fn decimal_to_f64(d: &Decimal) -> f64 {
    py_decimal_str(d).parse::<f64>().unwrap_or(f64::NAN)
}

fn integer_json(d: &Decimal) -> Value {
    let t = d.trunc();
    match i64::try_from(t.mantissa() / 10i128.pow(t.scale())) {
        Ok(i) => Value::Number(i.into()),
        Err(_) => finite_number(decimal_to_f64(d)),
    }
}

/// FastAPI's `decimal_encoder`: int when the exponent is >= 0 (`Decimal("97")`), else float
/// (`Decimal("97.0")` -> `97.0`).
pub fn decimal_json(d: &Decimal) -> Value {
    if d.scale() == 0 {
        integer_json(d)
    } else {
        finite_number(decimal_to_f64(d))
    }
}

/// `core.fhir._num`: int when the value is integral (`Decimal("97.0")` -> `97`), else float.
pub fn py_num(d: &Decimal) -> Value {
    if d.fract().is_zero() {
        integer_json(d)
    } else {
        finite_number(decimal_to_f64(d))
    }
}

/// A `real` (float4) as Python read it from the text Postgres prints: the shortest form of the
/// f32 (`157.98`, never `157.97999572753906`).
pub fn f32_json(v: f32) -> Value {
    finite_number(format!("{v}").parse::<f64>().unwrap_or(f64::NAN))
}

fn offset_text(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let s = seconds.unsigned_abs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if sec == 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}:{m:02}:{sec:02}")
    }
}

fn naive_iso(dt: &NaiveDateTime) -> String {
    let micros = dt.nanosecond() / 1000;
    let base = dt.format("%Y-%m-%dT%H:%M:%S").to_string();
    if micros == 0 {
        base
    } else {
        format!("{base}.{micros:06}")
    }
}

/// `datetime.isoformat()` of an aware datetime: `2026-09-14T08:00:00+00:00`, microseconds only
/// when they are not zero.
pub fn py_isoformat(dt: &DateTime<FixedOffset>) -> String {
    format!(
        "{}{}",
        naive_iso(&dt.naive_local()),
        offset_text(dt.offset().local_minus_utc())
    )
}

pub fn py_isoformat_utc(dt: &DateTime<Utc>) -> String {
    py_isoformat(&dt.with_timezone(&Utc.fix()))
}

/// `date.isoformat()`.
pub fn py_date(d: &NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// pydantic v2 JSON mode for a datetime: like isoformat, but `Z` for a zero offset.
pub fn pydantic_datetime(dt: &DateTime<FixedOffset>) -> String {
    let off = dt.offset().local_minus_utc();
    let tail = if off == 0 { "Z".to_string() } else { offset_text(off) };
    format!("{}{tail}", naive_iso(&dt.naive_local()))
}

pub fn pydantic_datetime_utc(dt: &DateTime<Utc>) -> String {
    pydantic_datetime(&dt.with_timezone(&Utc.fix()))
}

fn decode<'r, T: sqlx::Decode<'r, sqlx::Postgres>>(v: PgValueRef<'r>) -> Result<T> {
    T::decode(v).map_err(|e| Error::Db(sqlx::Error::Decode(e)))
}

fn array<'r, T, F>(v: PgValueRef<'r>, f: F) -> Result<Value>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
    Vec<Option<T>>: sqlx::Decode<'r, sqlx::Postgres>,
    F: Fn(T) -> Value,
{
    let items: Vec<Option<T>> = decode(v)?;
    Ok(Value::Array(
        items.into_iter().map(|i| i.map(&f).unwrap_or(Value::Null)).collect(),
    ))
}

/// One column value as FastAPI's `jsonable_encoder` would send the Python value psycopg returned.
pub fn value_to_json(v: PgValueRef<'_>) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let ty = v.type_info().name().to_string();
    Ok(match ty.as_str() {
        "BOOL" => Value::Bool(decode(v)?),
        "INT2" => Value::from(decode::<i16>(v)?),
        "INT4" => Value::from(decode::<i32>(v)?),
        "INT8" => Value::from(decode::<i64>(v)?),
        "FLOAT4" => f32_json(decode(v)?),
        "FLOAT8" => finite_number(decode(v)?),
        "NUMERIC" => decimal_json(&decode::<Decimal>(v)?),
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CITEXT" => Value::String(decode(v)?),
        "UUID" => Value::String(decode::<Uuid>(v)?.to_string()),
        "DATE" => Value::String(py_date(&decode(v)?)),
        "TIMESTAMPTZ" => Value::String(py_isoformat_utc(&decode(v)?)),
        "TIMESTAMP" => Value::String(naive_iso(&decode(v)?)),
        "JSON" | "JSONB" => decode::<Value>(v)?,
        "INET" | "CIDR" => {
            let net: ipnet::IpNet = decode(v)?;
            let text = if net.prefix_len() == net.max_prefix_len() {
                net.addr().to_string()
            } else {
                net.to_string()
            };
            Value::String(text)
        }
        "BYTEA" => Value::String(String::from_utf8_lossy(&decode::<Vec<u8>>(v)?).into_owned()),
        "TEXT[]" | "VARCHAR[]" | "NAME[]" => array::<String, _>(v, Value::String)?,
        "UUID[]" => array::<Uuid, _>(v, |u| Value::String(u.to_string()))?,
        "INT2[]" => array::<i16, _>(v, Value::from)?,
        "INT4[]" => array::<i32, _>(v, Value::from)?,
        "INT8[]" => array::<i64, _>(v, Value::from)?,
        "FLOAT4[]" => array::<f32, _>(v, f32_json)?,
        "FLOAT8[]" => array::<f64, _>(v, finite_number)?,
        "NUMERIC[]" => array::<Decimal, _>(v, |d| decimal_json(&d))?,
        "BOOL[]" => array::<bool, _>(v, Value::Bool)?,
        other => {
            return Err(Error::Invalid(format!(
                "row_to_json: no JSON form for a {other} column"
            )));
        }
    })
}

/// A whole row, keys in column order (Python's `dict(row._mapping)`).
pub fn row_to_json(row: &PgRow) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    for (i, col) in row.columns().iter().enumerate() {
        out.insert(col.name().to_string(), value_to_json(row.try_get_raw(i)?)?);
    }
    Ok(out)
}

/// Parse a decimal the way pydantic's lax mode does: a string (`"2.51"`, `"1E-7"`), an integer, or
/// a float through its shortest text (`2.51`, never `2.5099999`).
pub fn parse_decimal(v: &Value) -> std::result::Result<Decimal, String> {
    let text = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        other => return Err(format!("not a decimal: {other}")),
    };
    let parsed = if text.contains(['e', 'E']) {
        Decimal::from_scientific(&text)
    } else {
        text.parse::<Decimal>()
    };
    parsed.map_err(|_| format!("not a decimal: {text:?}"))
}

/// serde for `Option<Decimal>` fields of the pydantic models: written as `str(Decimal)` (pydantic
/// JSON mode), read leniently (`parse_decimal`).
pub mod opt_decimal {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serializer};
    use serde_json::Value;

    pub fn serialize<S: Serializer>(v: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(d) => s.serialize_str(&super::py_decimal_str(d)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
        match Option::<Value>::deserialize(d)? {
            None | Some(Value::Null) => Ok(None),
            Some(v) => super::parse_decimal(&v).map(Some).map_err(serde::de::Error::custom),
        }
    }
}

/// Parse an aware datetime as pydantic does: RFC 3339 (`Z` or an offset), a space for the `T`
/// allowed. A naive datetime is refused: every stored time is aware.
pub fn parse_datetime(text: &str) -> std::result::Result<DateTime<FixedOffset>, String> {
    let t = text.trim().replacen(' ', "T", 1);
    DateTime::parse_from_rfc3339(&t).map_err(|_| format!("not an aware datetime: {text:?}"))
}

/// serde for `DateTime<FixedOffset>` fields of the pydantic models (pydantic JSON mode).
pub mod datetime {
    use chrono::{DateTime, FixedOffset};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &DateTime<FixedOffset>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::pydantic_datetime(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<FixedOffset>, D::Error> {
        super::parse_datetime(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// serde for `Option<DateTime<FixedOffset>>`.
pub mod opt_datetime {
    use chrono::{DateTime, FixedOffset};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<DateTime<FixedOffset>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(dt) => s.serialize_str(&super::pydantic_datetime(dt)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<FixedOffset>>, D::Error> {
        match Option::<String>::deserialize(d)? {
            None => Ok(None),
            Some(t) => super::parse_datetime(&t).map(Some).map_err(serde::de::Error::custom),
        }
    }
}

/// Serializers for typed rows sent as JSON: the shapes `row_to_json` gives.
pub mod row {
    use chrono::{DateTime, Utc};
    use rust_decimal::Decimal;
    use serde::Serializer;
    use serde::ser::Serialize;

    pub fn decimal<S: Serializer>(v: &Option<Decimal>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(d) => super::decimal_json(d).serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn ts<S: Serializer>(v: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::py_isoformat_utc(v))
    }

    pub fn opt_ts<S: Serializer>(v: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(dt) => s.serialize_str(&super::py_isoformat_utc(dt)),
            None => s.serialize_none(),
        }
    }

    pub fn opt_f32<S: Serializer>(v: &Option<f32>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(f) => super::f32_json(*f).serialize(s),
            None => s.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::TimeZone;

    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).expect("decimal")
    }

    #[test]
    fn decimal_text_matches_python_str() {
        for (input, want) in [
            ("0.90", "0.90"),
            ("100", "100"),
            ("97.0", "97.0"),
            ("0.0000001", "1E-7"),
            ("0.000001", "0.000001"),
            ("0.00000123", "0.00000123"),
            ("0.000000123", "1.23E-7"),
            ("-2.51", "-2.51"),
            ("0", "0"),
            ("0.00", "0.00"),
            ("123456.789", "123456.789"),
        ] {
            assert_eq!(py_decimal_str(&d(input)), want, "{input}");
        }
    }

    #[test]
    fn decimal_json_follows_fastapi_and_fhir_rules() {
        assert_eq!(decimal_json(&d("97")), serde_json::json!(97));
        assert_eq!(serde_json::to_string(&decimal_json(&d("97.0"))).expect("json"), "97.0");
        assert_eq!(py_num(&d("97.0")), serde_json::json!(97));
        assert_eq!(serde_json::to_string(&py_num(&d("2.51"))).expect("json"), "2.51");
        assert_eq!(serde_json::to_string(&f32_json(157.98)).expect("json"), "157.98");
    }

    #[test]
    fn datetimes_print_like_python() {
        let utc = Utc.with_ymd_and_hms(2026, 9, 14, 8, 0, 0).single().expect("dt");
        assert_eq!(py_isoformat_utc(&utc), "2026-09-14T08:00:00+00:00");
        let plus2 = FixedOffset::east_opt(7200)
            .expect("offset")
            .with_ymd_and_hms(2026, 9, 14, 8, 0, 0)
            .single()
            .expect("dt")
            + chrono::Duration::microseconds(123000);
        assert_eq!(py_isoformat(&plus2), "2026-09-14T08:00:00.123000+02:00");
        assert_eq!(pydantic_datetime(&plus2), "2026-09-14T08:00:00.123000+02:00");
        assert_eq!(pydantic_datetime(&utc.fixed_offset()), "2026-09-14T08:00:00Z");
        assert_eq!(
            parse_datetime("2026-09-14 08:00:00+00:00").expect("dt"),
            utc.fixed_offset()
        );
        assert!(parse_datetime("2026-09-14T08:00:00").is_err());
    }

    #[test]
    fn decimals_parse_leniently() {
        assert_eq!(parse_decimal(&serde_json::json!(2.51)).expect("d"), d("2.51"));
        assert_eq!(parse_decimal(&serde_json::json!("1E-7")).expect("d"), d("0.0000001"));
        assert_eq!(parse_decimal(&serde_json::json!(79)).expect("d"), d("79"));
        assert!(parse_decimal(&serde_json::json!("x")).is_err());
    }
}
