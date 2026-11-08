//! Evaluate the rule pack over one profile's facts at one date (`state/engine.py`). Pure reads; no
//! wall clock inside.
//!
//! Inputs are the rows visible at `known_at` (None: the current view) with `effective_at` before the
//! end of the `as_of` day, minus rows tagged sensitive (they are counted, never evaluated). For a minor,
//! the genome gate keeps adult-onset, sensitive and carrier findings out.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc};
use ownhealth_core::access::{Profile, age_of_majority, get_profile};
use ownhealth_core::lifecycle::visible_sql;
use ownhealth_core::records::{active_medications, list_records};
use ownhealth_core::{Error, Result};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgConnection, Row};
use uuid::Uuid;

use crate::genome;
use crate::num::{self, dec, from_f64, num, py_sum, quantize};
use crate::pack::{Pack, PackError, System, int, text, truthy};

pub const FLAGGED: [&str; 5] = ["H", "HH", "L", "LL", "A"];

pub fn status_rank(s: &str) -> u8 {
    match s {
        "out" => 4,
        "watch" => 3,
        "ok" => 2,
        "stale" => 1,
        _ => 0,
    }
}

/// Inclusive end of the as_of day in UTC: `effective_at < day_end(as_of)`.
pub fn day_end(as_of: NaiveDate) -> DateTime<Utc> {
    (as_of + Duration::days(1)).and_time(NaiveTime::MIN).and_utc()
}

pub fn iso(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// A `YYYY-MM-DD` field of a row read with `row_to_json`.
pub fn date_of(row: &Map<String, Value>, key: &str) -> Option<NaiveDate> {
    row.get(key).and_then(Value::as_str).and_then(parse_date)
}

pub fn str_of<'a>(row: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn opt(s: Option<&str>) -> Value {
    s.map_or(Value::Null, |s| Value::String(s.into()))
}

#[derive(Debug, Clone)]
pub struct Point {
    pub id: Uuid,
    pub date: NaiveDate,
    /// value_num, the printed number parsed
    pub value: Option<Decimal>,
    pub canonical: Option<Decimal>,
    pub canonical_unit: Option<String>,
    pub printed_value: String,
    pub unit: Option<String>,
    /// the printed unit in UCUM, the unit ref_low and ref_high are in
    pub unit_ucum: Option<String>,
    pub ref_text: Option<String>,
    pub ref_low: Option<Decimal>,
    pub ref_high: Option<Decimal>,
    /// lab interpretation, or the printed flag
    pub flag: Option<String>,
    pub loinc: Option<String>,
    pub method: Option<String>,
    pub src: String,
    pub src_label: Option<String>,
    pub derivation: String,
}

impl Point {
    pub fn shown(&self) -> String {
        self.value.as_ref().map_or_else(|| self.printed_value.clone(), num)
    }

    pub fn comparable(&self) -> Option<Decimal> {
        self.canonical.or(self.value)
    }

