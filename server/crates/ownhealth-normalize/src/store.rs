//! The parts of `normalize/__init__.py` that read or write the database: `normalize_row` with its
//! duplicate check, household aliases, `rederive` and its job.

use chrono::{DateTime, Datelike, Duration, FixedOffset, Local, NaiveDate, TimeZone, Utc};
use futures::future::BoxFuture;
use ownhealth_core::acceptance::Check;
use ownhealth_core::fhir::received_ucum;
use ownhealth_core::jobs::{self, EnqueueOptions, Job, JobContext, JobError};
use ownhealth_core::observations::{Observation, ObservationIn, supersede_observation};
use ownhealth_core::provenance::SrcRef;
use ownhealth_core::pyjson::py_decimal_str;
use ownhealth_core::review::{ReviewItemIn, create_item};
use ownhealth_core::{Error, Result};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection};
use uuid::Uuid;

use crate::text::{looks_like_range, py_repr};
use crate::{
    DeriveIn, Derived, Normalized, ParsedValue, RefRange, RowIn, derivation_version, derive, parse_range, parse_value,
    pick_range, tables,
};

const LANE: &str = "lane A (ownhealth-normalize)";

fn fail(name: &str, reason: &str, detail: String) -> Check {
    Check {
        detail: Some(detail),
        ..Check::fail(name, reason)
    }
}

#[derive(sqlx::FromRow, Default)]
struct ProfileFacts {
    household_id: Option<Uuid>,
    sex_at_birth: Option<String>,
    birth_date: Option<NaiveDate>,
}

async fn profile(conn: &mut PgConnection, profile_id: Uuid) -> Result<ProfileFacts> {
    Ok(
        sqlx::query_as("select household_id, sex_at_birth, birth_date from profile where id = $1")
            .bind(profile_id)
            .fetch_optional(conn)
            .await?
            .unwrap_or_default(),
    )
}

fn age(birth: Option<NaiveDate>, on: NaiveDate) -> Option<i32> {
    let b = birth?;
    Some(on.year() - b.year() - i32::from((on.month(), on.day()) < (b.month(), b.day())))
}

