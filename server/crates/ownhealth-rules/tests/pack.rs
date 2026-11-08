//! The YAML rule pack v1 (0.1.1 tests/state/test_pack.py): loads, every rule cites, templates are
//! bilingual, overrides change the id.

use std::collections::BTreeSet;
use std::path::PathBuf;

use ownhealth_rules::pack::{P, RULE_KINDS, SHIPPED, build_pack, cpic_snapshot, load, load_pack, shipped_files, text};
use ownhealth_rules::rules_version;
use serde_json::{Value, json};

type R = anyhow::Result<()>;

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> anyhow::Result<TempDir> {
        let p = std::env::temp_dir().join(format!("oh_rules_{}", ownhealth_core::ids::token_hex(6)));
        std::fs::create_dir_all(&p)?;
        Ok(TempDir(p))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn shipped(name: &str) -> &'static str {
    SHIPPED
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .unwrap_or_default()
}

#[test]
fn shipped_pack() -> R {
    let p = load_pack()?;
    assert_eq!(rules_version()?, "ownhealth-rules 0.1.0");
    assert_eq!(p.version, "ownhealth-rules 0.1.0");
    assert_eq!(p.sha256.len(), 64);
    let kinds: BTreeSet<String> = p
        .rules
        .iter()
        .filter_map(|r| r["kind"].as_str().map(str::to_string))
        .collect();
    assert_eq!(kinds, RULE_KINDS.iter().map(|k| k.to_string()).collect());
    let keys: Vec<&str> = p.systems.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "cardiometabolic",
            "liver",
            "kidney",
            "thyroid",
            "blood",
            "inflammation",
            "vitamins",
            "sleep",
            "fitness"
        ]
    );
    for r in &p.rules {
        assert!(
            r["cite"].as_str().is_some_and(|c| c.starts_with("https://")),
            "{:?}",
            r["id"]
        );
    }
    // every analyte a system shows has a display name in both languages
    for s in &p.systems {
        for m in s.markers.iter().filter_map(Value::as_str) {
            let a = &p.analytes[m];
            assert!(
                a["en"].as_str().is_some_and(|x| !x.is_empty()) && a["es"].as_str().is_some_and(|x| !x.is_empty()),
                "{m}"
            );
        }
    }
    // the documented windows (architecture.md section 16)
    let window = |id: &str| p.rule(id).and_then(|r| r.get("window_days")).cloned();
    assert_eq!(window("recency.hba1c"), Some(json!(183)));
    assert_eq!(window("recency.lipids"), Some(json!(365)));
    assert_eq!(window("recency.thyroid"), Some(json!(365)));
    Ok(())
}

#[test]
fn every_drug_in_rules_resolves_to_the_cpic_table() -> R {
    let p = load_pack()?;
    let snap = cpic_snapshot()?;
    let mut known: BTreeSet<String> = ["drugs", "drugs_ids_only"]
        .iter()
        .flat_map(|k| snap[*k].as_array().cloned().unwrap_or_default())
        .filter_map(|d| d["name"].as_str().map(str::to_string))
        .collect();
    known.extend(p.drugs.keys().cloned());
    for r in p.rules_of("gene_drug").chain(p.rules_of("gene_lab")) {
        for d in r.get("drugs").and_then(Value::as_array).into_iter().flatten().map(text) {
            assert!(known.contains(&d) && p.drugs.contains_key(&d), "{:?} {d}", r["id"]);
        }
    }
    for (gene, classes) in p.phenoconversion["inhibitors"].as_object().into_iter().flatten() {
        for names in classes.as_object().into_iter().flat_map(|c| c.values()) {
            assert!(
                names
                    .as_array()
                    .into_iter()
                    .flatten()
                    .all(|n| p.drugs.contains_key(&text(n))),
                "{gene}"
            );
        }
    }
    for (drug, labs) in &p.drug_labs {
        assert!(p.drugs.contains_key(drug));
        for lab in labs.as_array().into_iter().flatten() {
            assert!(p.analytes.contains_key(&text(&lab["lab"])), "{drug}");
            let langs: BTreeSet<&String> = lab["reason"]
                .as_object()
                .map(|r| r.keys().collect())
                .unwrap_or_default();
            assert_eq!(langs.into_iter().cloned().collect::<Vec<_>>(), ["en", "es"], "{drug}");
        }
    }
    Ok(())
}

#[test]
fn override_changes_version_and_hash() -> R {
    let shipped_pack = load_pack()?;
    let dir = TempDir::new()?;
    std::fs::write(
        dir.0.join("thyroid.yaml"),
        shipped("thyroid.yaml").replace("365", "180"),
    )?;
    let local = load(dir.0.to_str())?;
    assert_eq!(
        local.version,
        format!("ownhealth-rules 0.1.0+local.{}", &local.sha256[..8])
    );
    assert_ne!(local.sha256, shipped_pack.sha256);
    assert_eq!(
        local.rule("recency.thyroid").and_then(|r| r.get("window_days")),
        Some(&json!(180))
    );
    Ok(())
}

