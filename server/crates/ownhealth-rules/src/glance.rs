//! The "Health at a glance" page (`state/glance.py`): one card per body system, the medication strip
//! with PGx badges, changes since the last draw, gaps, genome highlights and the state hash. Display
//! data derived from the same evaluation as the hashed body; status words come only from the engine.
//! Shape: `Dashboard` in ui/src/lib/api/types.ts.

use std::collections::{BTreeSet, HashMap};

use chrono::{SubsecRound, Utc};
use ownhealth_core::Result;
use ownhealth_core::access::Scopes;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::engine::{Evaluation, Marker, casefold, iso, med_drug, parse_date, str_of};
use crate::num::{self, num, to_f64};
use crate::pack::{P, Pack, cpic_snapshot, int, text, truthy};
use crate::rendering::window_months;

const COUNTRIES: [(&str, &str, &str); 7] = [
    ("ES", "Spain", "España"),
    ("PT", "Portugal", "Portugal"),
    ("FR", "France", "Francia"),
    ("DE", "Germany", "Alemania"),
    ("IT", "Italy", "Italia"),
    ("GB", "United Kingdom", "Reino Unido"),
    ("US", "United States", "Estados Unidos"),
];
const NORMAL_PHENOTYPES: [&str; 4] = ["normal metabolizer", "normal function", "indeterminate", "no result"];

fn f(v: Option<Decimal>) -> Value {
    v.map_or(Value::Null, |d| json!(to_f64(&d)))
}

fn short(phenotype: Option<&str>) -> String {
    phenotype
        .unwrap_or_default()
        .split_whitespace()
        .filter(|w| !["likely", "possible"].contains(&w.to_lowercase().as_str()))
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

pub fn actionable_phenotype(phenotype: Option<&str>) -> bool {
    let p = casefold(phenotype.unwrap_or_default().trim());
    !p.is_empty() && !NORMAL_PHENOTYPES.contains(&p.as_str()) && !p.ends_with("negative")
}

fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn value_of(m: &Marker, spec: &Map<String, Value>) -> (String, String) {
    if m.source == "wearables" {
        let unit = spec.get("unit").map(text).unwrap_or_default();
        let Some(v) = m.wearable.as_ref().and_then(|w| w.value) else {
            return (String::new(), unit);
        };
        let whole = v.trunc().to_i64().unwrap_or(0);
        if spec.get("format").and_then(Value::as_str) == Some("hm") {
            return (format!("{}:{:02}", whole.div_euclid(60), whole.rem_euclid(60)), unit);
        }
        if spec.get("agg").and_then(Value::as_str) == Some("sum30") {
            return (grouped(whole), unit);
        }
        return (num(&v), unit);
    }
    match m.latest() {
        Some(p) => (p.shown(), p.unit.clone().unwrap_or_default()),
        None => (String::new(), String::new()),
    }
}

/// The reference band in the spark's (canonical) unit; the range is printed in the report's unit.
fn band(m: &Marker) -> Value {
    let Some(p) = m.latest() else { return Value::Null };
    if p.ref_low.is_none() && p.ref_high.is_none() {
        return Value::Null;
    }
    let (mut low, mut high) = (p.ref_low, p.ref_high);
    if let (Some(_), Some(from), Some(to)) = (p.canonical, p.unit_ucum.as_deref(), p.canonical_unit.as_deref())
        && !from.is_empty()
        && !to.is_empty()
        && from != to
    {
        let conv = |b: Option<Decimal>| -> std::result::Result<Option<Decimal>, ()> {
            match b {
                None => Ok(None),
                Some(b) => match ownhealth_normalize::convert(b, from, to, &m.key) {
                    Ok(Some(c)) => Ok(Some(c)),
                    _ => Err(()),
                },
            }
        };
        match (conv(low), conv(high)) {
            (Ok(l), Ok(h)) => (low, high) = (l, h),
            _ => return Value::Null,
        }
    }
    let low = low.unwrap_or(Decimal::ZERO);
    let high = match high {
        Some(h) => h,
        None => {
            let base = low.checked_mul(num::parse("1.33").unwrap_or_default()).unwrap_or(low);
            m.points.iter().filter_map(|x| x.comparable()).fold(base, Decimal::max)
        }
    };
    json!([to_f64(&low), to_f64(&high)])
}

fn marker_row(
    m: &Marker,
    spec: &Map<String, Value>,
    pack: &Pack,
    lang: &str,
    sources: &HashMap<String, String>,
) -> Value {
    let (value, unit) = value_of(m, spec);
    if m.source == "wearables" {
        let Some(w) = &m.wearable else { return Value::Null };
        let name = spec
            .get(lang)
            .filter(|v| truthy(v))
            .or_else(|| spec.get("en").filter(|v| truthy(v)))
            .map_or_else(|| m.key.clone(), text);
        let newest = iso(w.newest);
        let label = [
            sources.get(&m.key).map(String::as_str),
            Some(newest.as_str()),
            Some("30 d"),
        ]
        .into_iter()
        .flatten()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        return json!({
            "alias_group": m.key,
            "name": name,
            "value": value,
            "unit": unit,
            "status": m.status,
            "stale": m.stale,
            "date": newest,
            "src": null,
            "src_label": label,
            "spark": w.spark.iter().map(|x| f(*x)).collect::<Vec<_>>(),
            "band": null,
            "rule": m.rule,
        });
    }
    let Some(p) = m.latest() else { return Value::Null };
    let spark_n = usize::try_from(int(pack.dashboard.get("spark_points")).unwrap_or(9)).unwrap_or(9);
    let points: Vec<_> = m.points.iter().filter(|x| x.comparable().is_some()).collect();
    let points = &points[points.len().saturating_sub(spark_n)..];
    json!({
        "alias_group": m.key,
        "name": pack.name(&m.key, lang),
        "value": value,
        "unit": unit,
        "status": m.status,
        "stale": m.stale,
        "date": iso(p.date),
        "src": p.src,
        "src_label": p.src_label,
        "spark": points.iter().map(|x| f(x.comparable())).collect::<Vec<_>>(),
        "band": band(m),
        "rule": m.rule,
    })
}

async fn wearable_sources(conn: &mut PgConnection, profile_id: Uuid) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "select distinct on (r.metric) r.metric, coalesce(s.device, s.provider) from wearable_rollup r \
         join wearable_source s on s.id = r.source_id where r.profile_id = $1 order by r.metric, r.t desc",
    )
    .bind(profile_id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().filter_map(|(m, s)| s.map(|s| (m, s))).collect())
}

