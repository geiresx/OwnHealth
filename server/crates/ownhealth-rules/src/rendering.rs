//! Render a state body for a reader (`state/rendering.py`): localized text, unit preference, identity
//! mode, scopes. Rendering never touches the body or its hash. The output is the `get_health_status`
//! data block of mcp.md 5.1 plus the envelope fields only lane E knows.

use std::collections::BTreeSet;

use ownhealth_core::access::Scopes;
use ownhealth_core::text::untrusted;
use ownhealth_core::{Error, Result};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};

use crate::engine::{parse_date, rcv_percent};
use crate::num;
use crate::pack::{P, Pack, PackError, int, text, truthy};

pub const LAB_KINDS: [&str; 3] = ["lab_range", "rcv", "trend"];

fn kind_of(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "LAB_HIGH" | "LAB_LOW" | "LAB_ABNORMAL" => "lab_range",
        "RCV_CHANGE" => "rcv",
        "TREND_UP" | "TREND_DOWN" => "trend",
        "GENE_DRUG" => "gene_drug",
        _ => return None,
    })
}

fn domain_scope(domain: &str) -> &'static str {
    match domain {
        "labs" => "read:labs",
        "records" => "read:records",
        "genome" => "read:genome",
        _ => "read:wearables",
    }
}

/// `round(window_days / 30.44)`, Python's banker's rounding of a float.
pub fn window_months(pack: &Pack, rule_id: &str) -> i64 {
    let days = int(pack.rule(rule_id).and_then(|r| r.get("window_days"))).unwrap_or(0);
    (days as f64 / 30.44).round_ties_even() as i64
}

/// The `canonical` block in the reader's unit preference, from the normalizer (lane A). None when no
/// conversion applies or the normalizer cannot convert (the printed value stays).
pub fn convert(group: Option<&str>, value: Option<&str>, unit: Option<&str>, units: &str) -> Option<Value> {
    if units == "as_reported" {
        return None;
    }
    let group = group.filter(|g| !g.is_empty())?;
    let value = value?;
    let unit = unit.filter(|u| !u.is_empty())?;
    let target = ownhealth_normalize::group_units(group)?
        .into_iter()
        .find(|(k, _)| k == units)
        .map(|(_, v)| v)
        .filter(|t| !t.is_empty())?;
    let source = ownhealth_normalize::to_ucum(unit).filter(|s| !s.is_empty())?;
    if target == source {
        return None;
    }
    let v: Decimal = num::parse(value)?;
    let out = ownhealth_normalize::convert(v, &source, &target, group)
        .ok()
        .flatten()?;
    Some(json!({"value": num::num(&out), "unit": target}))
}

fn s<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