#[test]
fn a_rule_without_cite_is_refused() -> R {
    let dir = TempDir::new()?;
    for (name, src) in SHIPPED {
        let src = if name == "liver.yaml" {
            src.replacen("cite: https://", "cite: http://", 1)
        } else {
            src.to_string()
        };
        std::fs::write(dir.0.join(name), src)?;
    }
    let files = ownhealth_rules::pack::read_dir(&dir.0)?;
    let err = build_pack(&files, true)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.starts_with("rule recency.liver: ") && err.contains("cite"), "{err}"); // the first cite in liver.yaml
    Ok(())
}

#[test]
fn a_template_missing_spanish_is_refused() -> R {
    let mut files = shipped_files()?;
    if let Some(t) = files
        .get_mut("templates.yaml")
        .and_then(|d| d.get_mut("templates"))
        .and_then(Value::as_object_mut)
    {
        t.insert("notice".into(), json!({"en": "only English"}));
    }
    let err = build_pack(&files, true)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("en and es"), "{err}");
    Ok(())
}

#[test]
fn cpic_snapshot_is_cc0_and_complete() -> R {
    let snap = cpic_snapshot()?;
    assert!(snap["license"].as_str().is_some_and(|l| l.starts_with("CC0-1.0")));
    let pairs = snap["pairs"].as_array().cloned().unwrap_or_default();
    assert!(pairs.iter().all(|p| p["cpiclevel"] == "A"));
    let names: BTreeSet<&str> = snap["drugs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["name"].as_str())
        .collect();
    for drug in [
        "clopidogrel",
        "codeine",
        "tramadol",
        "simvastatin",
        "warfarin",
        "fluorouracil",
        "capecitabine",
        "azathioprine",
        "mercaptopurine",
        "thioguanine",
    ] {
        assert!(names.contains(drug), "{drug}");
    }
    let have: BTreeSet<(String, String)> = pairs
        .iter()
        .map(|p| (text(&p["drugname"]), text(&p["genesymbol"])))
        .collect();
    for (d, g) in [
        ("clopidogrel", "CYP2C19"),
        ("codeine", "CYP2D6"),
        ("tramadol", "CYP2D6"),
        ("simvastatin", "SLCO1B1"),
        ("warfarin", "CYP2C9"),
        ("warfarin", "VKORC1"),
        ("fluorouracil", "DPYD"),
        ("azathioprine", "TPMT"),
        ("azathioprine", "NUDT15"),
    ] {
        assert!(have.contains(&(d.to_string(), g.to_string())), "{d} {g}");
    }
    Ok(())
}

#[test]
fn a_count_of_one_reads_singular() -> R {
    let p = load_pack()?;
    assert_eq!(
        p.text("subtitle.wearables", "en", &[("n", P::I(1))])?,
        "wearables synced 1 day ago"
    );
    assert_eq!(
        p.text("subtitle.wearables", "es", &[("n", P::I(1))])?,
        "wearables sincronizados hace 1 día"
    );
    assert_eq!(
        p.text("subtitle.wearables", "en", &[("n", P::I(3))])?,
        "wearables synced 3 days ago"
    );
    assert_eq!(
        p.text("genome.summary.carrier", "es", &[("n", P::I(1))])?,
        "1 hallazgo de portador"
    );
    assert_eq!(
        p.text("genome.summary.carrier", "en", &[("n", P::I(2))])?,
        "2 carrier findings"
    );
    Ok(())
}

#[test]
fn an_unquoted_decimal_in_a_pack_file_is_a_pack_error() -> R {
    let mut files = shipped_files()?;
    if let Some(r) = files
        .get_mut("kidney.yaml")
        .and_then(|d| d.get_mut("rules"))
        .and_then(|r| r.get_mut(0))
    {
        r["cv_i"] = json!(4.5); // what `cv_i: 4.5` loads as
    }
    let err = build_pack(&files, true)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.starts_with("kidney.yaml: ") && err.contains("quoted"), "{err}");
    // and the YAML itself, unquoted, loads as a float and is refused the same way
    let dir = TempDir::new()?;
    std::fs::write(
        dir.0.join("kidney.yaml"),
        shipped("kidney.yaml").replace("cv_i: \"4.5\"", "cv_i: 4.5"),
    )?;
    let err = load(dir.0.to_str()).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err.starts_with("kidney.yaml: ") && err.contains("quoted"), "{err}");
    Ok(())
}

/// SHIPPED lists the pack directory by hand; Python globs it. A file added there must not load in
/// Python and be skipped here (it would change rules_version, the pack sha256 and state_hash).
#[test]
fn shipped_lists_every_yaml_in_the_pack_directory() -> R {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../data/rules/pack");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|n| n.ends_with(".yaml"))
        .collect();
    on_disk.sort();
    let listed: Vec<String> = SHIPPED.iter().map(|(n, _)| n.to_string()).collect();
    assert_eq!(
        listed,
        on_disk,
        "SHIPPED in src/pack.rs is out of step with {}",
        dir.display()
    );
    Ok(())
}
