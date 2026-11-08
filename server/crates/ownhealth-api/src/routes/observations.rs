//! Lane A: `api/observations.py`. Observations, one analyte's trend, and user-added aliases
//! (CONTRACTS.md section 5).
//!
//! Values come back as printed by default (`units=as_reported`); `si` and `conventional` convert
//! with the normalizer and fall back to the printed value when a unit does not convert. No status
//! words are computed here: those come from lane E.

use std::collections::{BTreeSet, HashMap};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Days, NaiveDate, NaiveTime, TimeZone, Utc};
use ownhealth_core::Error;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, PrincipalKind, accessible_profiles};
use ownhealth_core::audit::{AuditEvent, hash_arg, write_audit};
use ownhealth_core::observations::{Observation, ObservationQuery, list_observations};
use ownhealth_core::pyjson::decimal_to_f64;
use ownhealth_core::records::list_records;
use ownhealth_normalize::{self as normalize, AliasIn, ResolveQuery, tables};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::extract::Caller;
use ownhealth_normalize::text::py_repr;

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/profiles/{p}/observations", get(observations))
        .route("/profiles/{p}/trend", get(trend))
        .route("/aliases", post(add_alias))
}

const CATEGORY_SCOPE: [(&str, &str); 5] = [
    ("laboratory", "read:labs"),
    ("vital-signs", "read:vitals"),
    ("activity", "read:wearables"),
    ("survey", "read:records"),
    ("exam", "read:records"),
];
const UNITS: [&str; 3] = ["si", "conventional", "as_reported"];

fn bad_input(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bad_input", message)
}

/// FastAPI's 422 for a query or body value that fails validation: `{"detail": [{type, loc, msg, input}]}`.
#[derive(Debug)]
pub struct Validation(Value);

impl axum::response::IntoResponse for Validation {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": [self.0]}))).into_response()
    }
}

pub enum Failure {
    Api(ApiError),
    Validation(Validation),
}

impl<E: Into<ApiError>> From<E> for Failure {
    fn from(e: E) -> Failure {
        Failure::Api(e.into())
    }
}

impl axum::response::IntoResponse for Failure {
    fn into_response(self) -> axum::response::Response {
        match self {
            Failure::Api(e) => e.into_response(),
            Failure::Validation(v) => v.into_response(),
        }
    }
}

type Answer = Result<Json<Value>, Failure>;

fn invalid(kind: &str, loc: &[&str], msg: &str, input: Value) -> Failure {
    Failure::Validation(Validation(
        json!({"type": kind, "loc": loc, "msg": msg, "input": input}),
    ))
}

/// The last value of a query parameter, as Starlette's `QueryParams.get`.
fn param<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn units_param(q: &[(String, String)]) -> Result<&str, Failure> {
    match param(q, "units") {
        None => Ok("as_reported"),
        Some(u) if UNITS.contains(&u) => Ok(u),
        Some(u) => Err(invalid(
            "literal_error",
            &["query", "units"],
            "Input should be 'si', 'conventional' or 'as_reported'",
            json!(u),
        )),
    }
}

fn date_param(q: &[(String, String)], key: &str) -> Result<Option<NaiveDate>, Failure> {
    match param(q, key) {
        None => Ok(None),
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map(Some).map_err(|_| {
            invalid(
                "date_from_datetime_parsing",
                &["query", key],
                "Input should be a valid date or datetime, invalid character in year",
                json!(s),
            )
        }),
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.is_empty())
}