    /// The unit `comparable()` is in: two points compare only when these match.
    pub fn comparable_unit(&self) -> Option<&str> {
        if self.canonical.is_some() {
            self.canonical_unit.as_deref()
        } else {
            self.unit.as_deref()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Wearable {
    pub value: Option<Decimal>,
    pub newest: NaiveDate,
    pub spark: Vec<Option<Decimal>>,
    pub n_days: usize,
}

#[derive(Debug, Clone)]
pub struct Marker {
    /// alias group, or wearable metric
    pub key: String,
    pub system: String,
    /// labs | wearables
    pub source: String,
    pub status: &'static str,
    pub stale: bool,
    /// the rule id that set the status
    pub rule: Option<String>,
    pub window_days: Option<i64>,
    pub recency_rule: Option<String>,
    /// chronological (labs)
    pub points: Vec<Point>,
    pub wearable: Option<Wearable>,
}

impl Marker {
    pub fn latest(&self) -> Option<&Point> {
        self.points.last()
    }

    pub fn newest(&self) -> Option<NaiveDate> {
        self.points
            .last()
            .map(|p| p.date)
            .or_else(|| self.wearable.as_ref().map(|w| w.newest))
    }
}

#[derive(Debug, Clone)]
pub struct SystemState {
    pub key: String,
    pub file: String,
    pub status: &'static str,
    pub rule: String,
    pub newest: Option<NaiveDate>,
    /// indexes into `Evaluation::markers`, in the system's marker order
    pub markers: Vec<usize>,
}

/// A lane D finding read into the fields the rules use (`_finding`).
#[derive(Debug, Clone, Default)]
pub struct Finding {
    pub id: Option<String>,
    pub category: Option<String>,
    pub gene: Option<String>,
    pub diplotype: Option<String>,
    pub phenotype: Option<String>,
    pub activity_score: Option<Decimal>,
    pub call_state: Option<String>,
    pub rsid: Option<String>,
    pub evidence: Option<String>,
    pub src: Option<String>,
    pub confidence: Value,
}

fn str_or<'a>(d: &'a Map<String, Value>, a: &str, b: &str) -> Option<&'a str> {
    d.get(a)
        .filter(|v| truthy(v))
        .or_else(|| d.get(b))
        .and_then(Value::as_str)
}

impl Finding {
    pub fn from_value(v: &Value) -> Finding {
        let empty = Map::new();
        let d = v.as_object().unwrap_or(&empty);
        let value = d.get("value").and_then(Value::as_object).unwrap_or(&empty);
        let s = |k: &str| d.get(k).and_then(Value::as_str).map(str::to_string);
        let from_value = |k: &str| {
            d.get(k)
                .filter(|v| truthy(v))
                .or_else(|| value.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Finding {
            id: str_or(d, "id", "fact_id").map(str::to_string),
            category: s("category"),
            gene: s("gene"),
            diplotype: from_value("diplotype"),
            phenotype: from_value("phenotype"),
            activity_score: dec(d.get("activity_score")),
            call_state: s("call_state"),
            rsid: s("rsid"),
            evidence: s("evidence"),
            src: str_or(d, "src", "src_id").map(str::to_string),
            confidence: d.get("confidence").cloned().unwrap_or(Value::Null),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub profile_id: Uuid,
    pub slug: String,
    pub as_of: NaiveDate,
    pub profile: Profile,
    pub minor: bool,
    pub markers: Vec<Marker>,
    pub flags: Vec<Map<String, Value>>,
    pub stale: Vec<Map<String, Value>>,
    pub withheld_sensitive: i64,
    pub series: HashMap<String, Vec<Point>>,
    pub systems: Vec<SystemState>,
    pub never_measured: Vec<Map<String, Value>>,
    pub medications: Vec<Map<String, Value>>,
    pub supplements: Vec<Map<String, Value>>,
    pub conditions: Vec<Map<String, Value>>,
    pub notes: Vec<Value>,
    pub pending: Vec<Value>,
    pub corrections: Vec<Value>,
    pub freshness: Map<String, Value>,
    pub findings: Vec<Finding>,
    pub genome_withheld_sensitive: i64,
    pub genome_withheld_minor: i64,
    pub genome_assay: Option<Map<String, Value>>,
    pub derivation: String,
}

impl Evaluation {
    pub fn marker(&self, key: &str) -> Option<&Marker> {
        self.markers.iter().find(|m| m.key == key)
    }
}

// ------------------------------------------------------------------ loading ----

async fn load_points(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    known_at: Option<DateTime<Utc>>,
    group: Option<&str>,
    pack: &Pack,
) -> Result<(HashMap<String, Vec<Point>>, i64)> {
    let mut where_ = vec![
        "o.profile_id = $1".to_string(),
        visible_sql("o", "$2"),
        "o.effective_at < $3".to_string(),
    ];
    let mut codes: Vec<String> = Vec::new();
    if let Some(g) = group {
        codes = pack
            .analytes
            .get(g)
            .and_then(|a| a.get("loinc"))
            .and_then(Value::as_array)
            .map(|c| c.iter().map(text).collect())
            .unwrap_or_default();
        where_.push("(o.alias_group = any($4) or o.loinc = any($5))".into());
    }
    let sql = format!(
        "select o.id, o.alias_group, o.loinc, o.effective_at, o.value_num, o.value_canonical, o.unit_canonical, \
         o.printed_value, o.printed_unit, o.unit_ucum, o.printed_ref_range, o.ref_low, o.ref_high, \
         o.interpretation, o.printed_flag, o.method, o.src_id, o.derivation_version, \
         cardinality(o.sensitivity) > 0 as sensitive, s.label \
         from observation o join src s on s.id = o.src_id where {} order by o.effective_at, o.recorded_at, o.id",
        where_.join(" and ")
    );
    let mut q = sqlx::query(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(known_at)
        .bind(day_end(as_of));
    if let Some(g) = group {
        q = q.bind(vec![g.to_string()]).bind(codes);
    }
    let rows = q.fetch_all(&mut *conn).await?;
    let mut series: HashMap<String, Vec<Point>> = HashMap::new();
    let mut sensitive = 0;
    for r in rows {
        if r.try_get::<bool, _>("sensitive")? {
            sensitive += 1;
            continue;
        }
        let mut group: String = r.try_get("alias_group")?;
        let loinc: Option<String> = r.try_get("loinc")?;
        if !pack.analytes.contains_key(&group)
            && let Some(g) = loinc.as_ref().and_then(|l| pack.loinc_to_group.get(l))
        {
            group = g.clone();
        }
        let printed: Option<String> = r.try_get("printed_flag")?;
        let printed = printed.map(|p| p.trim().to_uppercase()).filter(|p| !p.is_empty());
        let interpretation: Option<String> = r.try_get("interpretation")?;
        let effective: DateTime<Utc> = r.try_get("effective_at")?;
        series.entry(group).or_default().push(Point {
            id: r.try_get("id")?,
            date: effective.date_naive(),
            value: r.try_get("value_num")?,
            canonical: r.try_get("value_canonical")?,
            canonical_unit: r.try_get("unit_canonical")?,
            printed_value: r.try_get("printed_value")?,
            unit: r.try_get("printed_unit")?,
            unit_ucum: r.try_get("unit_ucum")?,
            ref_text: r.try_get("printed_ref_range")?,
            ref_low: r.try_get("ref_low")?,
            ref_high: r.try_get("ref_high")?,
            flag: interpretation.filter(|s| !s.is_empty()).or(printed),
            loinc,
            method: r.try_get("method")?,
            src: r.try_get("src_id")?,
            src_label: r.try_get("label")?,
            derivation: r.try_get("derivation_version")?,
        });
    }
    Ok((series, sensitive))
}

type Day = (NaiveDate, Option<f64>, Option<f64>);
type RollupRow = (String, i16, DateTime<Utc>, Option<f32>, Option<f32>);
type BySource = Vec<(i16, Vec<Day>)>;

/// Daily rollups of the last 90 days per metric, one source per metric (source_priority, else the
/// source with the most days). metric -> [(day, avg, sum)] chronological.
async fn wearables(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    metrics: &[String],
) -> Result<HashMap<String, Vec<Day>>> {
    if metrics.is_empty() {
        return Ok(HashMap::new());
    }
    let cut = day_end(as_of);
    let rows: Vec<RollupRow> = sqlx::query_as(
        "select r.metric, r.source_id, r.t, r.v_avg, r.v_sum from wearable_rollup r \
         where r.profile_id = $1 and r.bucket = 'day' and r.metric = any($2) and r.t < $3 and r.t >= $4 \
         order by r.metric, r.t",
    )
    .bind(profile_id)
    .bind(metrics)
    .bind(cut)
    .bind(cut - Duration::days(90))
    .fetch_all(&mut *conn)
    .await?;
    let prio: HashMap<String, Vec<i16>> =
        sqlx::query_as::<_, (String, Vec<i16>)>("select metric, source_ids from source_priority where profile_id = $1")
            .bind(profile_id)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    // metric -> [(source, days)] in first-seen order
    let mut by_metric: Vec<(String, BySource)> = Vec::new();
    for (metric, source, t, avg, sum) in rows {
        let day = (t.date_naive(), avg.map(num::f32_as_py), sum.map(num::f32_as_py));
        let idx = match by_metric.iter().position(|(m, _)| *m == metric) {
            Some(i) => i,
            None => {
                by_metric.push((metric, Vec::new()));
                by_metric.len() - 1
            }
        };
        let sources = &mut by_metric[idx].1;
        match sources.iter_mut().find(|(s, _)| *s == source) {
            Some((_, days)) => days.push(day),
            None => sources.push((source, vec![day])),
        }
    }
    let mut out = HashMap::new();
    for (metric, sources) in by_metric {
        let preferred = prio
            .get(&metric)
            .and_then(|ids| ids.iter().find(|id| sources.iter().any(|(s, _)| s == *id)).copied());
        let chosen = preferred.or_else(|| {
            sources
                .iter()
                .min_by_key(|(s, days)| (std::cmp::Reverse(days.len()), *s))
                .map(|(s, _)| *s)
        });
        if let Some(days) = sources.into_iter().find(|(s, _)| Some(*s) == chosen).map(|(_, d)| d) {
            out.insert(metric, days);
        }
    }
    Ok(out)
}

fn round1(v: Option<f64>, places: u32) -> Option<Decimal> {
    v.and_then(from_f64).map(|d| quantize(d, places))
}

fn mean(vals: &[f64]) -> Option<f64> {
    (!vals.is_empty()).then(|| py_sum(vals) / vals.len() as f64)
}

/// `_wearable_marker(spec, days, as_of)`.
pub fn wearable_marker(spec: &Value, days: &[Day], as_of: NaiveDate) -> Option<Wearable> {
    let last = days.last()?;
    let since = as_of - Duration::days(29);
    let last30: Vec<&Day> = days.iter().filter(|d| d.0 >= since).collect();
    let agg = spec.get("agg").and_then(Value::as_str).unwrap_or("avg30");
    let pick = |d: &Day| if agg == "sum30" { d.2 } else { d.1 };
    let value = if agg == "latest" {
        last.1
    } else {
        let vals: Vec<f64> = last30.iter().filter_map(|d| pick(d)).collect();
        mean(&vals)
    };
    let mut spark = Vec::new();
    if agg == "latest" {
        let from = days.len().saturating_sub(6);
        spark = days[from..].iter().map(|d| round1(d.1, 1)).collect();
    } else {
        let start = as_of - Duration::days(89);
        for b in 0..6 {
            let lo = start + Duration::days(15 * b);
            let hi = start + Duration::days(15 * (b + 1));
            let vals: Vec<f64> = days.iter().filter(|d| lo <= d.0 && d.0 < hi).filter_map(pick).collect();
            if let Some(m) = mean(&vals) {
                spark.push(round1(Some(m), 1));
            }
        }
    }
    let metric = spec.get("metric").and_then(Value::as_str).unwrap_or_default();
    let places = if ["steps", "hr_resting", "hrv_rmssd", "sleep_asleep"].contains(&metric) {
        0
    } else {
        1
    };
    Some(Wearable {
        value: round1(value, places),
        newest: last.0,
        spark,
        n_days: last30.len(),
    })
}

// -------------------------------------------------------------------- rules ----

fn d(s: &str) -> Decimal {
    num::parse(s).unwrap_or_default()
}

/// Reference change value in %, two-sided 95%: z * sqrt(2) * sqrt(CVa^2 + CVi^2) (Fraser).
fn rcv_value(rule: &Map<String, Value>) -> Option<Decimal> {
    use rust_decimal::MathematicalOps;
    let cvi = dec(rule.get("cv_i"))?;
    let cva = dec(rule.get("cv_a"))?;
    let sqrt2 = Decimal::from(2).sqrt()?;
    let inner = cvi.checked_mul(cvi)?.checked_add(cva.checked_mul(cva)?)?.sqrt()?;
    d("1.96").checked_mul(sqrt2)?.checked_mul(inner)
}

pub fn rcv_percent(rule: &Map<String, Value>) -> String {
    rcv_value(rule).map(|v| num(&quantize(v, 1))).unwrap_or_default()
}

fn rule_id(rule: &Map<String, Value>) -> String {
    rule.get("id").map(text).unwrap_or_default()
}

fn severity(rule: &Map<String, Value>, default: i64) -> i64 {
    rule.get("severity")
        .map_or(Some(default), |v| int(Some(v)))
        .unwrap_or(default)
}

/// `_rcv(rule, pts)`.
pub fn rcv(rule: &Map<String, Value>, pts: &[Point]) -> Option<Map<String, Value>> {
    if pts.len() < 2 {
        return None;
    }
    let (a, b) = (&pts[pts.len() - 2], &pts[pts.len() - 1]);
    let (va, vb) = (a.comparable()?, b.comparable()?);
    if va.is_zero() || a.comparable_unit() != b.comparable_unit() {
        return None;
    }
    let rcv = rcv_value(rule)?;
    let change = vb.checked_sub(va)?.checked_div(va)?.checked_mul(Decimal::from(100))?;
    if change.abs() <= rcv {
        return None;
    }
    let out = json!({
        "kind": "rcv",
        "reason": "RCV_CHANGE",
        "rule": rule_id(rule),
        "severity": severity(rule, 1),
        "value": b.shown(),
        "prev": a.shown(),
        "prev_date": iso(a.date),
        "unit": opt(b.unit.as_deref()),
        "change_pct": num(&quantize(change, 1)),
        "rcv_pct": num(&quantize(rcv, 1)),
        "date": iso(b.date),
        "src": [a.src, b.src],
    });
    out.as_object().cloned()
}

/// `_trend(rule, pts)`.
pub fn trend(rule: &Map<String, Value>, pts: &[Point]) -> Option<Map<String, Value>> {
    let n = usize::try_from(int(rule.get("points")).unwrap_or(3)).ok()?;
    let usable: Vec<&Point> = pts.iter().filter(|p| p.comparable().is_some()).collect();
    // one unit per series: a point the normalizer could not convert must not read as a jump
    let tail: Vec<&Point> = match usable.last() {
        Some(last) => {
            let unit = last.comparable_unit();
            let same: Vec<&Point> = usable.iter().copied().filter(|p| p.comparable_unit() == unit).collect();
            same[same.len().saturating_sub(n)..].to_vec()
        }
        None => Vec::new(),
    };
    let dates: BTreeSet<NaiveDate> = tail.iter().map(|p| p.date).collect();
    if tail.len() < n || dates.len() < n || n == 0 {
        return None;
    }
    let vals: Vec<Decimal> = tail.iter().filter_map(|p| p.comparable()).collect();
    let up = rule.get("direction").and_then(Value::as_str).unwrap_or("up") == "up";
    if !vals.windows(2).all(|w| if up { w[1] > w[0] } else { w[1] < w[0] }) {
        return None;
    }
    let x0 = tail[0].date;
    let year = d("365.25");
    let xs: Vec<Decimal> = tail
        .iter()
        .map(|p| Decimal::from((p.date - x0).num_days()).checked_div(year))
        .collect::<Option<_>>()?;
    let nd = Decimal::from(n as u64);
    let sum = |v: &[Decimal]| v.iter().try_fold(Decimal::ZERO, |a, b| a.checked_add(*b));
    let mx = sum(&xs)?.checked_div(nd)?;
    let my = sum(&vals)?.checked_div(nd)?;
    let sxx = xs
        .iter()
        .map(|x| (*x - mx).checked_mul(*x - mx))
        .collect::<Option<Vec<_>>>()?;
    let sxy = xs
        .iter()
        .zip(&vals)
        .map(|(x, y)| (*x - mx).checked_mul(*y - my))
        .collect::<Option<Vec<_>>>()?;
    let slope = sum(&sxy)?.checked_div(sum(&sxx)?)?;
    if slope.abs() < dec(rule.get("min_slope_per_year"))? {
        return None;
    }
    let last = tail[tail.len() - 1];
    let out = json!({
        "kind": "trend",
        "reason": if up { "TREND_UP" } else { "TREND_DOWN" },
        "rule": rule_id(rule),
        "severity": severity(rule, 1),
        "value": last.shown(),
        "unit": opt(last.unit.as_deref()),
        "n": n,
        "slope": num(&quantize(slope, 2)),
        "date": iso(last.date),
        "src": tail.iter().map(|p| p.src.clone()).collect::<Vec<_>>(),
    });
    out.as_object().cloned()
}

fn lab_range(rule: &Map<String, Value>, p: &Point) -> Result<Option<Map<String, Value>>> {
    let Some(flag) = p.flag.as_deref().filter(|f| FLAGGED.contains(f)) else {
        return Ok(None);
    };
    let missing = || PackError(format!("rule {}: no reason or severity for flag {flag}", rule_id(rule)));
    let reason = rule
        .get("reason")
        .and_then(|r| r.get(flag))
        .map(text)
        .ok_or_else(missing)?;
    let sev = int(rule.get("severity").and_then(|s| s.get(flag))).ok_or_else(missing)?;
    let out = json!({
        "kind": "lab_range",
        "reason": reason,
        "rule": rule_id(rule),
        "severity": sev,
        "value": p.shown(),
        "unit": opt(p.unit.as_deref()),
        "ref": opt(p.ref_text.as_deref()),
        "flag": flag,
        "loinc": opt(p.loinc.as_deref()),
        "date": iso(p.date),
        "src": p.src,
    });
    Ok(out.as_object().cloned())
}

fn matches_key(rule: &Map<String, Value>, key: &str) -> bool {
    rule.get("match")
        .and_then(Value::as_array)
        .is_some_and(|m| m.iter().any(|v| v.as_str() == Some(key)))
}

fn rules_for<'a>(pack: &'a Pack, kind: &'a str, key: &'a str) -> impl Iterator<Item = &'a Map<String, Value>> + 'a {
    pack.rules_of(kind).filter(move |r| matches_key(r, key))
}

/// Python's `str.casefold()` for the scripts our names use.
pub fn casefold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ſ' => out.push('s'),
            'ς' => out.push('σ'),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

fn phen(s: Option<&str>) -> String {
    casefold(s.unwrap_or_default().trim())
}

/// `unicodedata.normalize("NFKD", s)` without the characters `unicodedata.combining` flags.
fn strip_accents(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    use unicode_normalization::char::canonical_combining_class;
    s.nfkd().filter(|c| canonical_combining_class(*c) == 0).collect()
}

fn norm_drug(s: &str) -> String {
    strip_accents(&casefold(s)).trim().to_string()
}

/// A medication name or ingredient to the CPIC drug name key of drugs.yaml (no DB needed).
pub fn drug_key(pack: &Pack, name: Option<&str>) -> Option<String> {
    let name = name.filter(|n| !n.is_empty())?;
    let n = norm_drug(name);
    for (key, aliases) in &pack.drugs {
        let alias_hit = aliases
            .as_array()
            .is_some_and(|a| a.iter().any(|x| norm_drug(&text(x)) == n));
        if n == norm_drug(key) || alias_hit {
            return Some(key.clone());
        }
    }
    // first word of "Atorvastatin 20 mg", "Omeprazol cinfa"
    let head = n.split_whitespace().next().unwrap_or(&n).to_string();
    if head != n {
        return drug_key(pack, Some(&head));
    }
    None
}

pub fn med_drug(pack: &Pack, med: &Map<String, Value>) -> Option<String> {
    drug_key(pack, str_of(med, "ingredient")).or_else(|| drug_key(pack, str_of(med, "name")))
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `re.search(rf"(?<!\w){re.escape(w)}(?!\w)", n)`.
fn whole_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return true;
    }
    haystack.match_indices(word).any(|(i, _)| {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(text).collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![text(other)],
    }
}

// ----------------------------------------------------------------- evaluate ----

/// Evaluate the pack for one profile. `only_group` evaluates one lab series (status words,
/// check_drug's labs) and skips everything else.
pub async fn evaluate(
    conn: &mut PgConnection,
    profile_id: Uuid,
    as_of: NaiveDate,
    pack: &Pack,
    known_at: Option<DateTime<Utc>>,
    only_group: Option<&str>,
) -> Result<Evaluation> {
    let profile = get_profile(&mut *conn, profile_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("profile {profile_id} not found")))?;
    let majority = age_of_majority(&mut *conn, profile.household_id, 18).await?;
    let minor = profile.is_minor(as_of, majority);
    let age = profile.age_on(as_of);
    let (series, sensitive) = load_points(&mut *conn, profile_id, as_of, known_at, only_group, pack).await?;

    let mut flags: Vec<Map<String, Value>> = Vec::new();
    let mut stale: Vec<Map<String, Value>> = Vec::new();
    let mut markers: Vec<Marker> = Vec::new();
    let range_rule = pack
        .rules_of("lab_range")
        .next()
        .ok_or_else(|| PackError("the pack has no lab_range rule".into()))?;
    let mut in_scope: Vec<System> = pack
        .systems
        .iter()
        .filter(|s| only_group.is_none_or(|g| s.source == "labs" && s.markers.iter().any(|m| m.as_str() == Some(g))))
        .cloned()
        .collect();
    if let Some(g) = only_group
        && in_scope.is_empty()
    {
        in_scope.push(System {
            key: "other".into(),
            file: String::new(),
            order: 99,
            names: Map::new(),
            source: "labs".into(),
            markers: vec![Value::String(g.into())],
        });
    }
    let metrics: Vec<String> = in_scope
        .iter()
        .filter(|s| s.source == "wearables")
        .flat_map(|s| s.markers.iter().map(System::marker_key))
        .collect();
    let wdays = if only_group.is_none() {
        wearables(&mut *conn, profile_id, as_of, &metrics).await?
    } else {
        HashMap::new()
    };

    let put = |markers: &mut Vec<Marker>, m: Marker| match markers.iter().position(|x| x.key == m.key) {
        Some(i) => markers[i] = m,
        None => markers.push(m),
    };
    for system in &in_scope {
        for spec in &system.markers {
            let key = System::marker_key(spec);
            let mut m = Marker {
                key: key.clone(),
                system: system.key.clone(),
                source: system.source.clone(),
                status: "nodata",
                stale: false,
                rule: None,
                window_days: None,
                recency_rule: None,
                points: Vec::new(),
                wearable: None,
            };
            if let Some(rec) = rules_for(pack, "recency", &key).next() {
                m.window_days = int(rec.get("window_days"));
                m.recency_rule = Some(rule_id(rec));
            }
            if system.source == "wearables" {
                m.wearable = wearable_marker(spec, wdays.get(&key).map_or(&[][..], Vec::as_slice), as_of);
                if let Some(w) = &m.wearable {
                    m.status = "ok";
                    if m.window_days.is_some_and(|win| (as_of - w.newest).num_days() > win) {
                        m.status = "stale";
                        m.stale = true;
                        m.rule = m.recency_rule.clone();
                    }
                }
                put(&mut markers, m);
                continue;
            }
            m.points = series.get(&key).cloned().unwrap_or_default();
            let Some(p) = m.points.last().cloned() else {
                put(&mut markers, m);
                continue;
            };
            m.status = "ok";
            if let Some(win) = m.window_days
                && (as_of - p.date).num_days() > win
            {
                m.stale = true;
                let mut s = Map::new();
                s.insert("group".into(), json!(key));
                s.insert("last".into(), json!(iso(p.date)));
                s.insert("window_days".into(), json!(win));
                s.insert("rule".into(), opt(m.recency_rule.as_deref()));
                s.insert("src".into(), json!(p.src));
                stale.push(s);
            }
            if let Some(mut found) = lab_range(range_rule, &p)? {
                m.status = "out";
                m.rule = found.get("rule").map(text);
                found.insert("group".into(), json!(key));
                flags.push(found);
            } else {
                for rule in rules_for(pack, "rcv", &key) {
                    if let Some(mut f) = rcv(rule, &m.points) {
                        m.status = "watch";
                        m.rule = f.get("rule").map(text);
                        f.insert("group".into(), json!(key));
                        flags.push(f);
                        break;
                    }
                }
                if m.status != "watch" {
                    for rule in rules_for(pack, "trend", &key) {
                        if let Some(mut f) = trend(rule, &m.points) {
                            m.status = "watch";
                            m.rule = f.get("rule").map(text);
                            f.insert("group".into(), json!(key));
                            flags.push(f);
                            break;
                        }
                    }
                }
            }
            if m.status == "ok" && m.stale {
                m.status = "stale";
                m.rule = m.recency_rule.clone();
            }
            put(&mut markers, m);
        }
    }

    let mut ev = Evaluation {
        profile_id,
        slug: profile.slug.clone(),
        as_of,
        profile: profile.clone(),
        minor,
        markers,
        flags,
        stale,
        withheld_sensitive: sensitive,
        series,
        systems: Vec::new(),
        never_measured: Vec::new(),
        medications: Vec::new(),
        supplements: Vec::new(),
        conditions: Vec::new(),
        notes: Vec::new(),
        pending: Vec::new(),
        corrections: Vec::new(),
        freshness: Map::new(),
        findings: Vec::new(),
        genome_withheld_sensitive: 0,
        genome_withheld_minor: 0,
        genome_assay: None,
        derivation: String::new(),
    };
    if only_group.is_some() {
        return Ok(ev);
    }

    ev.systems = pack.systems.iter().map(|s| system_state(s, &ev.markers)).collect();

    let meds_all: Vec<Map<String, Value>> =
        active_medications(&mut *conn, profile_id, Some(as_of), known_at, None, false)
            .await?
            .into_iter()
            .filter(|m| date_of(m, "started").is_none_or(|s| s <= as_of))
            .collect();
    ev.medications = meds_all
        .iter()
        .filter(|m| str_of(m, "kind") == Some("medication"))
        .cloned()
        .collect();
    ev.supplements = meds_all
        .iter()
        .filter(|m| str_of(m, "kind") == Some("supplement"))
        .cloned()
        .collect();

    let findings_obj = genome::list_findings(&mut *conn, profile_id, minor, as_of).await?;
    ev.findings = findings_obj
        .get("findings")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Finding::from_value).collect())
        .unwrap_or_default();
    ev.genome_withheld_sensitive = findings_obj
        .get("withheld_sensitive")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    ev.genome_withheld_minor = findings_obj.get("withheld_minor").and_then(Value::as_i64).unwrap_or(0);
    ev.genome_assay = findings_obj.get("assay").and_then(Value::as_object).cloned();

    let mut never: Vec<Map<String, Value>> = Vec::new();
    let never_add = |never: &mut Vec<Map<String, Value>>, group: &str, rule: &str| {
        let seen = never
            .iter()
            .any(|n| n.get("group").and_then(Value::as_str) == Some(group));
        if !seen && ev.series.get(group).is_none_or(Vec::is_empty) {
            let mut n = Map::new();
            n.insert("group".into(), json!(group));
            n.insert("rule".into(), json!(rule));
            never.push(n);
        }
    };

    for rule in pack.rules_of("never_measured") {
        let empty = Map::new();
        let when = rule.get("when").and_then(Value::as_object).unwrap_or(&empty);
        if when.contains_key("min_age") && age.is_none_or(|a| i64::from(a) < int(when.get("min_age")).unwrap_or(0)) {
            continue;
        }
        if when.contains_key("max_age") && age.is_none_or(|a| i64::from(a) > int(when.get("max_age")).unwrap_or(0)) {
            continue;
        }
        if let Some(sex) = when.get("sex")
            && profile.sex_at_birth.as_deref() != sex.as_str()
        {
            continue;
        }
        if when.contains_key("taking") {
            let words: Vec<String> = str_list(when.get("taking")).iter().map(|w| casefold(w)).collect();
            let names: Vec<String> = meds_all
                .iter()
                .map(|m| {
                    casefold(&format!(
                        "{} {}",
                        str_of(m, "name").unwrap_or_default(),
                        str_of(m, "ingredient").unwrap_or_default()
                    ))
                })
                .collect();
            // whole words: "iron" must not fire on "Environ"
            if !words.iter().any(|w| names.iter().any(|n| whole_word(n, w))) {
                continue;
            }
        }
        let id = rule_id(rule);
        for group in str_list(rule.get("match")) {
            never_add(&mut never, &group, &id);
        }
    }

    let mut med_by_drug: Vec<(String, &Map<String, Value>)> = Vec::new();
    for med in &ev.medications {
        if let Some(k) = med_drug(pack, med)
            && !med_by_drug.iter().any(|(x, _)| *x == k)
        {
            med_by_drug.push((k, med));
        }
    }
    let med_for = |drug: &str| med_by_drug.iter().find(|(k, _)| k == drug).map(|(_, m)| *m);

    for rule in pack.rules_of("gene_drug") {
        let wanted: BTreeSet<String> = str_list(rule.get("phenotypes")).iter().map(|p| phen(Some(p))).collect();
        let gene = rule.get("gene").map(text);
        for f in &ev.findings {
            if f.gene != gene || !wanted.contains(&phen(f.phenotype.as_deref())) {
                continue;
            }
            for drug in str_list(rule.get("drugs")) {
                if let Some(med) = med_for(&drug) {
                    let mut flag = Map::new();
                    flag.insert("kind".into(), json!("gene_drug"));
                    flag.insert("reason".into(), json!("GENE_DRUG"));
                    flag.insert("rule".into(), json!(rule_id(rule)));
                    flag.insert("severity".into(), json!(severity(rule, 2)));
                    flag.insert("gene".into(), opt(f.gene.as_deref()));
                    flag.insert("phenotype".into(), opt(f.phenotype.as_deref()));
                    flag.insert("drug".into(), json!(drug));
                    flag.insert(
                        "date".into(),
                        date_of(med, "started").map_or(Value::Null, |d| json!(iso(d))),
                    );
                    flag.insert(
                        "src".into(),
                        json!([opt(f.src.as_deref()), med.get("src_id").cloned().unwrap_or(Value::Null)]),
                    );
                    ev.flags.push(flag);
                }
            }
        }
    }

    for rule in pack.rules_of("gene_lab") {
        let genes = str_list(rule.get("gene"));
        let cats = str_list(rule.get("categories"));
        let calls = str_list(rule.get("call_states"));
        let phens: BTreeSet<String> = str_list(rule.get("phenotypes")).iter().map(|p| phen(Some(p))).collect();
        let hit = ev.findings.iter().any(|f| {
            f.gene.as_ref().is_some_and(|g| genes.contains(g))
                && (cats.is_empty() || f.category.as_ref().is_some_and(|c| cats.contains(c)))
                && (calls.is_empty() || f.call_state.as_ref().is_some_and(|c| calls.contains(c)))
                && (phens.is_empty() || phens.contains(&phen(f.phenotype.as_deref())))
        });
        let drugs = str_list(rule.get("drugs"));
        if !hit || (!drugs.is_empty() && !drugs.iter().any(|d| med_for(d).is_some())) {
            continue;
        }
        let id = rule_id(rule);
        let window = rule.get("window_days").filter(|v| truthy(v)).and_then(|v| int(Some(v)));
        for lab in str_list(rule.get("labs")) {
            match ev.series.get(&lab).and_then(|p| p.last()) {
                None => never_add(&mut never, &lab, &id),
                Some(last) => {
                    let already = ev
                        .stale
                        .iter()
                        .any(|s| s.get("group").and_then(Value::as_str) == Some(&lab));
                    if let Some(win) = window
                        && (as_of - last.date).num_days() > win
                        && !already
                    {
                        let mut s = Map::new();
                        s.insert("group".into(), json!(lab));
                        s.insert("last".into(), json!(iso(last.date)));
                        s.insert("window_days".into(), json!(win));
                        s.insert("rule".into(), json!(id));
                        s.insert("src".into(), json!(last.src));
                        ev.stale.push(s);
                    }
                }
            }
        }
    }
    ev.never_measured = never;

    let order: HashMap<&str, usize> = pack.order.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
    let sort_key = |f: &Map<String, Value>| {
        let sev = f.get("severity").and_then(Value::as_i64).unwrap_or(0);
        let kind = f.get("kind").and_then(Value::as_str).unwrap_or_default();
        let day = f
            .get("date")
            .and_then(Value::as_str)
            .and_then(parse_date)
            .map_or(0, |d| d.num_days_from_ce());
        let who = f
            .get("group")
            .filter(|v| truthy(v))
            .or_else(|| f.get("gene"))
            .filter(|v| truthy(v))
            .map(text)
            .unwrap_or_default();
        (
            -sev,
            order.get(kind).copied().unwrap_or(99),
            -i64::from(day),
            who,
            f.get("rule").map(text).unwrap_or_default(),
        )
    };
    ev.flags.sort_by_cached_key(sort_key);
    ev.stale.sort_by_cached_key(|s| {
        (
            s.get("last").map(text).unwrap_or_default(),
            s.get("group").map(text).unwrap_or_default(),
        )
    });

    let mut conditions: Vec<Map<String, Value>> = list_records(&mut *conn, "condition", profile_id, known_at, false)
        .await?
        .into_iter()
        .filter(|c| {
            ["active", "recurrence", "relapse"].contains(&str_of(c, "clinical_status").unwrap_or_default())
                && !["refuted", "entered-in-error"].contains(&str_of(c, "verification_status").unwrap_or_default())
                && date_of(c, "onset").is_none_or(|o| o <= as_of)
        })
        .collect();
    conditions.sort_by_cached_key(|c| {
        (
            date_of(c, "onset").unwrap_or(NaiveDate::MIN),
            str_of(c, "text").unwrap_or_default().to_string(),
        )
    });
    ev.conditions = conditions;

    let cut = day_end(as_of);
    let sql = format!(
        "select n.author_kind, n.body, coalesce(n.effective_at, n.recorded_at) as at, n.src_id from note n \
         where n.profile_id = $1 and n.kind = 'priorities' and {} \
         and coalesce(n.effective_at, n.recorded_at) < $3 and n.sensitivity = '{{}}' \
         order by coalesce(n.effective_at, n.recorded_at) desc, n.id limit 3",
        visible_sql("n", "$2")
    );
    let notes: Vec<(String, String, DateTime<Utc>, Option<String>)> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(known_at)
        .bind(cut)
        .fetch_all(&mut *conn)
        .await?;
    ev.notes = notes
        .into_iter()
        .map(|(author, body, at, src)| {
            json!({"author_kind": author, "date": iso(at.date_naive()), "text": body, "src": src})
        })
        .collect();

    let know = if known_at.is_some() {
        "and o.recorded_at <= $3"
    } else {
        "and cast($3 as timestamptz) is null"
    };
    let sql = format!(
        "select o.alias_group, o.effective_at, o.src_id from observation o where o.profile_id = $1 \
         and o.status = 'provisional' and o.effective_at < $2 and o.sensitivity = '{{}}' {know} \
         order by o.effective_at desc, o.id limit 3"
    );
    let pending: Vec<(String, DateTime<Utc>, String)> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(cut)
        .bind(known_at)
        .fetch_all(&mut *conn)
        .await?;
    ev.pending = pending
        .into_iter()
        .map(|(g, at, src)| json!({"group": g, "date": iso(at.date_naive()), "src": src}))
        .collect();

    let corr_know = if known_at.is_some() {
        "and o.superseded_at <= $4"
    } else {
        "and cast($4 as timestamptz) is null"
    };
    let sql = format!(
        "select o.alias_group, o.printed_value, o.printed_unit, o.effective_at, o.status, o.status_reason, \
         o.superseded_at, o.src_id from observation o where o.profile_id = $1 \
         and o.status in ('superseded', 'retracted') and o.superseded_at < $2 \
         and o.superseded_at >= $3 and o.sensitivity = '{{}}' {corr_know} \
         and coalesce(o.status_reason, '') not like 'rederive norm-%' \
         order by o.superseded_at desc, o.id limit 3"
    );
    type Corr = (
        String,
        String,
        Option<String>,
        DateTime<Utc>,
        String,
        Option<String>,
        DateTime<Utc>,
        String,
    );
    let corrections: Vec<Corr> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(cut)
        .bind(cut - Duration::days(90))
        .bind(known_at)
        .fetch_all(&mut *conn)
        .await?;
    ev.corrections = corrections
        .into_iter()
        .map(|(g, value, unit, at, status, reason, on, src)| {
            json!({
                "group": g, "value": value, "unit": unit, "date": iso(at.date_naive()), "status": status,
                "reason": reason, "on": iso(on.date_naive()), "src": src,
            })
        })
        .collect();

    let lab_dates = ev.series.values().filter_map(|p| p.last().map(|p| p.date)).max();
    let wear_dates = ev
        .markers
        .iter()
        .filter_map(|m| m.wearable.as_ref().map(|w| w.newest))
        .max();
    let know = if known_at.is_some() {
        " and imported_at <= $3"
    } else {
        " and cast($3 as timestamptz) is null"
    };
    let sql = format!(
        "select max(imported_at) from genome_dataset where profile_id = $1 and status = 'current' \
         and imported_at < $2{know}"
    );
    let genome_date: Option<DateTime<Utc>> = sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(profile_id)
        .bind(cut)
        .bind(known_at)
        .fetch_one(&mut *conn)
        .await?;
    let iso_or_null = |d: Option<NaiveDate>| d.map_or(Value::Null, |d| json!(iso(d)));
    ev.freshness.insert("labs".into(), iso_or_null(lab_dates));
    ev.freshness.insert("wearables".into(), iso_or_null(wear_dates));
    ev.freshness
        .insert("genome".into(), iso_or_null(genome_date.map(|d| d.date_naive())));
    let derivations: BTreeSet<&str> = ev.series.values().flatten().map(|p| p.derivation.as_str()).collect();
    ev.derivation = derivations.into_iter().collect::<Vec<_>>().join(",");
    Ok(ev)
}

fn system_state(system: &System, markers: &[Marker]) -> SystemState {
    let idx: Vec<usize> = system
        .markers
        .iter()
        .filter_map(|spec| {
            let key = System::marker_key(spec);
            markers.iter().position(|m| m.key == key)
        })
        .collect();
    let ms: Vec<&Marker> = idx.iter().map(|&i| &markers[i]).collect();
    let with_data: Vec<(usize, &Marker)> = ms
        .iter()
        .enumerate()
        .filter(|(_, m)| m.status != "nodata")
        .map(|(i, m)| (i, *m))
        .collect();
    let newest = with_data.iter().filter_map(|(_, m)| m.newest()).max();
    let state = |status: &'static str, rule: String| SystemState {
        key: system.key.clone(),
        file: system.file.clone(),
        status,
        rule,
        newest,
        markers: idx.clone(),
    };
    if with_data.is_empty() {
        return SystemState {
            newest: None,
            ..state("nodata", system.file.clone())
        };
    }
    let worst = with_data
        .iter()
        .filter(|(_, m)| matches!(m.status, "out" | "watch"))
        .max_by_key(|(i, m)| (status_rank(m.status), std::cmp::Reverse(*i)));
    if let Some((_, m)) = worst {
        return state(m.status, m.rule.clone().unwrap_or_default());
    }
    if with_data.iter().any(|(_, m)| m.status == "ok") {
        return state("ok", system.file.clone());
    }
    let rule = with_data[0]
        .1
        .rule
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| system.file.clone());
    state("stale", rule)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_words_only() {
        assert!(whole_word("hierro (sulfato ferroso) ", "hierro"));
        assert!(!whole_word("environ skin cream ", "iron"));
        assert!(whole_word("iron", "iron"));
    }

    #[test]
    fn accents_and_case_fold_away() {
        assert_eq!(norm_drug(" Codeína "), "codeina");
        assert_eq!(norm_drug("BUPROPIÓN"), "bupropion");
    }
}
