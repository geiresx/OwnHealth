//! Cross-profile hints (data/family/hints.yaml) without a database: the shipped file loads, a broken
//! one is refused, and the consent and gate matrix of `hint_items` holds. A minor is never a source, a
//! genome hint never reaches a minor, a lab hint about a minor reaches only her guardians, and a gene
//! name shows only through an open gate. A hint names no source, value or src.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use ownhealth_core::family::catalog;
use ownhealth_rules::family::{
    HintSource, HintViewer, document, family_hash, hint_items, load_hints, parse_hints, render, status_section,
    with_hints,
};
use serde_json::{Value, json};

type R = anyhow::Result<()>;

const SHIPPED: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/family/hints.yaml"));

/// Every scope and gate open, an adult target.
fn open() -> HintViewer {
    HintViewer {
        labs: true,
        genome: true,
        sensitive: true,
        target_minor: false,
        guardian: false,
    }
}

/// A source with LDL-C of 190 or more and a BRCA2 P/LP, sharing `categories`.
fn source(rel: &str, categories: &[&str]) -> HintSource {
    HintSource {
        rel: rel.into(),
        categories: categories.iter().map(|c| c.to_string()).collect(),
        minor: false,
        labs: BTreeSet::from(["lab.ldl_fh".to_string()]),
        findings: BTreeMap::from([("genome.cascade".to_string(), vec![("BRCA2".to_string(), true)])]),
    }
}

fn items(viewer: &HintViewer, sources: &[HintSource]) -> anyhow::Result<Vec<Value>> {
    Ok(hint_items(load_hints()?, catalog()?, viewer, sources))
}