/// A UCUM code as a person reads it: 10*3/uL -> 10³/µL, u[IU]/mL -> µIU/mL, {INR} -> ''.
pub fn pretty_unit(ucum: &str) -> String {
    let mut s = ucum.to_string();
    for (a, b) in [("10*12", "10¹²"), ("10*9", "10⁹"), ("10*6", "10⁶"), ("10*3", "10³")] {
        s = s.replace(a, b);
    }
    // re.sub(r"(?<![A-Za-z])u(?=g|L|mol|\[IU\]|kat|U)", "µ", s)
    let mut micro = String::with_capacity(s.len());
    let mut prev: Option<char> = None;
    for (i, c) in s.char_indices() {
        let rest = &s[i + c.len_utf8()..];
        let ahead = ["g", "L", "mol", "[IU]", "kat", "U"]
            .iter()
            .any(|p| rest.starts_with(p));
        if c == 'u' && !prev.is_some_and(|p| p.is_ascii_alphabetic()) && ahead {
            micro.push('µ');
        } else {
            micro.push(c);
        }
        prev = Some(c);
    }
    let s = micro
        .replace("[IU]", "IU")
        .replace("[arb'U]", "U")
        .replace("/{1.73_m2}", "/1.73 m²")
        .replace("mm[Hg]", "mmHg")
        .replace("/[HPF]", "/HPF")
        .replace("(24.h)", "24 h");
    // re.sub(r"\{[^}]*\}|\[pH\]", "", s)
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while !rest.is_empty() {
        if rest.starts_with("[pH]") {
            rest = &rest[4..];
            continue;
        }
        if rest.starts_with('{')
            && let Some(end) = rest.find('}')
        {
            rest = &rest[end + 1..];
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn group_unit(group: &str, key: &str) -> Option<String> {
    normalize::group_units(group)?
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// (value, unit as shown) for one observation in the requested units.
fn shown(r: &Observation, units: &str) -> ApiResult<(Option<f64>, Option<String>)> {
    let printed = r.printed_unit.clone();
    let Some(value) = r.value_num else {
        return Ok((None, printed));
    };
    if units == "as_reported" {
        return Ok((Some(decimal_to_f64(&value)), printed));
    }
    let target = non_empty(group_unit(&r.alias_group, units).as_deref()).map(String::from);
    if let (Some(target), Some(ucum)) = (target, non_empty(r.unit_ucum.as_deref())) {
        if target == ucum {
            return Ok((Some(decimal_to_f64(&value)), printed));
        }
        if let Some(conv) = normalize::convert(value, ucum, &target, &r.alias_group)? {
            return Ok((Some(decimal_to_f64(&conv)), Some(pretty_unit(&target))));
        }
    }
    Ok((Some(decimal_to_f64(&value)), printed))
}

/// Lane E's status word for this marker as of the point's day: the UI colours points with it and
/// never computes one itself.
async fn status(conn: &mut PgConnection, profile_id: Uuid, group: &str, day: NaiveDate) -> ApiResult<Value> {
    Ok(json!(ownhealth_rules::status_word(conn, profile_id, group, day).await?))
}

async fn labels(
    conn: &mut PgConnection,
    rows: &[Observation],
) -> ApiResult<(HashMap<String, Value>, HashMap<Uuid, Value>)> {
    let src_ids: Vec<String> = rows
        .iter()
        .map(|r| r.src_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let docs: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| r.document_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut src = HashMap::new();
    if !src_ids.is_empty() {
        let found: Vec<(String, Option<String>)> = sqlx::query_as("select id, label from src where id = any($1)")
            .bind(&src_ids)
            .fetch_all(&mut *conn)
            .await?;
        src = found.into_iter().map(|(id, l)| (id, json!(l))).collect();
    }
    let mut labs = HashMap::new();
    if !docs.is_empty() {
        let found: Vec<(Uuid, Option<String>)> =
            sqlx::query_as("select id, provider_name from document where id = any($1)")
                .bind(&docs)
                .fetch_all(&mut *conn)
                .await?;
        labs = found.into_iter().map(|(id, l)| (id, json!(l))).collect();
    }
    Ok((src, labs))
}

fn allowed(perm: &Permission, category: Option<&str>) -> Result<Vec<&'static str>, Failure> {
    if let Some(category) = non_empty(category) {
        let Some((cat, scope)) = CATEGORY_SCOPE.iter().find(|(c, _)| *c == category) else {
            return Err(bad_input(format!("Unknown category {}.", py_repr(category))).into());
        };
        if !perm.has(scope) {
            return Err(Error::access_denied("insufficient_scope", format!("This needs {scope}.")).into());
        }
        return Ok(vec![cat]);
    }
    let cats: Vec<&str> = CATEGORY_SCOPE
        .iter()
        .filter(|(_, s)| perm.has(s))
        .map(|(c, _)| *c)
        .collect();
    if cats.is_empty() {
        return Err(Error::access_denied(
            "insufficient_scope",
            "This needs read:labs, read:vitals or read:wearables.",
        )
        .into());
    }
    Ok(cats)
}

fn via(p: &Principal) -> &'static str {
    if p.kind == PrincipalKind::Token { "api" } else { "ui" }
}

async fn audit(
    conn: &mut PgConnection,
    principal: &Principal,
    perm: &Permission,
    action: &str,
    rows: &[Observation],
    args: Option<&str>,
) -> ApiResult<()> {
    let src_ids: Vec<String> = rows
        .iter()
        .map(|r| r.src_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut ev = AuditEvent::new(principal.actor_kind(), via(principal), action);
    ev.actor_id = principal.user_id;
    ev.token_id = principal.token_id;
    ev.profile_id = Some(perm.profile.id);
    ev.resource_type = Some("observation".into());
    ev.src_ids = (!src_ids.is_empty()).then_some(src_ids);
    ev.args_summary = non_empty(args).map(hash_arg);
    ev.rows_out = Some(i32::try_from(rows.len()).unwrap_or(i32::MAX));
    write_audit(conn, &ev).await?;
    Ok(())
}

async fn query_groups(conn: &mut PgConnection, q: &str, household_id: Uuid) -> ApiResult<BTreeSet<String>> {
    let res = normalize::resolve_analyte(
        Some(conn),
        q,
        &ResolveQuery {
            lang: "es",
            household_id: Some(household_id),
            ..Default::default()
        },
    )
    .await?;
    let mut groups: BTreeSet<String> = res.candidates.iter().map(|c| c.alias_group.clone()).collect();
    if res.reasons.is_empty() {
        groups.insert(res.alias_group);
    }
    Ok(groups)
}

fn day_of(r: &Observation) -> NaiveDate {
    r.effective_at.date_naive()
}

async fn observations(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> Answer {
    let units = units_param(&q)?;
    let from = date_param(&q, "from")?;
    let to = date_param(&q, "to")?;
    let text_q = non_empty(param(&q, "q"));
    let lang = param(&q, "lang").unwrap_or("es");
    let category = param(&q, "category");
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, None).await?;
    let cats = allowed(&perm, category)?;
    let loinc = non_empty(param(&q, "loinc").map(|l| l.strip_prefix("LOINC:").unwrap_or(l)));
    let query = ObservationQuery {
        alias_group: non_empty(param(&q, "alias_group")).map(String::from),
        loinc: loinc.map(|l| vec![l.to_string()]).unwrap_or_default(),
        category: non_empty(category).map(String::from),
        since: from.map(|d| Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN))),
        until: to
            .and_then(|d| d.and_hms_micro_opt(23, 59, 59, 999_999))
            .map(|t| Utc.from_utc_datetime(&t)),
        exclude_sensitive: !perm.has("read:sensitive"),
        ..Default::default()
    };
    let mut rows: Vec<Observation> = list_observations(&mut tx, perm.profile.id, &query)
        .await?
        .into_iter()
        .filter(|r| cats.contains(&r.category.as_str()))
        .collect();
    if let Some(text_q) = text_q {
        let groups = query_groups(&mut tx, text_q, perm.profile.household_id).await?;
        let fq = tables::fold(text_q);
        rows.retain(|r| groups.contains(&r.alias_group) || tables::fold(&r.printed_name).contains(&fq));
    }
    let (src, labs) = labels(&mut tx, &rows).await?;
    let mut items = Vec::with_capacity(rows.len());
    for r in &rows {
        let (value, unit) = shown(r, units)?;
        let name = non_empty(normalize::display_name(&r.alias_group, lang).as_deref())
            .map_or_else(|| r.printed_name.clone(), String::from);
        items.push(json!({
            "id": r.id,
            "src": r.src_id,
            "src_label": src.get(&r.src_id).cloned().unwrap_or(Value::Null),
            "name": name,
            "printed": {
                "name": r.printed_name,
                "value": r.printed_value,
                "unit": r.printed_unit,
                "range": r.printed_ref_range,
                "flag": r.printed_flag,
            },
            "value": value,
            "unit": unit,
            "loinc": r.loinc,
            "alias_group": r.alias_group,
            "interpretation": r.interpretation,
            "method": r.method,
            "date": day_of(r).format("%Y-%m-%d").to_string(),
            "lab": r.document_id.and_then(|d| labs.get(&d).cloned()).unwrap_or(Value::Null),
            "accepted_by": r.accepted_by,
            "status": r.status,
        }));
    }
    let action = if text_q.is_some() { "search" } else { "read" };
    audit(&mut tx, &caller.0, &perm, action, &rows, text_q).await?;
    tx.commit().await?;
    Ok(Json(json!({ "items": items })))
}

async fn group_for_code(
    conn: &mut PgConnection,
    profile_id: Uuid,
    code: &str,
    household_id: Uuid,
) -> ApiResult<Option<String>> {
    let c = code.strip_prefix("LOINC:").unwrap_or(code);
    if let Some(lc) = tables::loinc_codes().get(c) {
        return Ok(Some(lc.alias_group.clone()));
    }
    if tables::groups().contains_key(code) {
        return Ok(Some(code.to_string()));
    }
    let seen: Option<String> = sqlx::query_scalar(
        "select alias_group from observation where profile_id = $1 and (loinc = $2 or alias_group = $3) limit 1",
    )
    .bind(profile_id)
    .bind(c)
    .bind(code)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(seen) = seen.filter(|s| !s.is_empty()) {
        return Ok(Some(seen));
    }
    let res = normalize::resolve_analyte(
        Some(conn),
        code,
        &ResolveQuery {
            lang: "es",
            household_id: Some(household_id),
            ..Default::default()
        },
    )
    .await?;
    Ok(if res.reasons.iter().any(|r| r == "unmapped") {
        None
    } else {
        Some(res.alias_group)
    })
}

fn window_start(window: &str, today: NaiveDate) -> ApiResult<Option<NaiveDate>> {
    if window == "all" {
        return Ok(None);
    }
    let w = window.strip_suffix('\n').unwrap_or(window); // Python's `$` also matches before a final newline
    let bad = || bad_input("window is P<n>D, P<n>M, P<n>Y or all.");
    let body = w.strip_prefix('P').ok_or_else(bad)?;
    let (digits, unit) = body.split_at(body.len().saturating_sub(1));
    let per = match unit {
        "D" => 1u64,
        "M" => 30,
        "Y" => 365,
        _ => return Err(bad()),
    };
    if digits.is_empty() || digits.len() > 7 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let start = today.checked_sub_days(Days::new(n * per));
    let year_one = NaiveDate::from_ymd_opt(1, 1, 1).unwrap_or(NaiveDate::MIN);
    match start.filter(|d| *d >= year_one) {
        Some(d) => Ok(Some(d)),
        None => Err(bad_input("window reaches before year 1.")), // the calendar has no such day
    }
}

/// Python's `repr(float)` for the numbers we print: shortest round trip, a ".0" on whole numbers.
fn py_float(x: f64) -> String {
    let s = x.to_string();
    if x.is_finite() && !s.contains(['.', 'e', 'E']) {
        format!("{s}.0")
    } else {
        s
    }
}

/// Python's `round(x, 1)` for a float.
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

async fn trend(
    State(rt): State<Runtime>,
    caller: Caller,
    Path(p): Path<String>,
    Query(q): Query<Vec<(String, String)>>,
) -> Answer {
    let Some(code) = param(&q, "code") else {
        return Err(invalid("missing", &["query", "code"], "Field required", Value::Null));
    };
    let window = param(&q, "window").unwrap_or("P3Y");
    let units = units_param(&q)?;
    let lang = param(&q, "lang").unwrap_or("es");
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, &p, None, None).await?;
    let cats = allowed(&perm, None)?;
    let profile_id = perm.profile.id;
    let Some(group) = group_for_code(&mut tx, profile_id, code, perm.profile.household_id).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown",
            format!("No analyte matches {}.", py_repr(code)),
        )
        .into());
    };
    let today = chrono::Local::now().date_naive();
    let start = window_start(window, today)?;
    let query = ObservationQuery {
        alias_group: Some(group.clone()),
        since: start.map(|d| Utc.from_utc_datetime(&d.and_time(NaiveTime::MIN))),
        exclude_sensitive: !perm.has("read:sensitive"),
        ..Default::default()
    };
    let mut rows: Vec<Observation> = list_observations(&mut tx, profile_id, &query)
        .await?
        .into_iter()
        .filter(|r| cats.contains(&r.category.as_str()))
        .collect();
    rows.sort_by_key(|r| (r.effective_at, r.id.to_string()));
    let printed_ucums: BTreeSet<Option<String>> = rows
        .iter()
        .filter(|r| r.value_num.is_some())
        .map(|r| r.unit_ucum.clone())
        .collect();
    let has_group = normalize::group_units(&group).is_some();
    let target: Option<String> = if units == "as_reported" {
        if printed_ucums.len() == 1 {
            printed_ucums.into_iter().next().flatten()
        } else if has_group {
            group_unit(&group, "canonical")
        } else {
            None
        }
    } else {
        group_unit(&group, units)
    };
    let target_set = non_empty(target.as_deref());
    let (src, labs) = labels(&mut tx, &rows).await?;
    let mut points: Vec<Map<String, Value>> = Vec::new();
    let mut skipped = 0usize;
    let mut converted: BTreeSet<String> = BTreeSet::new();
    let mut statuses: HashMap<NaiveDate, Value> = HashMap::new();
    for r in &rows {
        let Some(num) = r.value_num else { continue };
        let ucum = r.unit_ucum.as_deref();
        let (value, lo, hi) = match (target_set, non_empty(ucum)) {
            (Some(t), Some(u)) if u != t => {
                converted.insert(u.to_string());
                let conv = |v: Option<_>| -> ApiResult<Option<_>> {
                    match v {
                        Some(v) => Ok(normalize::convert(v, u, t, &group)?),
                        None => Ok(None),
                    }
                };
                (conv(Some(num))?, conv(r.ref_low)?, conv(r.ref_high)?)
            }
            _ if target.is_none() || ucum == target.as_deref() => (Some(num), r.ref_low, r.ref_high),
            _ => (None, None, None),
        };
        let Some(value) = value else {
            skipped += 1;
            continue;
        };
        let day = day_of(r);
        if let std::collections::hash_map::Entry::Vacant(slot) = statuses.entry(day) {
            slot.insert(status(&mut tx, profile_id, &group, day).await?);
        }
        let printed: Vec<&str> = [Some(r.printed_value.as_str()), r.printed_unit.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect();
        let point = json!({
            "date": day.format("%Y-%m-%d").to_string(),
            "value": decimal_to_f64(&value),
            "unit": match target_set { Some(t) => json!(pretty_unit(t)), None => json!(r.printed_unit) },
            "printed": printed.join(" "),
            "range": r.printed_ref_range,
            "ref_low": lo.map(|v| decimal_to_f64(&v)),
            "ref_high": hi.map(|v| decimal_to_f64(&v)),
            "flag": r.interpretation,
            "method": r.method,
            "loinc": r.loinc,
            "lab": r.document_id.and_then(|d| labs.get(&d).cloned()).unwrap_or(Value::Null),
            "src": r.src_id,
            "src_label": src.get(&r.src_id).cloned().unwrap_or(Value::Null),
            "status": statuses.get(&day).cloned().unwrap_or(Value::Null),
        });
        if let Value::Object(m) = point {
            points.push(m);
        }
    }
    let codes: Vec<String> = rows
        .iter()
        .filter_map(|r| non_empty(r.loinc.as_deref()).map(|l| format!("LOINC:{l}")))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let es = lang == "es";
    let mut limitations: Vec<String> = Vec::new();
    let methods: BTreeSet<&str> = rows.iter().filter_map(|r| non_empty(r.method.as_deref())).collect();
    if codes.len() > 1 || methods.len() > 1 {
        for c in &codes {
            let bare = c.strip_prefix("LOINC:").unwrap_or(c);
            let n = points
                .iter()
                .filter(|pt| pt.get("loinc").and_then(Value::as_str) == Some(bare))
                .count();
            if let Some(lc) = tables::loinc_codes().get(bare)
                && n > 0
            {
                limitations.push(if es {
                    format!(
                        "{} {} ({c}).",
                        if n == 1 {
                            "1 punto es".to_string()
                        } else {
                            format!("{n} puntos son")
                        },
                        lc.long_name
                    )
                } else {
                    format!(
                        "{} {} ({c}).",
                        if n == 1 {
                            "1 point is".to_string()
                        } else {
                            format!("{n} points are")
                        },
                        lc.long_name
                    )
                });
            }
        }
    }
    let ranges: BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| non_empty(r.printed_ref_range.as_deref()))
        .collect();
    if ranges.len() > 1 {
        limitations.push(if es {
            "Los laboratorios usan rangos de referencia distintos.".into()
        } else {
            "Labs use different reference ranges.".into()
        });
    }
    let target_pretty = || target.as_deref().map(pretty_unit).unwrap_or_else(|| "None".into());
    for u in &converted {
        let m = tables::molar().get(&group).filter(|m| m.g_per_mol.is_some());
        let mass = m.map(|m| py_float(m.g_per_mol_text.parse::<f64>().unwrap_or(f64::NAN)));
        if es {
            let how = match (m, &mass) {
                (Some(m), Some(g)) => format!(" con la masa molar de {} ({g} g/mol)", m.species),
                _ => String::new(),
            };
            limitations.push(format!(
                "Los valores impresos en {} se muestran convertidos a {}{how}.",
                pretty_unit(u),
                target_pretty()
            ));
        } else {
            let how = match (m, &mass) {
                (Some(m), Some(g)) => format!(" with the molar mass of {} ({g} g/mol)", m.species),
                _ => String::new(),
            };
            limitations.push(format!(
                "Values printed in {} are shown converted to {}{how}.",
                pretty_unit(u),
                target_pretty()
            ));
        }
    }
    if skipped > 0 {
        let t = target_pretty();
        limitations.push(match (es, skipped) {
            (true, 1) => format!("1 valor en una unidad que no se convierte a {t} no se dibuja."),
            (true, n) => format!("{n} valores en una unidad que no se convierte a {t} no se dibujan."),
            (false, 1) => format!("1 value in a unit that does not convert to {t} is not drawn."),
            (false, n) => format!("{n} values in a unit that does not convert to {t} are not drawn."),
        });
    }
    let mut annotations: Vec<Value> = Vec::new();
    if perm.has("read:records") {
        let meds = list_records(&mut tx, "medication", profile_id, None, perm.has("read:sensitive")).await?;
        let date = |m: &Map<String, Value>, k: &str| {
            m.get(k)
                .and_then(Value::as_str)
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        };
        for m in &meds {
            let Some(started) = date(m, "started") else { continue };
            let stopped = date(m, "stopped");
            if start.is_none_or(|s| stopped.unwrap_or(today) >= s) {
                let label: Vec<&str> = ["name", "strength"]
                    .iter()
                    .filter_map(|k| m.get(*k).and_then(Value::as_str))
                    .filter(|s| !s.is_empty())
                    .collect();
                annotations.push(json!({
                    "date": started.format("%Y-%m-%d").to_string(),
                    "end": stopped.map(|d| d.format("%Y-%m-%d").to_string()),
                    "label": label.join(" "),
                    "src": m.get("src_id").cloned().unwrap_or(Value::Null),
                }));
            }
        }
        annotations.sort_by(|a, b| a["date"].as_str().cmp(&b["date"].as_str()));
    }
    let pack = ownhealth_rules::load_pack().map_err(Error::from)?;
    let rcv_pct: Option<f64> = pack
        .rules_of("rcv")
        .find(|r| {
            r.get("match")
                .and_then(Value::as_array)
                .is_some_and(|m| m.iter().any(|x| x.as_str() == Some(group.as_str())))
        })
        .and_then(|r| ownhealth_rules::engine::rcv_percent(r).parse::<f64>().ok());
    let (mut last_vs_prev, mut exceeds_rcv) = (Value::Null, Value::Null);
    let value_at = |i: usize| points.get(i).and_then(|p| p.get("value")).and_then(Value::as_f64);
    if points.len() >= 2
        && let (Some(prev), Some(last)) = (value_at(points.len() - 2), value_at(points.len() - 1))
        && prev != 0.0
    {
        let change = (last - prev) / prev * 100.0;
        last_vs_prev = json!(round1(change));
        if let Some(rcv) = rcv_pct {
            exceeds_rcv = json!(change.abs() > rcv);
        }
    }
    let mut options: Vec<String> = Vec::new();
    for key in ["canonical", "si", "conventional"] {
        if let Some(u) = has_group.then(|| group_unit(&group, key)).flatten() {
            let pu = pretty_unit(&u);
            if !options.contains(&pu) {
                options.push(pu);
            }
        }
    }
    audit(&mut tx, &caller.0, &perm, "read", &rows, None).await?;
    tx.commit().await?;
    let name = non_empty(normalize::display_name(&group, lang).as_deref())
        .map(String::from)
        .unwrap_or_else(|| rows.first().map_or_else(|| group.clone(), |r| r.printed_name.clone()));
    let unit = match target_set {
        Some(t) => json!(pretty_unit(t)),
        None => rows.first().map_or(json!(""), |r| json!(r.printed_unit)),
    };
    Ok(Json(json!({
        "name": name,
        "alias_group": group,
        "codes": codes,
        "unit": unit,
        "unit_options": options,
        "points": points,
        "annotations": annotations,
        "stats": {
            "n": points.len(),
            "last_vs_previous_pct": last_vs_prev,
            "rcv_pct": rcv_pct,
            "exceeds_rcv": exceeds_rcv,
        },
        "limitations": limitations,
    })))
}