/// One printed lab row to an ObservationIn plus every validator result (dimension, plausibility,
/// flag mismatch, range parse, duplicate, conflict). The caller adds grounding and identity checks
/// and routes the row through `core::review::route_fact`.
pub async fn normalize_row(conn: &mut PgConnection, row: &RowIn<'_>) -> Result<Normalized> {
    let prof = profile(&mut *conn, row.profile_id).await?;
    let pv = parse_value(row.printed_value, row.lang, row.printed_unit)?;
    let mut checks: Vec<Check> = Vec::new();
    if let Some(reason) = &pv.reason {
        checks.push(fail(
            "value",
            reason,
            format!(
                "the value {} reads two ways in a {} report",
                py_repr(row.printed_value),
                row.lang
            ),
        ));
    }
    if looks_like_range(row.printed_value) {
        checks.push(fail(
            "value_is_range",
            "value_is_range",
            format!("the value {} looks like a range", py_repr(row.printed_value)),
        ));
    } else if pv.value.is_none() && pv.text_code.is_none() {
        checks.push(fail(
            "value",
            "implausible",
            format!("no number or known result in {}", py_repr(row.printed_value)),
        ));
    }

    let printed_range = row.printed_range.filter(|r| !r.is_empty());
    let ranges = match printed_range {
        Some(r) => parse_range(r, row.lang, row.printed_unit)?,
        None => Vec::new(),
    };
    let on = row.effective_at.date_naive();
    let mut range: Option<RefRange> = if ranges.is_empty() {
        None
    } else {
        pick_range(&ranges, prof.sex_at_birth.as_deref(), age(prof.birth_date, on)).cloned()
    };
    if let Some(pr) = printed_range.filter(|r| !r.trim().is_empty()) {
        if ranges.is_empty() {
            checks.push(fail(
                "range",
                "range_parse",
                format!("cannot read the range {}", py_repr(pr)),
            ));
        } else if let Some(r) = &range {
            if let (Some(lo), Some(hi)) = (r.low, r.high)
                && lo >= hi
            {
                checks.push(fail(
                    "range",
                    "range_parse",
                    format!("low {} is not below high {}", py_decimal_str(&lo), py_decimal_str(&hi)),
                ));
                range = None;
            } else {
                checks.push(Check::pass("range"));
            }
        } else {
            checks.push(fail(
                "range",
                "range_parse",
                "no printed range applies to this profile's sex and age".into(),
            ));
        }
    }

    let (derived, _res, dchecks) = derive(
        Some(&mut *conn),
        &DeriveIn {
            printed_name: row.printed_name,
            printed_unit: row.printed_unit,
            value: pv.value,
            comparator: pv.comparator.as_deref(),
            text_code: pv.text_code.as_deref(),
            range: range.as_ref(),
            printed_flag: row.printed_flag,
            lang: row.lang,
            specimen: row.specimen,
            method: row.method,
            provider: row.provider,
            loinc: row.loinc,
            household_id: prof.household_id,
        },
    )
    .await?;
    checks.extend(dchecks);
    checks.extend(
        duplicates(
            &mut *conn,
            row.profile_id,
            &derived,
            &pv,
            row.effective_at,
            row.document_id,
        )
        .await?,
    );
    let ref_applies = range
        .as_ref()
        .map(|r| r.applies_to.clone())
        .filter(|a| a.as_object().is_some_and(|o| !o.is_empty()));
    let observation = ObservationIn {
        profile_id: row.profile_id,
        category: "laboratory".into(),
        source_kind: "clinical".into(),
        printed_name: row.printed_name.into(),
        printed_value: row.printed_value.into(),
        printed_unit: row.printed_unit.map(String::from),
        printed_ref_range: row.printed_range.map(String::from),
        printed_flag: row.printed_flag.map(String::from),
        value_num: pv.value,
        value_cmp: pv.comparator.clone(),
        value_text: pv.text_code.as_ref().map(|_| row.printed_value.to_string()),
        value_code: pv.text_code.clone(),
        method: derived.method,
        ref_low: range.as_ref().and_then(|r| r.low),
        ref_high: range.as_ref().and_then(|r| r.high),
        ref_applies,
        specimen: derived.specimen,
        loinc: derived.loinc,
        alias_group: derived.alias_group,
        unit_ucum: derived.unit_ucum,
        value_canonical: derived.value_canonical,
        unit_canonical: derived.unit_canonical,
        interpretation: derived.interpretation,
        derivation_version: derivation_version(),
        effective_at: row.effective_at,
        document_id: None,
        candidate_row_id: None,
        report_key: None,
        sensitivity: Vec::new(),
        fhir: None,
    };
    Ok(Normalized { observation, checks })
}

#[derive(sqlx::FromRow)]
struct Accepted {
    id: Uuid,
    value_canonical: Option<Decimal>,
    value_num: Option<Decimal>,
    value_code: Option<String>,
    value_cmp: Option<String>,
    method: Option<String>,
    document_id: Option<Uuid>,
}

fn blank(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !s.is_empty())
}

/// One fact from two sources (architecture.md, dedup): same alias group and day, equal value, the
/// same comparator and the same method (both empty counts as equal), from another source. A direct
/// and a calculated LDL are two analytes, not a conflict; `<5` beside `5` is a conflict; rows of one
/// source are never merged.
async fn duplicates(
    conn: &mut PgConnection,
    profile_id: Uuid,
    derived: &Derived,
    pv: &ParsedValue,
    effective_at: DateTime<FixedOffset>,
    document_id: Option<Uuid>,
) -> Result<Vec<Check>> {
    let rows: Vec<Accepted> = sqlx::query_as(
        "select id, value_canonical, value_num, value_code, value_cmp, method, document_id from observation \
         where profile_id = $1 and alias_group = $2 and status in ('current', 'provisional') \
         and superseded_at is null and effective_at between $3 and $4 order by effective_at, id",
    )
    .bind(profile_id)
    .bind(&derived.alias_group)
    .bind(effective_at - Duration::days(1))
    .bind(effective_at + Duration::days(1))
    .fetch_all(conn)
    .await?;
    let rows: Vec<Accepted> = rows
        .into_iter()
        .filter(|r| blank(&r.method) == blank(&derived.method))
        .collect();
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mine = derived.value_canonical.or(pv.value);
    let tolerance = Decimal::new(1, 6);
    let mut conflict: Option<Uuid> = None;
    for r in &rows {
        let theirs = r.value_canonical.or(r.value_num);
        let same_number = match (mine, theirs) {
            (Some(a), Some(b)) => match (a.checked_sub(b), b.abs().checked_mul(tolerance)) {
                (Some(diff), Some(limit)) => diff.abs() <= limit,
                _ => false,
            },
            _ => false,
        };
        let same_code = pv
            .text_code
            .as_deref()
            .is_some_and(|c| !c.is_empty() && Some(c) == r.value_code.as_deref());
        if (same_number || same_code) && blank(&r.value_cmp) == pv.comparator.as_deref().filter(|c| !c.is_empty()) {
            if document_id.is_some() && r.document_id == document_id {
                continue; // the same source twice: never merged
            }
            return Ok(vec![fail(
                "duplicate",
                "duplicate",
                format!("the same value is already accepted as observation {}", r.id),
            )]);
        }
        conflict = conflict.or(Some(r.id));
    }
    Ok(match conflict {
        None => Vec::new(),
        Some(id) => vec![fail(
            "conflict",
            "conflict",
            format!("another value for this analyte and day is accepted ({id})"),
        )],
    })
}