fn list<'a>(body: &'a Value, key: &str) -> impl Iterator<Item = &'a Map<String, Value>> + 'a {
    body.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

fn has_items(body: &Value, key: &str) -> bool {
    body.get(key).is_some_and(truthy)
}

/// `render_state(body, lang=, units=, identity_mode=, scopes=)` with the pack given.
pub fn render_with(
    pack: &Pack,
    body: &Value,
    lang: &str,
    units: &str,
    identity_mode: &str,
    scopes: Option<&Scopes>,
) -> Result<Value> {
    let lang = if lang == "es" { "es" } else { "en" };
    let as_of = body
        .get("as_of")
        .and_then(Value::as_str)
        .and_then(parse_date)
        .ok_or_else(|| Error::Invalid("state body without as_of".into()))?;
    let mode = if ["full", "pseudonymous", "minimal"].contains(&identity_mode) {
        identity_mode
    } else {
        "pseudonymous"
    };
    let allowed = |domain: &str| scopes.is_none_or(|sc| sc.contains(domain_scope(domain)));
    let mut by_scope: BTreeSet<&str> = BTreeSet::new();
    let when = |d: Option<&str>| -> Result<Value> {
        let Some(d) = d else { return Ok(Value::Null) };
        if mode != "minimal" {
            return Ok(json!(d));
        }
        let day = parse_date(d).ok_or_else(|| Error::Invalid(format!("bad date {d}")))?;
        Ok(json!(pack.text(
            "days_ago",
            lang,
            &[("n", P::I((as_of - day).num_days()))]
        )?))
    };
    let when_s = |d: Option<&str>| -> Result<String> { Ok(when(d)?.as_str().map(str::to_string).unwrap_or_default()) };
    // "on 2026-09-14", or "425 days ago" in minimal mode
    let on = |d: Option<&str>| -> Result<String> {
        if mode != "minimal" {
            Ok(pack.text("on_date", lang, &[("date", P::from(d))])?)
        } else {
            when_s(d)
        }
    };

    let mut flags: Vec<Value> = Vec::new();
    for f in list(body, "flags") {
        let reason = s(f, "reason").unwrap_or_default();
        let rule_id = s(f, "rule").unwrap_or_default();
        // A pack override may map a lab flag to its own reason code: the rule's kind then says what it is.
        let kind = kind_of(reason)
            .map(str::to_string)
            .or_else(|| pack.rule(rule_id).and_then(|r| r.get("kind")).map(text));
        let kind = match kind {
            Some(k) if LAB_KINDS.contains(&k.as_str()) || k == "gene_drug" => k,
            _ => {
                return Err(PackError(format!("flag reason {reason} of rule {rule_id} has no known kind")).into());
            }
        };
        let is_lab = LAB_KINDS.contains(&kind.as_str());
        let domain_ok = if is_lab {
            allowed("labs")
        } else {
            allowed("genome") && allowed("records")
        };
        if !domain_ok {
            if is_lab {
                by_scope.insert("labs");
            } else {
                for d in ["genome", "records"] {
                    if !allowed(d) {
                        by_scope.insert(d);
                    }
                }
            }
            continue;
        }
        let mut out = Map::new();
        out.insert("rank".into(), json!(flags.len() + 1));
        out.insert(
            "kind".into(),
            json!(if kind == "lab_range" {
                "out_of_range"
            } else {
                kind.as_str()
            }),
        );
        out.insert("rule".into(), json!(rule_id));
        let empty = Map::new();
        let rule = pack.rule(rule_id).unwrap_or(&empty);
        let unit = s(f, "unit");
        let unit_txt = unit.unwrap_or_default();
        let value = s(f, "value");
        let text_out;
        if is_lab {
            let group = s(f, "group").unwrap_or_default();
            let name = pack.name(group, lang);
            let loinc = s(f, "loinc")
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .or_else(|| pack.default_loinc(group));
            out.insert(
                "code".into(),
                loinc.map_or(Value::Null, |l| json!(format!("LOINC:{l}"))),
            );
            out.insert("name".into(), json!(name));
            out.insert("value".into(), f.get("value").cloned().unwrap_or(Value::Null));
            out.insert("unit".into(), f.get("unit").cloned().unwrap_or(Value::Null));
            out.insert("date".into(), when(s(f, "date"))?);
            if kind == "lab_range" {
                out.insert("range".into(), f.get("ref").cloned().unwrap_or(Value::Null));
                out.insert("flag".into(), f.get("flag").cloned().unwrap_or(Value::Null));
                text_out = pack.text(
                    reason,
                    lang,
                    &[
                        ("name", P::from(&name)),
                        ("value", P::from(value)),
                        ("unit", P::from(unit_txt)),
                        ("ref", P::from(s(f, "ref").unwrap_or_default())),
                        ("flag", P::from(s(f, "flag"))),
                        ("when", P::from(on(s(f, "date"))?)),
                    ],
                )?;
            } else if kind == "rcv" {
                let rcv_pct = rcv_percent(rule);
                out.insert("previous".into(), f.get("prev").cloned().unwrap_or(Value::Null));
                out.insert("change_pct".into(), f.get("change_pct").cloned().unwrap_or(Value::Null));
                out.insert("rcv_pct".into(), json!(rcv_pct));
                text_out = pack.text(
                    "RCV_CHANGE",
                    lang,
                    &[
                        ("name", P::from(&name)),
                        ("prev", P::from(s(f, "prev"))),
                        ("value", P::from(value)),
                        ("unit", P::from(unit_txt)),
                        ("change_pct", P::from(s(f, "change_pct"))),
                        ("rcv_pct", P::from(rcv_pct)),
                        ("prev_date", P::from(when_s(s(f, "prev_date"))?)),
                    ],
                )?;
            } else {
                let points = int(rule.get("points")).unwrap_or(3);
                out.insert("points".into(), json!(points));
                out.insert("slope_per_year".into(), f.get("slope").cloned().unwrap_or(Value::Null));
                text_out = pack.text(
                    reason,
                    lang,
                    &[
                        ("name", P::from(&name)),
                        ("n", P::I(points)),
                        ("slope", P::from(s(f, "slope"))),
                        ("value", P::from(value)),
                        ("unit", P::from(unit_txt)),
                        ("when", P::from(on(s(f, "date"))?)),
                    ],
                )?;
            }
            if let Some(c) = convert(Some(group), value, unit, units) {
                out.insert("canonical".into(), c);
            }
        } else {
            let drug = capitalize(s(f, "drug").unwrap_or_default());
            out.insert("drug".into(), json!(drug));
            out.insert("gene".into(), f.get("gene").cloned().unwrap_or(Value::Null));
            out.insert("phenotype".into(), f.get("phenotype").cloned().unwrap_or(Value::Null));
            out.insert("guideline".into(), json!("CPIC"));
            text_out = pack.text(
                "GENE_DRUG",
                lang,
                &[
                    ("drug", P::from(&drug)),
                    ("gene", P::from(s(f, "gene"))),
                    ("phenotype", P::from(s(f, "phenotype"))),
                ],
            )?;
        }
        out.insert(
            "text".into(),
            json!(text_out.split_whitespace().collect::<Vec<_>>().join(" ")),
        );
        out.insert("src".into(), f.get("src").cloned().unwrap_or(Value::Null));
        out.retain(|_, v| !v.is_null());
        flags.push(Value::Object(out));
    }

    let labs_ok = allowed("labs");
    if !labs_ok && (has_items(body, "stale") || has_items(body, "never_measured") || has_items(body, "pending")) {
        by_scope.insert("labs");
    }
    let mut stale = Vec::new();
    if labs_ok {
        for st in list(body, "stale") {
            let rule = s(st, "rule").unwrap_or_default();
            let months = window_months(pack, rule);
            let name = pack.name(s(st, "group").unwrap_or_default(), lang);
            let last = when(s(st, "last"))?;
            stale.push(json!({
                "name": name,
                "last": last,
                "window_months": months,
                "rule": st.get("rule"),
                "text": pack.text("STALE", lang, &[
                    ("name", P::from(&name)),
                    ("last", P::from(last.as_str())),
                    ("window_months", P::I(months)),
                ])?,
                "src": st.get("src"),
            }));
        }
    }
    let mut never = Vec::new();
    if labs_ok {
        for n in list(body, "never_measured") {
            let name = pack.name(s(n, "group").unwrap_or_default(), lang);
            never.push(json!({
                "name": name,
                "rule": n.get("rule"),
                "text": pack.text("NEVER_MEASURED", lang, &[("name", P::from(&name)), ("rule", P::from(s(n, "rule")))])?,
            }));
        }
    }
    let records_ok = allowed("records");
    if !records_ok
        && (has_items(body, "medications") || has_items(body, "supplements") || has_items(body, "conditions"))
    {
        by_scope.insert("records");
    }
    let meds = |key: &str| -> Result<Vec<Value>> {
        if !records_ok {
            return Ok(Vec::new());
        }
        list(body, key)
            .map(|m| {
                let name = [s(m, "name"), s(m, "strength")]
                    .into_iter()
                    .flatten()
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                Ok(json!({"name": name, "since": when(s(m, "since"))?, "src": m.get("src")}))
            })
            .collect()
    };
    let mut conditions = Vec::new();
    if records_ok {
        for c in list(body, "conditions") {
            conditions.push(json!({
                "name": c.get("name"),
                "verification": c.get("verification"),
                "since": when(s(c, "since"))?,
                "src": c.get("src"),
            }));
        }
    }
    let mut notes = Vec::new();
    if mode != "minimal" {
        for n in list(body, "notes") {
            let origin = if s(n, "author_kind") == Some("agent") {
                "agent_note"
            } else {
                "person_note"
            };
            notes.push(json!({
                "author_kind": n.get("author_kind"),
                "date": when(s(n, "date"))?,
                "text": untrusted(s(n, "text"), origin)?,
                "src": n.get("src"),
            }));
        }
    }
    let mut pending = Vec::new();
    if labs_ok {
        for p in list(body, "pending") {
            pending.push(json!({
                "name": pack.name(s(p, "group").unwrap_or_default(), lang),
                "date": when(s(p, "date"))?,
                "src": p.get("src"),
            }));
        }
    }
    let mut corrections = Vec::new();
    if labs_ok {
        for c in list(body, "corrections") {
            let fact = format!(
                "{} {} {}",
                pack.name(s(c, "group").unwrap_or_default(), lang),
                c.get("value").map(text).unwrap_or_else(|| "None".into()),
                s(c, "unit").unwrap_or_default()
            );
            let mut item = Map::new();
            item.insert(
                "fact".into(),
                json!(format!("{} ({})", fact.trim(), when_s(s(c, "date"))?)),
            );
            item.insert("status".into(), c.get("status").cloned().unwrap_or(Value::Null));
            item.insert("on".into(), when(s(c, "on"))?);
            item.insert("src".into(), c.get("src").cloned().unwrap_or(Value::Null));
            if let Some(reason) = s(c, "reason").filter(|r| !r.is_empty())
                && mode != "minimal"
            {
                item.insert("reason".into(), untrusted(Some(reason), "person_note")?);
            }
            corrections.push(Value::Object(item));
        }
    }
    let empty = Map::new();
    let fresh = body.get("freshness").and_then(Value::as_object).unwrap_or(&empty);
    let mut freshness = Map::new();
    for domain in ["labs", "wearables", "genome"] {
        if let Some(d) = fresh.get(domain).filter(|v| truthy(v)) {
            if allowed(domain) {
                freshness.insert(domain.into(), when(d.as_str())?);
            } else {
                by_scope.insert(domain);
            }
        }
    }

    let withheld = body.get("withheld").and_then(Value::as_object).unwrap_or(&empty);
    let mut limitations: Vec<String> = pack
        .limitations
        .iter()
        .map(|code| pack.text(code, lang, &[]))
        .collect::<Result<_, _>>()?;
    let sensitive = withheld.get("sensitive").cloned().unwrap_or(json!(0));
    if truthy(&sensitive) {
        limitations.push(pack.text(
            "SENSITIVE_EXCLUDED",
            lang,
            &[("n", P::I(sensitive.as_i64().unwrap_or(0)))],
        )?);
    }
    Ok(json!({
        "profile": body.get("profile"),
        "as_of": body.get("as_of"),
        "lang": lang,
        "identity_mode": mode,
        "rules_version": body.get("rules"),
        "state_hash": body.get("state_hash"),
        "data": {
            "flags": flags,
            "medications": meds("medications")?,
            "supplements": meds("supplements")?,
            "active_conditions": conditions,
            "never_measured": never,
            "stale": stale,
            "pending": pending,
            "corrections": corrections,
            "notes": notes,
            "data_freshness": freshness,
        },
        "withheld": {
            "sensitive": sensitive,
            "by_scope": by_scope.into_iter().collect::<Vec<_>>(),
            "minor_rule": withheld.get("minor_rule").cloned().unwrap_or(json!(0)),
        },
        "limitations": limitations,
        "notice": pack.text("notice", lang, &[])?,
    }))
}

/// Python's `str.capitalize()`: the first character upper, the rest lower.
pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}