async fn add_alias(State(rt): State<Runtime>, caller: Caller, body: axum::body::Bytes) -> Answer {
    let body: Value = serde_json::from_slice(&body)
        .map_err(|_| invalid("json_invalid", &["body"], "JSON decode error", Value::Null))?;
    let field = |k: &str| body.get(k).and_then(Value::as_str);
    for k in ["profile", "alias", "alias_group"] {
        match body.get(k) {
            None => return Err(invalid("missing", &["body", k], "Field required", body.clone())),
            Some(Value::String(_)) => {}
            Some(v) => {
                return Err(invalid(
                    "string_type",
                    &["body", k],
                    "Input should be a valid string",
                    v.clone(),
                ));
            }
        }
    }
    for k in ["lang", "loinc", "unit", "specimen"] {
        if let Some(v) = body.get(k).filter(|v| !v.is_null() && !v.is_string()) {
            return Err(invalid(
                "string_type",
                &["body", k],
                "Input should be a valid string",
                v.clone(),
            ));
        }
    }
    if let Some(l) = field("lang").filter(|l| !tables::LANGS.contains(l)) {
        return Err(invalid(
            "literal_error",
            &["body", "lang"],
            "Input should be 'es', 'en', 'de' or 'fr'",
            json!(l),
        ));
    }
    let (profile, alias, alias_group) = (
        field("profile").unwrap_or_default(),
        field("alias").unwrap_or_default(),
        field("alias_group").unwrap_or_default(),
    );
    let principal = &caller.0;
    let mut tx = rt.pool.begin().await?;
    let perm = caller.require(&mut tx, profile, None, Some("review")).await?;
    let household_id = perm.profile.household_id;
    let alias_id = normalize::add_alias(
        &mut tx,
        &AliasIn {
            household_id,
            alias,
            alias_group,
            lang: field("lang"),
            added_by: principal.user_id,
            unit: field("unit"),
            specimen: field("specimen"),
            loinc: field("loinc"),
        },
    )
    .await
    .map_err(|e| match e {
        Error::Invalid(m) => Failure::Api(bad_input(m)),
        other => Failure::Api(other.into()),
    })?;
    let mut changed = 0i64;
    for other in accessible_profiles(&mut tx, principal).await? {
        if other.profile.household_id == household_id && other.can("review") {
            changed += normalize::rederive(&mut tx, Some(other.profile.id), None).await?;
        }
    }
    let mut ev = AuditEvent::new(principal.actor_kind(), via(principal), "alias_add");
    ev.actor_id = principal.user_id;
    ev.token_id = principal.token_id;
    ev.profile_id = Some(perm.profile.id);
    ev.resource_type = Some("analyte_alias".into());
    ev.resource_ids = Some(vec![alias_id.to_string()]);
    ev.args_summary = Some(hash_arg(alias));
    write_audit(&mut tx, &ev).await?;
    tx.commit().await?;
    Ok(Json(
        json!({"id": alias_id, "alias_group": alias_group, "rederived": changed}),
    ))
}