fn rules(hints: &[Value]) -> Vec<String> {
    hints
        .iter()
        .map(|h| {
            format!(
                "{} {}",
                h["rule"].as_str().unwrap_or_default(),
                h["rel"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

#[test]
fn the_shipped_hints_file_loads_with_both_rules_cited() -> R {
    let h = load_hints()?;
    assert_eq!(h.version, "0.1.0");
    let ids: Vec<&str> = h.rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["lab.ldl_fh", "genome.cascade"]);
    assert!(h.rules.iter().all(|r| r.cite.starts_with("https://")));
    assert!(h.rules.iter().any(|r| r.category == "cascade" && !r.minors));
    Ok(())
}

#[test]
fn a_broken_hints_file_is_refused() {
    for (what, from, to, reason) in [
        (
            "a genome hint for minors",
            "minors: false",
            "minors: true",
            "adults only",
        ),
        (
            "an unknown category",
            "category: labs",
            "category: vitals",
            "unknown category",
        ),
        (
            "an http cite",
            "https://doi.org/10.1161",
            "http://doi.org/10.1161",
            "https",
        ),
        (
            "an unknown finding category",
            "[monogenic_pathogenic, acmg_sf]",
            "[monogenic, acmg_sf]",
            "finding category",
        ),
        ("no threshold", "    at_least: 190\n", "", "at_least"),
        (
            "a missing template",
            "    withheld:\n      en: \"A relative shared",
            "    other:\n      en: \"A relative shared",
            "withheld: no title",
        ),
        (
            "an imperative",
            "en: \"The guideline treats",
            "en: \"Ask for a lipid test. The guideline treats",
            "advice",
        ),
    ] {
        let body = SHIPPED.replacen(from, to, 1);
        assert_ne!(body, SHIPPED, "{what}: the mutation did not apply");
        match parse_hints(body.as_bytes()) {
            Ok(_) => panic!("{what} loaded"),
            Err(e) => assert!(e.0.contains(reason), "{what}: {e}"),
        }
    }
}

#[test]
fn hints_fire_exactly_on_consented_categories() -> R {
    assert_eq!(items(&open(), &[])?, Vec::<Value>::new(), "no share, no hint");
    assert_eq!(rules(&items(&open(), &[source("MTH", &["labs"])])?), ["lab.ldl_fh MTH"]);
    assert_eq!(
        rules(&items(&open(), &[source("MTH", &["cascade"])])?),
        ["genome.cascade MTH"]
    );
    assert_eq!(
        rules(&items(&open(), &[source("MTH", &["labs", "cascade"])])?),
        ["lab.ldl_fh MTH", "genome.cascade MTH"]
    );
    // a second-degree relative is outside both rules, and a relative by marriage is no blood relative
    assert_eq!(
        items(&open(), &[source("PGRMTH", &["labs", "cascade"])])?,
        Vec::<Value>::new()
    );
    assert_eq!(
        items(&open(), &[source("SPS", &["labs", "cascade"])])?,
        Vec::<Value>::new()
    );
    // a consented source whose results meet no test gives nothing
    let quiet = HintSource {
        labs: BTreeSet::new(),
        findings: BTreeMap::new(),
        ..source("MTH", &["labs", "cascade"])
    };
    assert_eq!(items(&open(), &[quiet])?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn a_minor_is_never_a_source_and_a_genome_hint_never_reaches_a_minor() -> R {
    let minor = HintSource {
        minor: true,
        ..source("DAU", &["labs", "cascade"])
    };
    assert_eq!(items(&open(), &[minor])?, Vec::<Value>::new());
    // Lucía as the target: her father shares both
    let guardian = HintViewer {
        target_minor: true,
        guardian: true,
        ..open()
    };
    let father = [source("FTH", &["labs", "cascade"])];
    assert_eq!(rules(&items(&guardian, &father)?), ["lab.ldl_fh FTH"]);
    // anyone else who reads her labs sees no hint about her
    let caregiver = HintViewer {
        guardian: false,
        ..guardian.clone()
    };
    assert_eq!(items(&caregiver, &father)?, Vec::<Value>::new());
    Ok(())
}

#[test]
fn every_hint_follows_the_callers_scopes_and_gene_gate() -> R {
    let both = [source("MTH", &["labs", "cascade"])];
    // read:records without read:genome: no genome hint, so no gene name
    let records = HintViewer {
        genome: false,
        sensitive: false,
        ..open()
    };
    let got = items(&records, &both)?;
    assert_eq!(rules(&got), ["lab.ldl_fh MTH"]);
    assert!(!json!(got).to_string().contains("BRCA2"));
    // without read:labs, no lab hint
    let genome_only = HintViewer { labs: false, ..open() };
    assert_eq!(rules(&items(&genome_only, &both)?), ["genome.cascade MTH"]);
    // read:genome with the sensitive gate closed: the hint stays, its gene does not
    let closed = HintViewer {
        sensitive: false,
        ..open()
    };
    let got = items(&closed, &both)?;
    assert_eq!(got[1]["withheld"], true, "{got:?}");
    assert!(got[1].get("gene").is_none(), "{got:?}");
    assert!(!json!(got).to_string().contains("BRCA2"));
    // the control: the gate open names it; a gene that is not sensitive needs no gate
    assert_eq!(items(&open(), &both)?[1]["gene"], "BRCA2");
    let plain = HintSource {
        findings: BTreeMap::from([("genome.cascade".to_string(), vec![("LDLR".to_string(), false)])]),
        ..source("MTH", &["cascade"])
    };
    assert_eq!(items(&closed, &[plain])?[0]["gene"], "LDLR");
    Ok(())
}

#[test]
fn a_hint_carries_only_the_rule_relationship_and_analyte_or_gene() -> R {
    let got = items(&open(), &[source("MTH", &["labs", "cascade"])])?;
    for h in &got {
        let keys: BTreeSet<&str> = h.as_object().into_iter().flatten().map(|(k, _)| k.as_str()).collect();
        let allowed: BTreeSet<&str> = [
            "rule", "category", "rel", "analyte", "gene", "withheld", "basis", "source", "cite",
        ]
        .into_iter()
        .collect();
        assert!(keys.is_subset(&allowed), "{h}");
        assert!(h.get("src").is_none() && h.get("members").is_none(), "{h}");
        assert!(h["cite"].as_str().is_some_and(|c| c.starts_with("https://")), "{h}");
    }
    assert_eq!(got[0]["analyte"], "ldl_c");
    Ok(())
}

#[test]
fn hints_enter_the_family_hash_only_when_present() -> R {
    let day = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default();
    let doc = document("alex", day, &[])?;
    let same = with_hints(doc.clone(), Vec::new())?;
    assert_eq!(same, doc);
    let hinted = with_hints(doc.clone(), items(&open(), &[source("MTH", &["labs"])])?)?;
    assert_ne!(hinted["family_hash"], doc["family_hash"]);
    assert_eq!(hinted["family_hash"].as_str(), Some(family_hash(&hinted)?.as_str()));
    // the words: the gene in the title, a withheld one as a shared finding
    let closed = HintViewer {
        sensitive: false,
        ..open()
    };
    let mut hints = items(&open(), &[source("MTH", &["cascade"])])?;
    hints.extend(items(&closed, &[source("FTH", &["cascade"])])?);
    let shown = render(&with_hints(doc, hints)?)?;
    let h = &shown["hints"];
    assert!(h[0]["title"]["en"].as_str().is_some_and(|t| t.contains("BRCA2")), "{h}");
    assert!(h[0]["title"]["es"].as_str().is_some_and(|t| t.contains("BRCA2")), "{h}");
    assert_eq!(
        h[1]["title"]["es"], "Un familiar ha compartido un hallazgo genético",
        "{h}"
    );
    assert!(!h[1].to_string().contains("BRCA2"), "{h}");
    // the status names hints by rule and relationship only
    let section = status_section(&shown).unwrap_or_default();
    assert_eq!(
        section["hints"],
        json!([{"rule": "genome.cascade", "rel": "MTH", "gene": "BRCA2"}, {"rule": "genome.cascade", "rel": "FTH", "withheld": true}])
    );
    assert!(!section.to_string().contains("http"), "{section}");
    Ok(())
}