// -------------------------------------------------------------------- aliases ----

#[derive(Debug, Clone, Default)]
pub struct AliasIn<'a> {
    pub household_id: Uuid,
    pub alias: &'a str,
    pub alias_group: &'a str,
    pub lang: Option<&'a str>,
    pub added_by: Option<Uuid>,
    pub unit: Option<&'a str>,
    pub specimen: Option<&'a str>,
    pub loinc: Option<&'a str>,
}

/// A household's own alias ("add alias" from the review queue). The group must be a curated one.
/// Python's `AliasError` is `Error::Invalid` here.
pub async fn add_alias(conn: &mut PgConnection, a: &AliasIn<'_>) -> Result<Uuid> {
    if !tables::groups().contains_key(a.alias_group) {
        return Err(Error::Invalid(format!(
            "unknown alias_group {}",
            py_repr(a.alias_group)
        )));
    }
    if let Some(l) = a.loinc.filter(|l| !l.is_empty())
        && tables::loinc_codes()
            .get(l)
            .is_none_or(|c| c.alias_group != a.alias_group)
    {
        return Err(Error::Invalid(format!("LOINC {l} is not a code of {}", a.alias_group)));
    }
    let norm = tables::fold(a.alias);
    if norm.is_empty() || tables::panels().contains(&norm) {
        return Err(Error::Invalid("a panel or category name cannot be an alias".into()));
    }
    Ok(sqlx::query_scalar(
        "insert into analyte_alias (household_id, alias, alias_norm, lang, alias_group, loinc, unit, specimen, \
         added_by) values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         on conflict (coalesce(household_id::text, ''), alias_norm, coalesce(lang, ''), coalesce(unit, ''), \
         coalesce(specimen, '')) do update set alias_group = excluded.alias_group, loinc = excluded.loinc, \
         alias = excluded.alias, added_by = excluded.added_by returning id",
    )
    .bind(a.household_id)
    .bind(a.alias)
    .bind(&norm)
    .bind(a.lang)
    .bind(a.alias_group)
    .bind(a.loinc)
    .bind(a.unit)
    .bind(a.specimen)
    .bind(a.added_by)
    .fetch_one(conn)
    .await?)
}

// ------------------------------------------------------------------- rederive ----

#[derive(sqlx::FromRow)]
struct RederiveRow {
    #[sqlx(flatten)]
    o: Observation,
    doc_lang: Option<String>,
    provider_name: Option<String>,
    household_id: Uuid,
    sex_at_birth: Option<String>,
    birth_date: Option<NaiveDate>,
    src_kind: Option<String>,
    /// `effective_at.date()` as psycopg gives it: in the session's time zone.
    effective_date: NaiveDate,
}

/// `_json`: decimals as their `str`, everything else as it is.
fn json_of_decimal(v: Option<Decimal>) -> Value {
    v.map_or(Value::Null, |d| Value::String(py_decimal_str(&d)))
}

fn json_of_str(v: &Option<String>) -> Value {
    v.as_ref().map_or(Value::Null, |s| Value::String(s.clone()))
}

