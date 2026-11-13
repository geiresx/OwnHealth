//! check_drug (`state/drugs.py`): PGx facts + current medications + relevant labs + CYP2D6
//! phenoconversion (genomics.md 8).
//!
//! Guidance is quoted verbatim from the bundled CPIC snapshot (CC0, from CPIC's own API) with its
//! classification, the pair's CPIC level and the guideline URL. Nothing from ClinPGx or PharmGKB
//! (CC BY-SA 4.0) is bundled. DPWG text is quoted from the person's own genome file, as received and
//! for the genotype phenotype only. Our own words never advise: they say what the record holds and
//! what is not assessed.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use chrono::NaiveDate;
use ownhealth_core::access::{Scopes, today_utc};
use ownhealth_core::records::active_medications;
use ownhealth_core::{Error, Result};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::engine::{Finding, casefold, drug_key, evaluate, iso, med_drug, str_of};
use crate::genome;
use crate::num::{self, dec, num};
use crate::pack::{P, Pack, cpic_snapshot, load_pack, text};
use crate::rendering::capitalize;

/// CPIC looks these genes up by activity score.
const AS_GENES: [&str; 3] = ["CYP2D6", "CYP2C9", "DPYD"];
const NO_RESULT: &str = "No Result";
/// The drug check's texts outside the rule pack (DPWG), so the pack and its hash stay as shipped.
const OWN_TEXTS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/rules/check_drug.yaml"
));

/// One `check_drug.yaml` template in `lang` (en or es).
fn own_text(key: &str, lang: &str) -> Result<String> {
    static DOC: OnceLock<std::result::Result<Value, String>> = OnceLock::new();
    let doc = DOC
        .get_or_init(|| serde_yaml::from_str(OWN_TEXTS).map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| Error::Invalid(format!("check_drug.yaml: {e}")))?;
    doc.get("templates")
        .and_then(|t| t.get(key))
        .and_then(|t| t.get(lang))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Invalid(format!("check_drug.yaml: no {lang} text for {key}")))
}

/// A DPWG entry of a fact's drug_guidance, kept only when it quotes text for the finding's own
/// (genotype) phenotype. The file is untrusted: its url is kept only as an https URL and its level
/// only as a short code, so neither carries free text past the quote's untrusted wrapper.
fn dpwg_row(gene: &str, phenotype: Option<&str>, d: &Value) -> Option<Value> {
    if d["source"] != "DPWG" {
        return None;
    }
    let text = d["recommendation"]["text"]
        .as_str()
        .or_else(|| d["recommendation"].as_str())?;
    if text.trim().is_empty() {
        return None;
    }
    if let Some(p) = d["phenotype"].as_str()
        && norm_value(gene, Some(p)) != norm_value(gene, phenotype)
    {
        return None;
    }
    let url = d["url"]
        .as_str()
        .filter(|u| u.starts_with("https://") && u.len() > 8 && u.len() <= 500 && !u.contains(char::is_whitespace));
    let level = d["level"]
        .as_str()
        .filter(|l| (1..=4).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphanumeric()));
    Some(json!({
        "level": level,
        "url": url,
        "quote": {"trust": "untrusted_text", "origin": "genome_facts", "text": text},
    }))
}

fn arr<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Map<String, Value>> + 'a {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