pub async fn build_dashboard(
    conn: &mut PgConnection,
    ev: &Evaluation,
    body: &Value,
    pack: &Pack,
    lang: &str,
    scopes: &Scopes,
) -> Result<Value> {
    let lang = if lang == "es" { "es" } else { "en" };
    let as_of = ev.as_of;
    let max_markers = usize::try_from(int(pack.dashboard.get("max_markers_per_card")).unwrap_or(3)).unwrap_or(3);
    let labs_ok = scopes.contains("read:labs");
    let wear_ok = scopes.contains("read:wearables");
    let sources = if wear_ok {
        wearable_sources(&mut *conn, ev.profile_id).await?
    } else {
        HashMap::new()
    };
    let empty = Map::new();

    let mut systems = Vec::new();
    for (s, def) in ev.systems.iter().zip(&pack.systems) {
        let allowed = if def.source == "labs" { labs_ok } else { wear_ok };
        let mut rows = Vec::new();
        if allowed {
            for (spec, &mi) in def.markers.iter().zip(&s.markers) {
                let m = &ev.markers[mi];
                if m.status != "nodata" && rows.len() < max_markers {
                    rows.push(marker_row(m, spec.as_object().unwrap_or(&empty), pack, lang, &sources));
                }
            }
        }
        let name = def
            .names
            .get(lang)
            .filter(|v| truthy(v))
            .or_else(|| def.names.get("en").filter(|v| truthy(v)))
            .map_or_else(|| s.key.clone(), text);
        let mut card = Map::new();
        card.insert("key".into(), json!(s.key));
        card.insert("name".into(), json!(name));
        card.insert("status".into(), json!(if allowed { s.status } else { "nodata" }));
        card.insert("markers".into(), Value::Array(rows));
        card.insert(
            "newest".into(),
            match s.newest {
                Some(d) if allowed => json!(iso(d)),
                _ => Value::Null,
            },
        );
        card.insert("rule".into(), json!(if allowed { &s.rule } else { &def.file }));
        card.insert("rule_file".into(), json!(def.file));
        card.insert("withheld".into(), json!(!allowed));
        if !allowed {
            card.insert(
                "needs".into(),
                json!(if def.source == "labs" {
                    "read:labs"
                } else {
                    "read:wearables"
                }),
            );
        }
        systems.push(Value::Object(card));
    }

    // changes since the previous draw, among the lab markers on the page
    let lab_markers: Vec<&Marker> = ev
        .markers
        .iter()
        .filter(|m| m.source == "labs" && !m.points.is_empty())
        .collect();
    let draws: BTreeSet<_> = lab_markers
        .iter()
        .flat_map(|m| m.points.iter().map(|p| p.date))
        .collect();
    let mut draws: Vec<_> = draws.into_iter().collect();
    draws.reverse();
    let latest_draw = draws.first().copied();
    let prev_draw = draws.get(1).copied();
    let mut changes = Vec::new();
    let mut not_this_time = Vec::new();
    if labs_ok && let Some(latest_draw) = latest_draw {
        let min_pct = pack
            .dashboard
            .get("changes_min_pct")
            .and_then(|v| num::parse(&text(v)))
            .unwrap_or(Decimal::from(15));
        for m in &lab_markers {
            let Some(p) = m.latest() else { continue };
            if p.date != latest_draw {
                if m.stale {
                    not_this_time.push(json!(pack.name(&m.key, lang)));
                }
                continue;
            }
            let prev: Vec<_> = m.points[..m.points.len() - 1]
                .iter()
                .filter(|x| x.comparable().is_some() && x.comparable_unit() == p.comparable_unit())
                .collect();
            let (Some(last_prev), Some(b)) = (prev.last(), p.comparable()) else {
                continue;
            };
            let Some(a) = last_prev.comparable().filter(|a| !a.is_zero()) else {
                continue;
            };
            let pct = (b - a).checked_div(a).and_then(|x| x.checked_mul(Decimal::from(100)));
            let Some(pct) = pct else { continue };
            if matches!(m.status, "out" | "watch") || pct.abs() >= min_pct {
                changes.push(json!({
                    "name": pack.name(&m.key, lang),
                    "from": last_prev.shown(),
                    "to": p.shown(),
                    "unit": p.unit.clone().unwrap_or_default(),
                    "direction": if b > a { "up" } else { "down" },
                    "status": m.status,
                    "src": p.src,
                }));
            }
        }
    }

    // gaps: stale series (grouped by the rule that set them) and never-measured analytes
    let mut due = Vec::new();
    if labs_ok {
        let mut by_rule: Vec<(String, Vec<&Map<String, Value>>)> = Vec::new();
        for st in body
            .get("stale")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let rule = st.get("rule").map(text).unwrap_or_default();
            match by_rule.iter_mut().find(|(r, _)| *r == rule) {
                Some((_, items)) => items.push(st),
                None => by_rule.push((rule, vec![st])),
            }
        }
        for (rule, items) in by_rule {
            let names = items
                .iter()
                .map(|i| pack.name(str_of(i, "group").unwrap_or_default(), lang))
                .collect::<Vec<_>>()
                .join(" + ");
            due.push(json!({
                "title": pack.text("due.retest", lang, &[("name", P::from(names))])?,
                "why": pack.text("due.stale", lang, &[
                    ("rule", P::from(&rule)),
                    ("window_months", P::I(window_months(pack, &rule))),
                ])?,
                "status": "stale",
                "kind": "lab",
                "phase": null,
                "rule": rule,
            }));
        }
        for n in body
            .get("never_measured")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let rule = n.get("rule").map(text).unwrap_or_default();
            due.push(json!({
                "title": pack.name(str_of(n, "group").unwrap_or_default(), lang),
                "why": pack.text("due.never", lang, &[("rule", P::from(&rule))])?,
                "status": "watch",
                "kind": "lab",
                "phase": null,
                "rule": rule,
            }));
        }
    }

    // medication strip with PGx badges (CPIC level A pairs with a non-normal phenotype)
    let genome_ok = scopes.contains("read:genome");
    let findings = if genome_ok { &ev.findings[..] } else { &[] };
    let snap = cpic_snapshot()?;
    let pairs: Vec<&Map<String, Value>> = snap
        .get("pairs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .collect();
    let mut medications = Vec::new();
    if scopes.contains("read:records") {
        for med in &ev.medications {
            let key = med_drug(pack, med);
            let mut badge = Value::Null;
            for pair in pairs
                .iter()
                .filter(|p| key.is_some() && str_of(p, "drugname") == key.as_deref())
            {
                let gene = str_of(pair, "genesymbol");
                let hit = findings.iter().find(|f| f.gene.as_deref() == gene);
                if let Some(f) = hit
                    && actionable_phenotype(f.phenotype.as_deref())
                {
                    let level = pair.get("cpiclevel").map(text).unwrap_or_default();
                    badge = json!({
                        "gene": f.gene,
                        "label": format!("{} {}", f.gene.as_deref().unwrap_or("None"), short(f.phenotype.as_deref())),
                        "phenotype": f.phenotype,
                        "level": pair.get("cpiclevel"),
                        "text": pack.text("pgx.badge", lang, &[
                            ("gene", P::from(f.gene.as_deref())),
                            ("phenotype", P::from(f.phenotype.as_deref())),
                            ("level", P::from(level)),
                        ])?,
                        "src": f.src,
                    });
                    break;
                }
            }
            medications.push(json!({
                "name": med.get("name"),
                "strength": med.get("strength"),
                "since": med.get("started"),
                "src": med.get("src_id"),
                "pgx": badge,
            }));
        }
    }

    let mut genome = Value::Null;
    let hidden = ev.genome_withheld_sensitive + ev.genome_withheld_minor;
    let any_withheld = ev.genome_withheld_sensitive != 0 || ev.genome_withheld_minor != 0;
    if genome_ok && (!findings.is_empty() || any_withheld) {
        let pgx: Vec<_> = findings
            .iter()
            .filter(|f| f.category.as_deref() == Some("pgx") && actionable_phenotype(f.phenotype.as_deref()))
            .collect();
        let carriers = findings
            .iter()
            .filter(|f| f.category.as_deref() == Some("carrier"))
            .count();
        let join = |parts: &[Option<&str>]| {
            parts
                .iter()
                .flatten()
                .filter(|x| !x.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut chips: Vec<String> = pgx
            .iter()
            .map(|f| {
                let s = short(f.phenotype.as_deref());
                join(&[f.gene.as_deref(), f.diplotype.as_deref(), Some(&s)])
            })
            .collect();
        chips.extend(
            findings
                .iter()
                .filter(|f| f.category.as_deref() == Some("risk_marker") && f.call_state.as_deref() == Some("alt"))
                .map(|f| join(&[f.gene.as_deref(), f.rsid.as_deref()])),
        );
        let summary = [
            ("pgx", pgx.len() as i64),
            ("carrier", carriers as i64),
            ("hidden", hidden),
        ]
        .iter()
        .map(|(k, n)| pack.text(&format!("genome.summary.{k}"), lang, &[("n", P::I(*n))]))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join(" · ");
        let assay = ev.genome_assay.clone().unwrap_or_default();
        let source = assay
            .get("source_tool")
            .filter(|v| truthy(v))
            .or_else(|| assay.get("pipeline"))
            .cloned()
            .unwrap_or(Value::Null);
        genome = json!({"summary": summary, "chips": chips, "hidden": hidden, "source": source});
    }

    let review_open: i64 =
        sqlx::query_scalar("select count(*) from review_item where profile_id = $1 and status = 'open'")
            .bind(ev.profile_id)
            .fetch_one(&mut *conn)
            .await?;

    let prof = &ev.profile;
    let mut parts: Vec<String> = Vec::new();
    if let Some(sex) = prof.sex_at_birth.as_deref().filter(|s| ["female", "male"].contains(s)) {
        parts.push(pack.text(&format!("sex.{sex}"), lang, &[])?);
    }
    if let Some(age) = prof.age_on(as_of) {
        parts.push(age.to_string());
    }
    if let Some(country) = prof.country.as_deref().filter(|c| !c.is_empty()) {
        parts.push(match COUNTRIES.iter().find(|(code, _, _)| *code == country) {
            Some((_, en, es)) => (if lang == "es" { es } else { en }).to_string(),
            None => country.to_string(),
        });
    }
    let fresh = body.get("freshness").and_then(Value::as_object).unwrap_or(&empty);
    let days_since = |k: &str| {
        fresh
            .get(k)
            .and_then(Value::as_str)
            .and_then(parse_date)
            .map(|d| (as_of - d).num_days())
    };
    if labs_ok && let Some(n) = days_since("labs") {
        parts.push(pack.text("subtitle.lab", lang, &[("n", P::I(n))])?);
    }
    if wear_ok && let Some(n) = days_since("wearables") {
        parts.push(pack.text("subtitle.wearables", lang, &[("n", P::I(n))])?);
    }

    let changes_vs = match prev_draw {
        Some(d) if !changes.is_empty() => json!(iso(d)),
        _ => Value::Null,
    };
    let generated = Utc::now().trunc_subsecs(0).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    Ok(json!({
        "profile": ev.slug,
        "as_of": iso(as_of),
        "subtitle": parts.join(" · "),
        "systems": systems,
        "medications": medications,
        "changes": changes,
        "changes_vs": changes_vs,
        "not_measured_this_time": not_this_time,
        "due": due,
        "genome": genome,
        "review_open": review_open,
        "state_hash": body.get("state_hash"),
        "rules_version": body.get("rules"),
        "generated_at": generated,
    }))
}