/// The derived columns that changed, in `_DERIVED` order, as `[old, new]` pairs.
fn diff(o: &Observation, d: &Derived) -> Map<String, Value> {
    let mut out = Map::new();
    let mut text = |k: &str, a: &Option<String>, b: &Option<String>| {
        if a != b {
            out.insert(k.into(), json!([json_of_str(a), json_of_str(b)]));
        }
    };
    text("loinc", &o.loinc, &d.loinc);
    let group = Some(o.alias_group.clone());
    text("alias_group", &group, &Some(d.alias_group.clone()));
    text("method", &o.method, &d.method);
    text("unit_ucum", &o.unit_ucum, &d.unit_ucum);
    if o.value_canonical != d.value_canonical {
        out.insert(
            "value_canonical".into(),
            json!([json_of_decimal(o.value_canonical), json_of_decimal(d.value_canonical)]),
        );
    }
    let mut text = |k: &str, a: &Option<String>, b: &Option<String>| {
        if a != b {
            out.insert(k.into(), json!([json_of_str(a), json_of_str(b)]));
        }
    };
    text("unit_canonical", &o.unit_canonical, &d.unit_canonical);
    text("interpretation", &o.interpretation, &d.interpretation);
    out
}

fn reparse(
    printed: &str,
    lang: &str,
    unit: Option<&str>,
    sex: Option<&str>,
    age: Option<i32>,
) -> Result<Option<RefRange>> {
    let ranges = parse_range(printed, lang, unit)?;
    Ok(pick_range(&ranges, sex, age).cloned())
}