/// `{name: drugid}` of the snapshot's drugs and drugs_ids_only (later entries win).
fn drug_ids(snap: &Value) -> HashMap<String, Value> {
    arr(snap, "drugs")
        .chain(arr(snap, "drugs_ids_only"))
        .map(|d| {
            (
                d.get("name").map(text).unwrap_or_default(),
                d.get("drugid").cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

/// es/en generic or brand name, or a CPIC drug id, to `{cpic_drug_id, name}`. None when unknown.
pub fn resolve(pack: &Pack, snap: &Value, name: &str) -> Option<Value> {
    let raw = name.trim();
    if raw.is_empty() {
        return None;
    }
    let ids = drug_ids(snap);
    let wanted = casefold(raw);
    let by_id = ids
        .iter()
        .find(|(_, v)| v.as_str().is_some_and(|id| casefold(id) == wanted))
        .map(|(k, _)| k.clone());
    let key = by_id.or_else(|| drug_key(pack, Some(raw)))?;
    Some(json!({"cpic_drug_id": ids.get(&key).cloned().unwrap_or(Value::Null), "name": key}))
}

/// `format(score.normalize(), "f")`, with `.0` when integral.
pub fn score_key(score: &Decimal) -> String {
    let s = num(&score.normalize());
    if s.contains('.') { s } else { format!("{s}.0") }
}

fn norm_value(gene: &str, v: Option<&str>) -> String {
    let mut v = v.unwrap_or_default().trim();
    if v.to_uppercase().starts_with(&gene.to_uppercase()) {
        v = v.get(gene.len()..).unwrap_or_default();
    }
    casefold(v).trim().to_string()
}

#[derive(Debug, Clone)]
struct Person {
    phenotype: Option<String>,
    activity_score: Option<Decimal>,
}

/// Does a CPIC recommendation's lookup key fit the person's per-gene values?
fn matches(rec: &Map<String, Value>, person: &HashMap<String, Person>) -> bool {
    let empty = Map::new();
    let lookup = rec.get("lookupkey").and_then(Value::as_object).unwrap_or(&empty);
    for (gene, key) in lookup {
        let key = text(key);
        let Some(p) = person.get(gene) else {
            if key != NO_RESULT {
                return false;
            }
            continue;
        };
        match p.activity_score {
            Some(score) if AS_GENES.contains(&gene.as_str()) => {
                if let Some(floor) = key.strip_prefix('≥') {
                    if num::parse(floor).is_none_or(|f| score < f) {
                        return false;
                    }
                } else if key == "n/a" || key == NO_RESULT || num::parse(&key) != Some(score) {
                    return false;
                }
            }
            _ => {
                let of = |k: &str| {
                    rec.get(k)
                        .and_then(|m| m.get(gene))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                };
                let phen = norm_value(gene, of("phenotypes").or_else(|| of("allelestatus")));
                let mine = norm_value(gene, p.phenotype.as_deref());
                if norm_value(gene, Some(&key)) != mine && phen != mine {
                    return false;
                }
            }
        }
    }
    true
}

fn lookup_values(r: &Map<String, Value>) -> Vec<String> {
    r.get("lookupkey")
        .and_then(Value::as_object)
        .map(|m| m.values().map(text).collect())
        .unwrap_or_default()
}

fn guidance_for<'a>(snap: &'a Value, drug_id: &str, person: &HashMap<String, Person>) -> Vec<&'a Map<String, Value>> {
    let recs: Vec<&Map<String, Value>> = arr(snap, "recommendations")
        .filter(|r| str_of(r, "drugid") == Some(drug_id) && matches(r, person))
        .collect();
    let mut exact: Vec<&Map<String, Value>> = recs
        .iter()
        .copied()
        .filter(|r| !lookup_values(r).iter().any(|v| v.starts_with('≥')))
        .collect();
    if exact.is_empty() && !recs.is_empty() {
        // only "≥" bands matched: the highest one stands in for the score
        let top = recs
            .iter()
            .flat_map(|r| lookup_values(r))
            .filter_map(|v| v.strip_prefix('≥').and_then(num::parse))
            .max();
        if let Some(top) = top {
            let want = format!("≥{}", score_key(&top));
            exact = recs.into_iter().filter(|r| lookup_values(r).contains(&want)).collect();
        }
    }
    let useful: Vec<_> = exact
        .iter()
        .copied()
        .filter(|r| str_of(r, "classification") != Some("No Recommendation"))
        .collect();
    if useful.is_empty() { exact } else { useful }
}

/// The CPIC activity-score band of a (converted) score; min/max inclusive, above/below exclusive.
pub fn band(pack: &Pack, score: Decimal) -> Option<String> {
    let bands = pack.phenoconversion.get("rule")?.get("bands")?.as_array()?;
    for b in bands.iter().filter_map(Value::as_object) {
        let lo_ok = match b.get("above") {
            Some(a) => dec(Some(a)).is_some_and(|a| score > a),
            None => dec(b.get("min")).is_some_and(|m| score >= m),
        };
        let below = b.contains_key("below");
        let hi = if below { dec(b.get("below")) } else { dec(b.get("max")) };
        let hi_ok = hi.is_none_or(|h| if below { score < h } else { score <= h });
        if lo_ok && hi_ok {
            return b.get("phenotype").map(text);
        }
    }
    None
}

fn inhibitors<'a>(
    pack: &Pack,
    gene: &str,
    meds: &[&'a Map<String, Value>],
) -> Vec<(&'static str, &'a Map<String, Value>)> {
    let table = pack
        .phenoconversion
        .get("inhibitors")
        .and_then(|t| t.get(gene))
        .cloned()
        .unwrap_or(Value::Null);
    let mut out = Vec::new();
    for &med in meds {
        let Some(k) = med_drug(pack, med) else { continue };
        for strength in ["strong", "moderate"] {
            let listed = table
                .get(strength)
                .and_then(Value::as_array)
                .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(k.as_str())));
            if listed {
                out.push((strength, med));
                break;
            }
        }
    }
    out.sort_by_key(|(s, m)| (*s != "strong", str_of(m, "name").unwrap_or_default().to_lowercase()));
    out
}

fn med_label(m: &Map<String, Value>) -> String {
    [str_of(m, "name"), str_of(m, "strength")]
        .into_iter()
        .flatten()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// mcp.md 5.6 `data`: drug, pgx[], phenoconversion[], current_medications_relevant[],
/// relevant_labs[], guidance[] (quoted, cited), not_assessed[], withheld.by_scope. An unknown drug is
/// `Error::NotFound` (never "no interaction").
pub async fn check_drug(
    conn: &mut PgConnection,
    profile_id: Uuid,
    drug: &str,
    scopes: &Scopes,
    lang: &str,
    as_of: Option<NaiveDate>,
) -> Result<Value> {
    let pack = load_pack()?;
    let snap = cpic_snapshot()?;
    let lang = if lang == "es" { "es" } else { "en" };
    let resolved = resolve(&pack, snap, drug).ok_or_else(|| Error::NotFound(format!("Unknown drug '{drug}'.")))?;
    let key = resolved["name"].as_str().unwrap_or_default().to_string();
    let drug_id = resolved["cpic_drug_id"].as_str().map(str::to_string);
    let as_of = as_of.unwrap_or_else(today_utc);
    let t = |k: &str, params: &[(&str, P)]| pack.text(k, lang, params);
    let has_genome = scopes.contains("read:genome");
    let has_records = scopes.contains("read:records");
    let has_labs = scopes.contains("read:labs");
    let by_scope: Vec<&str> = [("genome", has_genome), ("records", has_records), ("labs", has_labs)]
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(d, _)| d)
        .collect();

    let pairs: Vec<&Map<String, Value>> = arr(snap, "pairs")
        .filter(|p| str_of(p, "drugname") == Some(&key))
        .collect();
    let genes: Vec<String> = pairs
        .iter()
        .filter_map(|p| str_of(p, "genesymbol").map(str::to_string))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let level: HashMap<String, String> = pairs
        .iter()
        .map(|p| {
            (
                p.get("genesymbol").map(text).unwrap_or_default(),
                p.get("cpiclevel").map(text).unwrap_or_default(),
            )
        })
        .collect();
    let gid = arr(snap, "drugs")
        .find(|d| str_of(d, "name") == Some(&key))
        .and_then(|d| d.get("guidelineid").cloned())
        .filter(|g| !g.is_null());
    let guideline = gid.and_then(|gid| arr(snap, "guidelines").find(|g| g.get("id") == Some(&gid)));
    let url = match guideline {
        Some(g) => g.get("url").cloned().unwrap_or(Value::Null),
        None => pairs
            .first()
            .and_then(|p| p.get("guidelineurl").cloned())
            .unwrap_or(Value::Null),
    };

    let rows = if has_records {
        active_medications(
            &mut *conn,
            profile_id,
            Some(as_of),
            None,
            None,
            scopes.contains("read:sensitive"),
        )
        .await?
    } else {
        Vec::new()
    };
    // medicines and supplements alike (v0.3); each row below says which kind it is
    let meds: Vec<&Map<String, Value>> = rows.iter().collect();
    let currently_taking = if has_records {
        json!(meds.iter().any(|m| med_drug(&pack, m).as_deref() == Some(&key)))
    } else {
        Value::Null
    };

    let mut not_assessed: Vec<String> = Vec::new();
    let mut pgx = Vec::new();
    let mut findings: Vec<(String, Finding)> = Vec::new();
    // DPWG rows of each kept finding's own drug_guidance (the genome crate keeps only this drug's)
    let mut dpwg: Vec<(String, Value)> = Vec::new();
    // each kept finding's `stale` (v0.3): a call made against an older CPIC or PharmCAT release
    let mut stale: HashMap<String, Value> = HashMap::new();
    let finding =
        |findings: &[(String, Finding)], gene: &str| findings.iter().find(|(g, _)| g == gene).map(|(_, f)| f.clone());
    if has_genome && !pairs.is_empty() {
        if let Some(id) = &drug_id {
            for raw in genome::pgx_for_drug(&mut *conn, profile_id, id, as_of).await? {
                let f = Finding::from_value(&raw);
                if let Some(g) = f.gene.clone()
                    && genes.contains(&g)
                    && finding(&findings, &g).is_none()
                {
                    for d in raw["guidance"].as_array().into_iter().flatten() {
                        if let Some(d) = dpwg_row(&g, f.phenotype.as_deref(), d) {
                            dpwg.push((g.clone(), d));
                        }
                    }
                    stale.insert(g.clone(), raw.get("stale").cloned().unwrap_or(Value::Null));
                    findings.push((g, f));
                }
            }
            for gene in &genes {
                let Some(f) = finding(&findings, gene) else {
                    not_assessed.push(t("drug.na.gene", &[("gene", P::from(gene))])?);
                    continue;
                };
                pgx.push(json!({
                    "gene": gene,
                    "diplotype": f.diplotype,
                    "phenotype": f.phenotype,
                    "activity_score": f.activity_score.as_ref().map(num),
                    "evidence": f.evidence,
                    "confidence": f.confidence,
                    "call_state": f.call_state,
                    "cpic_level": level.get(gene),
                    "stale": stale.get(gene).cloned().unwrap_or(Value::Null),
                    "src": f.src,
                }));
            }
        }
    } else if has_genome && pairs.is_empty() {
        not_assessed.push(t("drug.na.no_pair", &[("drug", P::from(&key))])?);
    }

    // phenoconversion (needs genome and records)
    let empty = Map::new();
    let rule = pack
        .phenoconversion
        .get("rule")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let rule_gene = rule.get("gene").map(text);
    let mut phenoconversion: Vec<Map<String, Value>> = Vec::new();
    let mut functional: HashMap<String, Person> = HashMap::new();
    if has_genome && has_records {
        for gene in &genes {
            let inh = inhibitors(&pack, gene, &meds);
            let Some(&(strength, med)) = inh.first() else { continue };
            let f = finding(&findings, gene);
            let is_rule_gene = rule_gene.as_deref() == Some(gene.as_str());
            let mut entry = Map::new();
            entry.insert("gene".into(), json!(gene));
            entry.insert("by".into(), json!(med_label(med)));
            entry.insert("inhibitor_strength".into(), json!(strength));
            entry.insert("computed".into(), json!(false));
            entry.insert("functional_phenotype".into(), Value::Null);
            entry.insert("activity_score".into(), Value::Null);
            entry.insert("activity_score_functional".into(), Value::Null);
            entry.insert(
                "rule".into(),
                if is_rule_gene {
                    rule.get("id").cloned().unwrap_or(Value::Null)
                } else {
                    Value::Null
                },
            );
            entry.insert(
                "basis".into(),
                pack.phenoconversion
                    .get("inhibitors_cite")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            entry.insert("src".into(), med.get("src_id").cloned().unwrap_or(Value::Null));
            let strength_txt = t(&format!("strength.{strength}"), &[])?;
            let score = f.as_ref().and_then(|f| f.activity_score);
            if let (true, Some(f), Some(score)) = (is_rule_gene, f.as_ref(), score) {
                let mult = dec(rule.get("multipliers").and_then(|m| m.get(strength)))
                    .ok_or_else(|| Error::Invalid(format!("no multiplier for {strength}")))?;
                let fscore = score * mult;
                let fphen = band(&pack, fscore);
                entry.insert("computed".into(), json!(true));
                entry.insert("functional_phenotype".into(), json!(fphen));
                entry.insert("activity_score".into(), json!(num(&score)));
                entry.insert("activity_score_functional".into(), json!(score_key(&fscore)));
                entry.insert(
                    "text".into(),
                    json!(t(
                        "drug.pheno.computed",
                        &[
                            ("by", P::from(med_label(med))),
                            ("strength", P::from(&strength_txt)),
                            ("score", P::from(num(&score))),
                            ("score_functional", P::from(score_key(&fscore))),
                            ("functional_phenotype", P::from(fphen.clone())),
                            ("phenotype", P::from(f.phenotype.clone())),
                        ],
                    )?),
                );
                entry.insert("based_on".into(), json!([f.src, med.get("src_id")]));
                if let Some(fp) = fphen
                    && norm_value(gene, Some(&fp)) != norm_value(gene, f.phenotype.as_deref())
                {
                    functional.insert(
                        gene.clone(),
                        Person {
                            phenotype: Some(fp),
                            activity_score: Some(fscore),
                        },
                    );
                }
            } else if is_rule_gene {
                entry.insert(
                    "text".into(),
                    json!(t(
                        "drug.pheno.no_score",
                        &[("by", P::from(med_label(med))), ("strength", P::from(&strength_txt))]
                    )?),
                );
            } else {
                entry.insert(
                    "text".into(),
                    json!(t(
                        "drug.pheno.not_computed",
                        &[
                            ("by", P::from(med_label(med))),
                            ("strength", P::from(&strength_txt)),
                            ("gene", P::from(gene))
                        ],
                    )?),
                );
            }
            phenoconversion.push(entry);
        }
    }

    // guidance, verbatim from the CPIC snapshot
    let mut guidance: Vec<Value> = Vec::new();
    if let (false, Some(drug_id)) = (findings.is_empty(), drug_id.as_deref()) {
        let person: HashMap<String, Person> = findings
            .iter()
            .map(|(g, f)| {
                (
                    g.clone(),
                    Person {
                        phenotype: f.phenotype.clone(),
                        activity_score: f.activity_score,
                    },
                )
            })
            .collect();
        let mut variants = vec![("genotype", person.clone())];
        if !functional.is_empty() {
            let mut both = person;
            both.extend(functional.clone());
            variants.push(("functional", both));
        }
        let via = phenoconversion
            .iter()
            .find(|p| p.get("computed") == Some(&json!(true)))
            .and_then(|p| p.get("src").and_then(Value::as_str).map(str::to_string));
        let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
        for (kind, values) in &variants {
            for r in guidance_for(snap, drug_id, values) {
                let lookup: BTreeSet<&String> = r
                    .get("lookupkey")
                    .and_then(Value::as_object)
                    .map(|m| m.keys().collect())
                    .unwrap_or_default();
                let phen = lookup
                    .iter()
                    .map(|g| match values.get(*g) {
                        Some(p) => format!("{g} {}", p.phenotype.as_deref().unwrap_or("None")),
                        None => format!("{g} {NO_RESULT}"),
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let applies = t(&format!("drug.applies.{kind}"), &[("phenotype", P::from(phen))])?;
                let population = r.get("population").map(text).unwrap_or_else(|| "None".into());
                let quote = r.get("drugrecommendation").map(text).unwrap_or_default();
                if !seen.insert((kind.to_string(), population.clone(), quote)) {
                    continue;
                }
                let levels: BTreeSet<&str> = lookup
                    .iter()
                    .filter_map(|g| level.get(*g).map(String::as_str))
                    .filter(|l| !l.is_empty())
                    .collect();
                let levels = levels.into_iter().collect::<Vec<_>>().join("/");
                let mut based_on: Vec<String> = lookup
                    .iter()
                    .filter_map(|g| finding(&findings, g).and_then(|f| f.src))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                if *kind == "functional"
                    && let Some(v) = &via
                {
                    based_on.push(v.clone());
                }
                let pop = r
                    .get("population")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|p| !p.is_empty());
                guidance.push(json!({
                    "source": "CPIC",
                    "level": if levels.is_empty() { Value::Null } else { json!(levels) },
                    "strength": r.get("classification"),
                    "applies_to": applies,
                    "population": pop,
                    "quote": r.get("drugrecommendation"),
                    "url": url,
                    "recommendation_id": r.get("id"),
                    "origin": "cpic_snapshot",
                    "license": "CC0-1.0",
                    "based_on": based_on,
                }));
            }
        }
        let cpic_empty = guidance.is_empty();
        // DPWG beside CPIC: the genome file's own text, for the genotype phenotype only
        for (gene, d) in &dpwg {
            let Some(f) = finding(&findings, gene) else { continue };
            let phen = format!("{gene} {}", f.phenotype.as_deref().unwrap_or("None"));
            guidance.push(json!({
                "source": "DPWG",
                "level": d["level"],
                "strength": null,
                "applies_to": t("drug.applies.genotype", &[("phenotype", P::from(phen))])?,
                "population": null,
                "quote": d["quote"],
                "url": d["url"],
                "recommendation_id": null,
                "origin": "genome_facts",
                "license": null,
                "based_on": f.src.into_iter().collect::<Vec<_>>(),
            }));
        }
        // the checked genes whose fact carries no DPWG text we could quote
        let uncovered: Vec<&str> = pgx
            .iter()
            .filter_map(|p| p["gene"].as_str())
            .filter(|g| !dpwg.iter().any(|(d, _)| d == g))
            .collect();
        if !uncovered.is_empty() {
            not_assessed.push(own_text("drug.na.dpwg", lang)?.replace("{genes}", &uncovered.join(", ")));
        }
        if cpic_empty {
            if key == "warfarin" {
                not_assessed.push(t("drug.na.warfarin", &[])?);
            } else {
                for (gene, f) in &findings {
                    not_assessed.push(t(
                        "drug.na.no_rec",
                        &[("gene", P::from(gene)), ("phenotype", P::from(f.phenotype.clone()))],
                    )?);
                }
            }
        }
    }

    // current medications, each with how it relates to this drug
    let mut current = Vec::new();
    for &m in &meds {
        let k = med_drug(&pack, m);
        let (mut relation, mut relation_text) = ("not_assessed".to_string(), t("drug.relation.not_assessed", &[])?);
        if k.as_deref() == Some(&key) {
            relation = "same_drug".into();
            relation_text = t("drug.relation.same", &[])?;
        } else {
            for gene in &genes {
                if let Some(&(hit, _)) = inhibitors(&pack, gene, &[m]).first() {
                    relation = format!("inhibitor:{gene}:{hit}");
                    let strength = t(&format!("strength.{hit}"), &[])?;
                    relation_text = t(
                        "drug.relation.inhibitor",
                        &[("gene", P::from(gene)), ("strength", P::from(strength))],
                    )?;
                    break;
                }
            }
        }
        current.push(json!({
            "name": med_label(m),
            "kind": m.get("kind"),
            "since": m.get("started"),
            "relation": relation,
            "relation_text": relation_text,
            "src": m.get("src_id"),
        }));
    }

    let mut labs = Vec::new();
    if has_labs {
        let items: Vec<Value> = pack
            .drug_labs
            .get(&key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in items {
            let group = item.get("lab").map(text).unwrap_or_default();
            let ev = evaluate(&mut *conn, profile_id, as_of, &pack, None, Some(&group)).await?;
            let marker = ev.marker(&group);
            let p = marker.and_then(|m| m.latest());
            let reason = match item.get("reason") {
                Some(Value::Object(r)) => r.get(lang).cloned().unwrap_or(Value::Null),
                Some(other) => other.clone(),
                None => Value::Null,
            };
            labs.push(json!({
                "name": pack.name(&group, lang),
                "alias_group": group,
                "value": p.map(|p| p.shown()),
                "unit": p.and_then(|p| p.unit.clone()),
                "date": p.map(|p| iso(p.date)),
                "flag": p.and_then(|p| p.flag.clone()),
                "status": marker.map_or("nodata", |m| m.status),
                "stale": marker.is_some_and(|m| m.stale),
                "reason": reason,
                "src": p.map(|p| p.src.clone()),
            }));
        }
    }

    not_assessed.push(t("drug.na.ddi", &[])?);
    let retrieved = snap.get("retrieved").map(text).unwrap_or_default();
    let mut limitations = vec![t("drug.lim.snapshot", &[("date", P::from(&retrieved))])?];
    if !phenoconversion.is_empty() {
        limitations.push(t("drug.lim.phenoconversion", &[])?);
    }
    if !dpwg.is_empty() {
        limitations.push(own_text("drug.lim.dpwg", lang)?);
    }
    // CPIC rows only: a DPWG row has no population and is not one more indication
    let populations: BTreeSet<String> = guidance
        .iter()
        .filter(|g| g["source"] == "CPIC")
        .map(|g| g["population"].to_string())
        .collect();
    if populations.len() > 1 {
        limitations.push(t("drug.lim.indication", &[])?);
    }
    let codes: BTreeSet<String> = meds
        .iter()
        .filter(|m| med_drug(&pack, m).as_deref() == Some(&key))
        .filter(|m| m.get("code").is_some_and(crate::pack::truthy))
        .map(|m| {
            format!(
                "{}:{}",
                m.get("code_system").map(text).unwrap_or_else(|| "None".into()),
                m.get("code").map(text).unwrap_or_default()
            )
        })
        .collect();
    Ok(json!({
        "drug": {
            "name": capitalize(&key),
            "query": drug,
            "cpic_drug_id": drug_id,
            "codes_as_received": codes,
            "currently_taking": currently_taking,
            "genes": genes,
        },
        "pgx": pgx,
        "phenoconversion": phenoconversion,
        "current_medications_relevant": current,
        "relevant_labs": labs,
        "guidance": guidance,
        "not_assessed": not_assessed,
        "limitations": limitations,
        "withheld": {"by_scope": by_scope},
        "db_versions": {"cpic": retrieved},
        "notice": t("drug.notice", &[])?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The texts beside the pack pass the cited-file checks: version, https cite, es and en, wording.
    #[test]
    fn own_texts_are_a_cited_file_in_both_languages() -> Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/rules/check_drug.yaml");
        let file = crate::cited::load(&path)?;
        assert_eq!(file.version, "check_drug 0.1.0");
        for key in ["drug.na.dpwg", "drug.lim.dpwg"] {
            for lang in ["en", "es"] {
                assert!(!own_text(key, lang)?.is_empty(), "{key} {lang}");
            }
        }
        assert!(own_text("drug.na.missing", "en").is_err());
        Ok(())
    }

    #[test]
    fn score_keys_print_like_python() {
        let d = |s: &str| num::parse(s).unwrap_or_default();
        assert_eq!(score_key(&(d("2.0") * d("0"))), "0.0");
        assert_eq!(score_key(&(d("2.0") * d("0.5"))), "1.0");
        assert_eq!(score_key(&d("1.25")), "1.25");
        assert_eq!(score_key(&d("100")), "100.0");
    }
}