/// Recompute the derived layer (loinc, alias group, canonical, interpretation) after a table change.
///
/// Observation rows are immutable, so a changed row is superseded by a copy with the new derived
/// layer and the same source; truth and parsed columns never change. Only current rows are touched
/// (a provisional row waits for its final report). One `rederive_diff` review item lists what
/// changed, and each touched profile gets a `state.rebuild` job. Returns the changed count.
pub async fn rederive(conn: &mut PgConnection, profile_id: Option<Uuid>, since: Option<NaiveDate>) -> Result<i64> {
    let version = derivation_version();
    let since = since.and_then(|d| {
        let midnight = d.and_hms_opt(0, 0, 0)?;
        Local
            .from_local_datetime(&midnight)
            .earliest()
            .map(|t| t.with_timezone(&Utc))
    });
    let sql = "select o.*, d.language as doc_lang, d.provider_name, p.household_id as household_id, \
               p.sex_at_birth, p.birth_date, s.kind as src_kind, o.effective_at::date as effective_date \
               from observation o join profile p on p.id = o.profile_id left join src s on s.id = o.src_id \
               left join document d on d.id = o.document_id where o.status = 'current' and o.superseded_at is null \
               and o.category = 'laboratory' and ($1::uuid is null or o.profile_id = $1) \
               and ($2::timestamptz is null or o.effective_at >= $2) \
               order by o.profile_id, o.effective_at, o.id for update of o";
    let rows: Vec<RederiveRow> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(since)
        .fetch_all(&mut *conn)
        .await?;
    let mut changes: indexmap::IndexMap<Uuid, (Uuid, Vec<Value>)> = indexmap::IndexMap::new();
    for row in rows {
        let r = &row.o;
        let lang = match row.doc_lang.as_deref() {
            Some(l) if tables::LANGS.contains(&l) => l,
            _ => "en",
        };
        let sex = row.sex_at_birth.as_deref();
        let age_then = age(row.birth_date, row.effective_date);
        let printed_range = r.printed_ref_range.as_deref().filter(|s| !s.is_empty());
        let mut range = None;
        if r.ref_low.is_some() || r.ref_high.is_some() {
            let applies = r
                .ref_applies
                .clone()
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| Value::Object(Map::new()));
            let mut stored = RefRange {
                low: r.ref_low,
                high: r.ref_high,
                text: r.printed_ref_range.clone().unwrap_or_default(),
                applies_to: applies,
                low_inclusive: true,
                high_inclusive: true,
                text_code: None,
            };
            if let Some(pr) = printed_range {
                // keep exclusive bounds and text codes as printed
                if let Some(again) = reparse(pr, lang, r.printed_unit.as_deref(), sex, age_then)?
                    && (again.low, again.high) == (r.ref_low, r.ref_high)
                {
                    stored = again;
                }
            }
            range = Some(stored);
        } else if let Some(pr) = printed_range {
            range = reparse(pr, lang, r.printed_unit.as_deref(), sex, age_then)?;
        }
        // a structured source's LOINC and UCUM code are truth: keep them, whoever accepted the row
        let fhir_import = row.src_kind.as_deref() == Some("fhir_import");
        let received = r.accepted_by == "import" || fhir_import;
        let keep_code = r
            .loinc
            .as_deref()
            .filter(|l| received && tables::loinc_codes().contains_key(*l));
        let keep_unit = if fhir_import {
            received_ucum(Some(&r.fhir), r.loinc.as_deref())
        } else {
            None
        };
        let printed_unit = keep_unit
            .as_deref()
            .filter(|u| !u.is_empty())
            .or(r.printed_unit.as_deref());
        let method = r.method.as_deref().filter(|m| matches!(*m, "direct" | "calculated"));
        let (derived, _res, _) = derive(
            Some(&mut *conn),
            &DeriveIn {
                printed_name: &r.printed_name,
                printed_unit,
                value: r.value_num,
                comparator: r.value_cmp.as_deref(),
                text_code: r.value_code.as_deref(),
                range: range.as_ref(),
                printed_flag: r.printed_flag.as_deref(),
                lang,
                specimen: r.specimen.as_deref(),
                method,
                provider: row.provider_name.as_deref(),
                loinc: keep_code,
                household_id: Some(row.household_id),
            },
        )
        .await?;
        if derived.loinc.is_none() && r.loinc.is_some() {
            continue; // never lose a code a row already had
        }
        let changed = diff(r, &derived);
        if changed.is_empty() {
            continue;
        }
        let new = ObservationIn {
            profile_id: r.profile_id,
            category: r.category.clone(),
            source_kind: r.source_kind.clone(),
            printed_name: r.printed_name.clone(),
            printed_value: r.printed_value.clone(),
            printed_unit: r.printed_unit.clone(),
            printed_ref_range: r.printed_ref_range.clone(),
            printed_flag: r.printed_flag.clone(),
            value_num: r.value_num,
            value_cmp: r.value_cmp.clone(),
            value_text: r.value_text.clone(),
            value_code: r.value_code.clone(),
            method: derived.method,
            ref_low: r.ref_low,
            ref_high: r.ref_high,
            ref_applies: r.ref_applies.clone().filter(|v| !v.is_null()),
            specimen: r.specimen.clone(),
            loinc: derived.loinc,
            alias_group: derived.alias_group,
            unit_ucum: derived.unit_ucum,
            value_canonical: derived.value_canonical,
            unit_canonical: derived.unit_canonical,
            interpretation: derived.interpretation,
            derivation_version: version.clone(),
            effective_at: r.effective_at.fixed_offset(),
            document_id: r.document_id,
            candidate_row_id: r.candidate_row_id,
            report_key: r.report_key.clone(),
            sensitivity: r.sensitivity.clone(),
            fhir: if received { Some(r.fhir.clone()) } else { None },
        };
        let committed = supersede_observation(
            &mut *conn,
            r.id,
            &new,
            &SrcRef::Id(r.src_id.clone()),
            &r.accepted_by,
            &format!("rederive {version}"),
            r.reviewed_by,
        )
        .await?;
        changes
            .entry(r.profile_id)
            .or_insert_with(|| (row.household_id, Vec::new()))
            .1
            .push(json!({
                "old": r.id.to_string(),
                "new": committed.id.to_string(),
                "src": r.src_id,
                "changed": Value::Object(changed),
            }));
    }
    let mut total = 0i64;
    for (pid, (household_id, items)) in changes {
        total += items.len() as i64;
        create_item(
            &mut *conn,
            &ReviewItemIn {
                kind: "rederive_diff".into(),
                household_id: Some(household_id),
                profile_id: Some(pid),
                payload: json!({"derivation_version": version, "rows": items}),
                ..Default::default()
            },
        )
        .await?;
        let key = format!("state.rebuild:{pid}");
        jobs::enqueue(
            &mut *conn,
            "state.rebuild",
            &json!({"profile_id": pid.to_string()}),
            EnqueueOptions::dedupe(&key),
        )
        .await?;
    }
    Ok(total)
}

/// Job `normalize.rederive`: re-derive one profile's observations, or every profile's, with the
/// tables in force. Rows whose derived layer changed are superseded; the printed layer never changes.
fn rederive_job(ctx: JobContext, job: Job) -> BoxFuture<'static, jobs::HandlerResult> {
    Box::pin(async move {
        let profile_id = match job.payload.get("profile_id") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or_else(|| JobError::permanent("payload has no valid profile_id"))?,
            ),
        };
        let mut tx = ctx.pool.begin().await?;
        let changed = rederive(&mut tx, profile_id, None).await?;
        tx.commit().await?;
        Ok(Some(json!({"changed": changed})))
    })
}

/// Register lane A's job handler (`normalize.rederive`). Lane A has no demo hook.
pub fn register() {
    tracing::debug!(lane = LANE, "register");
    jobs::register_handler("normalize.rederive", rederive_job);
}
