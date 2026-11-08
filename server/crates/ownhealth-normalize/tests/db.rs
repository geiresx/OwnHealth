//! The database tests of `normalize/`: tests/normalize/test_norm_row.py, test_norm_rederive.py and
//! the household-alias cases of test_norm_resolve.py, with the same cases and expectations. Each test
//! runs in its own throwaway database (scripts/test-db.sh) inside one transaction, as the Python
//! `conn` fixture does.

use std::str::FromStr;

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use ownhealth_core::Error;
use ownhealth_core::access::create_household;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::observations::{CommitOptions, Committed, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::review::{RouteFact, route_fact};
use ownhealth_core::testing::TestDb;
use ownhealth_normalize::{
    AliasIn, Normalized, ResolveQuery, RowIn, add_alias, derivation_version, normalize_row, rederive, resolve_analyte,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

type R = anyhow::Result<()>;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn when() -> DateTime<FixedOffset> {
    Utc.with_ymd_and_hms(2026, 9, 14, 8, 0, 0)
        .single()
        .unwrap_or_else(|| panic!("date"))
        .fixed_offset()
}

async fn family(conn: &mut PgConnection) -> anyhow::Result<Family> {
    Ok(create_rivera_family(conn, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?)
}

/// A synthetic LabNorte report, one row per line: name, value, unit, range (es locale).
const REPORT: [(&str, &str, &str, &str); 16] = [
    ("Hemoglobina", "14,8", "g/dL", "13,5 - 17,5"),
    ("Plaquetas", "245", "x10^9/L", "150 - 400"),
    ("VCM", "91", "fL", "80 - 100"),
    ("Leucocitos", "6,8", "x10^3/µL", "4,0 - 11,0"),
    ("Glucosa", "92", "mg/dL", "70 - 110"),
    ("Hemoglobina glicada (HbA1c)", "5,6", "%", "4,0 - 5,6"),
    ("Creatinina", "0,98", "mg/dL", "0,70 - 1,20"),
    ("Filtrado glomerular (CKD-EPI 2021)", "92", "mL/min/1.73m2", "> 90"),
    ("TSH", "2,10", "µUI/mL", "0,40 - 4,00"),
    ("T4 libre", "1,2", "ng/dL", "0,90 - 1,70"),
    ("Colesterol LDL", "97", "mg/dL", "< 130"),
    ("Triglicéridos", "88", "mg/dL", "< 150"),
    ("Ferritina", "120", "ng/mL", "30 - 400"),
    ("Vitamina D (25-OH)", "24", "ng/mL", "30 - 100"),
    ("ALT (GPT)", "31", "U/L", "7 - 41"),
    ("Sodio", "141", "mmol/L", "136 - 145"),
];

/// `_row` with its keyword arguments.
#[derive(Default)]
struct Opt<'a> {
    flag: Option<&'a str>,
    lang: Option<&'a str>,
    provider: Option<&'a str>,
    loinc: Option<&'a str>,
    document_id: Option<Uuid>,
    profile: Option<&'a str>,
}

async fn row_with(
    c: &mut PgConnection,
    f: &Family,
    name: &str,
    value: &str,
    unit: Option<&str>,
    rng: Option<&str>,
    o: Opt<'_>,
) -> anyhow::Result<Normalized> {
    Ok(normalize_row(
        c,
        &RowIn {
            profile_id: f.profile(o.profile.unwrap_or("alex")),
            printed_name: name,
            printed_value: value,
            printed_unit: unit,
            printed_range: rng,
            printed_flag: o.flag,
            effective_at: when(),
            lang: o.lang.unwrap_or("es"),
            specimen: None,
            provider: o.provider,
            loinc: o.loinc,
            method: None,
            document_id: o.document_id,
        },
    )
    .await?)
}

async fn row(
    c: &mut PgConnection,
    f: &Family,
    name: &str,
    value: &str,
    unit: &str,
    rng: Option<&str>,
) -> anyhow::Result<Normalized> {
    row_with(c, f, name, value, Some(unit), rng, Opt::default()).await
}

fn failed(n: &Normalized) -> Vec<String> {
    let mut v: Vec<String> = n
        .checks
        .iter()
        .filter(|c| !c.ok)
        .filter_map(|c| c.reason.clone())
        .collect();
    v.sort();
    v
}

fn has(n: &Normalized, reason: &str) -> bool {
    failed(n).iter().any(|r| r == reason)
}

async fn commit(c: &mut PgConnection, n: &Normalized, document_id: Option<Uuid>) -> anyhow::Result<Committed> {
    let mut obs = n.observation.clone();
    if document_id.is_some() {
        obs.document_id = document_id;
    }
    Ok(commit_observation(c, &obs, &SrcRef::New(SrcIn::manual(None)), CommitOptions::new("person")).await?)
}

// --------------------------------------------------------------- test_norm_row ----

#[tokio::test]
async fn test_clean_rows_pass_every_check() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    for (name, value, unit, rng) in REPORT {
        let n = row(&mut tx, &f, name, value, unit, Some(rng)).await?;
        assert_eq!(failed(&n), Vec::<String>::new(), "{name}: {:?}", n.checks);
        let o = &n.observation;
        assert!(
            o.loinc.is_some() && o.value_canonical.is_some() && o.unit_canonical.is_some(),
            "{name}"
        );
        assert_eq!(o.derivation_version, derivation_version());
        // truth layer kept as printed
        assert!(o.printed_value == value && o.printed_unit.as_deref() == Some(unit));
    }
    Ok(())
}

#[tokio::test]
async fn test_row_values_are_derived() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let o = row(&mut tx, &f, "Colesterol LDL", "3,4", "mmol/L", Some("< 3,36"))
        .await?
        .observation;
    assert_eq!(
        (
            o.loinc.as_deref(),
            o.alias_group.as_str(),
            o.unit_ucum.as_deref(),
            o.unit_canonical.as_deref()
        ),
        (Some("39469-2"), "ldl_c", Some("mmol/L"), Some("mg/dL"))
    );
    assert_eq!(o.value_canonical.map(|v| v.round_dp(1)), Some(d("131.5")));
    assert_eq!(
        (o.value_num, o.ref_high, o.interpretation.as_deref()),
        (Some(d("3.4")), Some(d("3.36")), Some("H"))
    );
    let vit = row(&mut tx, &f, "Vitamina D (25-OH)", "24", "ng/mL", Some("30 - 100"))
        .await?
        .observation;
    assert_eq!(vit.interpretation.as_deref(), Some("L"));
    Ok(())
}

/// Unit from the next column: every shifted row fails at least one check, none would auto-accept.
/// Rows whose neighbour prints the same unit are not shifted in any visible way and are skipped.
#[tokio::test]
async fn test_column_shift_is_caught_on_every_row() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let (mut missed, mut shifted) = (Vec::new(), 0);
    for (i, (name, value, own, rng)) in REPORT.iter().enumerate() {
        let unit = REPORT[(i + 1) % REPORT.len()].2;
        if unit == *own {
            continue;
        }
        shifted += 1;
        let n = row(&mut tx, &f, name, value, unit, Some(rng)).await?;
        if failed(&n).is_empty() {
            missed.push((name, unit));
        }
    }
    assert!(shifted >= 12 && missed.is_empty(), "{shifted} {missed:?}");
    Ok(())
}

#[tokio::test]
async fn test_shift_to_another_dimension_is_unit_dimension_mismatch() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    for (name, value, unit) in [
        ("Hemoglobina glicada (HbA1c)", "5,6", "mg/dL"), // glucose's unit
        ("TSH", "2,10", "ng/dL"),                        // free T4's unit
        ("Creatinina", "0,98", "mL/min/1.73m2"),         // eGFR's unit
        ("Plaquetas", "245", "fL"),                      // MCV's unit
        ("Ferritina", "120", "U/L"),                     // ALT's unit
        ("Sodio", "141", "%"),
    ] {
        let n = row(&mut tx, &f, name, value, unit, None).await?;
        assert!(has(&n, "unit_dimension_mismatch"), "{name}: {:?}", failed(&n));
        assert!(n.observation.value_canonical.is_none() && n.observation.unit_canonical.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn test_shift_inside_the_same_dimension_is_caught_by_plausibility() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    // vitamin D's unit: also mass/volume
    let n = row(&mut tx, &f, "Colesterol LDL", "97", "ng/mL", Some("< 130")).await?;
    assert!(has(&n, "implausible"));
    Ok(())
}

#[tokio::test]
async fn test_unconvertible_unit() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    // canonical is nmol/L: never converted
    let n = row(&mut tx, &f, "Lipoproteína (a)", "45", "mg/dL", Some("< 30")).await?;
    assert!(has(&n, "unit_unconvertible"));
    assert!(n.observation.value_canonical.is_none() && n.observation.printed_value == "45");
    Ok(())
}

#[tokio::test]
async fn test_flag_mismatch() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let flag = |flag| Opt {
        flag: Some(flag),
        ..Default::default()
    };
    let n = row_with(&mut tx, &f, "Glucosa", "92", Some("mg/dL"), Some("70 - 110"), flag("H")).await?;
    assert!(has(&n, "flag_mismatch"));
    let n = row_with(
        &mut tx,
        &f,
        "Glucosa",
        "120",
        Some("mg/dL"),
        Some("70 - 110"),
        flag("H"),
    )
    .await?;
    assert!(failed(&n).is_empty());
    let hh = row_with(
        &mut tx,
        &f,
        "Potasio",
        "7,1",
        Some("mmol/L"),
        Some("3,5 - 5,1"),
        flag("HH"),
    )
    .await?;
    assert_eq!(hh.observation.interpretation.as_deref(), Some("HH")); // critical only because the lab printed it
    assert!(!has(&hh, "flag_mismatch")); // HH agrees with a value above the range
    let ll = row_with(
        &mut tx,
        &f,
        "Potasio",
        "2,1",
        Some("mmol/L"),
        Some("3,5 - 5,1"),
        flag("LL"),
    )
    .await?;
    assert!(ll.observation.interpretation.as_deref() == Some("LL") && !has(&ll, "flag_mismatch"));
    let n = row_with(
        &mut tx,
        &f,
        "Potasio",
        "4,0",
        Some("mmol/L"),
        Some("3,5 - 5,1"),
        flag("HH"),
    )
    .await?;
    assert!(has(&n, "flag_mismatch"));
    Ok(())
}

#[tokio::test]
async fn test_separator_and_factor_1000() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let n = row(&mut tx, &f, "Hierro", "162", "mg/dL", Some("60 - 160")).await?; // µg/dL misread as mg/dL
    assert!(
        has(&n, "implausible") && has(&n, "suspect_separator"),
        "{:?}",
        failed(&n)
    );
    let amb = row(&mut tx, &f, "Glucosa", "1.200", "mg/dL", None).await?;
    assert!(has(&amb, "ambiguous_separator"));
    Ok(())
}

#[tokio::test]
async fn test_value_is_range_and_range_parse() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let n = row(&mut tx, &f, "Glucosa", "70 - 110", "mg/dL", Some("70 - 110")).await?;
    assert!(has(&n, "value_is_range"));
    // a value on a bound is common and real (HbA1c 5.6 with 4.0 - 5.6): not a range
    let n = row(&mut tx, &f, "Glucosa", "110", "mg/dL", Some("70 - 110")).await?;
    assert!(failed(&n).is_empty());
    let n = row(&mut tx, &f, "Glucosa", "92", "mg/dL", Some("ver nota")).await?;
    assert!(has(&n, "range_parse"));
    let n = row(&mut tx, &f, "Glucosa", "92", "mg/dL", Some("110 - 70")).await?;
    assert!(has(&n, "range_parse"));
    Ok(())
}

#[tokio::test]
async fn test_sex_specific_range_uses_the_profile() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let rng = Some("Hombres: 13,5-17,5 Mujeres: 12,0-16,0");
    let alex = row(&mut tx, &f, "Hemoglobina", "13,0", "g/dL", rng).await?.observation;
    assert_eq!(
        (alex.ref_low, alex.interpretation.as_deref(), alex.ref_applies),
        (Some(d("13.5")), Some("L"), Some(json!({"sex": "male"})))
    );
    let sam = Opt {
        profile: Some("sam"),
        ..Default::default()
    };
    let sam = row_with(&mut tx, &f, "Hemoglobina", "13,0", Some("g/dL"), rng, sam)
        .await?
        .observation;
    assert_eq!(
        (sam.ref_low, sam.interpretation.as_deref()),
        (Some(d("12.0")), Some("N"))
    );
    Ok(())
}

#[tokio::test]
async fn test_unmapped_and_panel_rows() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let lab = Opt {
        provider: Some("LabNorte"),
        ..Default::default()
    };
    let n = row_with(&mut tx, &f, "Índice aterogénico", "3,1", Some(""), Some("< 4,5"), lab).await?;
    assert!(has(&n, "unmapped") && n.observation.alias_group == "indice_aterogenico@labnorte");
    assert!(n.observation.loinc.is_none());
    let p = row_with(&mut tx, &f, "Perfil lipídico", "", None, None, Opt::default()).await?;
    assert!(has(&p, "unmapped"));
    Ok(())
}

#[tokio::test]
async fn test_text_result() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let o = row_with(
        &mut tx,
        &f,
        "Nitritos",
        "Negativo",
        None,
        Some("Negativo"),
        Opt::default(),
    )
    .await?
    .observation;
    assert_eq!(
        (
            o.value_code.as_deref(),
            o.value_text.as_deref(),
            o.interpretation.as_deref(),
            o.loinc.as_deref()
        ),
        (Some("negative"), Some("Negativo"), Some("N"), Some("5802-4"))
    );
    let pos = row_with(
        &mut tx,
        &f,
        "Nitritos",
        "Positivo",
        None,
        Some("Negativo"),
        Opt::default(),
    )
    .await?;
    assert!(pos.observation.interpretation.as_deref() == Some("A") && failed(&pos).is_empty());
    Ok(())
}

#[tokio::test]
async fn test_structured_code_is_kept_and_checked() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let coded = |loinc| Opt {
        loinc: Some(loinc),
        lang: Some("en"),
        ..Default::default()
    };
    let ok = row_with(
        &mut tx,
        &f,
        "LDL Chol Calc (NIH)",
        "97",
        Some("mg/dL"),
        None,
        coded("13457-7"),
    )
    .await?;
    assert!(
        ok.observation.loinc.as_deref() == Some("13457-7") && failed(&ok).is_empty(),
        "{:?}",
        ok.checks
    );
    let bad = row_with(&mut tx, &f, "LDL", "97", Some("mg/dL"), None, coded("99999-9")).await?;
    assert!(has(&bad, "loinc_unknown"));
    let shifted = row_with(&mut tx, &f, "LDL", "2.5", Some("mmol/mol"), None, coded("13457-7")).await?;
    assert!(has(&shifted, "unit_dimension_mismatch"));
    Ok(())
}

#[tokio::test]
async fn test_duplicate_and_conflict() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let first = row(&mut tx, &f, "Colesterol LDL", "97", "mg/dL", Some("< 130")).await?;
    commit(&mut tx, &first, None).await?;
    // 2.51 mmol/L is 97.06 mg/dL: not equal
    let same = row(&mut tx, &f, "Colesterol LDL", "2,51", "mmol/L", Some("< 3,36")).await?;
    assert!(has(&same, "conflict"));
    let en = Opt {
        lang: Some("en"),
        ..Default::default()
    };
    let dup = row_with(&mut tx, &f, "LDL-C", "97", Some("mg/dL"), Some("<130"), en).await?;
    assert!(has(&dup, "duplicate"));
    Ok(())
}

#[tokio::test]
async fn test_clean_row_auto_accepts_through_route_fact() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let src = SrcIn::manual(None);
    let n = row(&mut tx, &f, "Colesterol LDL", "97", "mg/dL", Some("< 130")).await?;
    let res = route_fact(
        &mut tx,
        RouteFact::new(&n.observation, &src, &n.checks, 0.99, "structured_valid", "matched"),
    )
    .await?;
    assert_eq!(res.state, "auto_accepted");
    let shifted = row(
        &mut tx,
        &f,
        "Hemoglobina glicada (HbA1c)",
        "5,6",
        "mg/dL",
        Some("4,0 - 5,6"),
    )
    .await?;
    let res = route_fact(
        &mut tx,
        RouteFact::new(
            &shifted.observation,
            &src,
            &shifted.checks,
            0.99,
            "structured_valid",
            "matched",
        ),
    )
    .await?;
    assert!(res.state == "in_review" && res.reasons.iter().any(|r| r == "unit_dimension_mismatch"));
    Ok(())
}

#[tokio::test]
async fn test_a_partial_name_and_a_text_on_a_quantity_go_to_review() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let part = row(&mut tx, &f, "Cortisol (tarde)", "12", "ug/dL", Some("5 - 25")).await?;
    assert!(has(&part, "unmapped"));
    // serum glucose has no ordinal code
    let neg = row_with(&mut tx, &f, "Glucosa", "Negativo", None, None, Opt::default()).await?;
    assert!(has(&neg, "implausible"));
    let strip = row_with(
        &mut tx,
        &f,
        "Proteínas en orina",
        "Negativo",
        None,
        Some("Negativo"),
        Opt::default(),
    )
    .await?;
    assert!(failed(&strip).is_empty() && strip.observation.loinc.as_deref() == Some("20454-5"));
    Ok(())
}

/// A direct and a Friedewald LDL never merge; `<5` beside `5` is a conflict, not one fact.
#[tokio::test]
async fn test_duplicate_needs_the_same_method_and_comparator() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let calc = row(&mut tx, &f, "Colesterol LDL (calculado)", "97", "mg/dL", Some("< 130")).await?;
    commit(&mut tx, &calc, None).await?;
    let direct = row(&mut tx, &f, "Colesterol LDL directo", "97", "mg/dL", Some("< 130")).await?;
    assert_eq!(direct.observation.method.as_deref(), Some("direct"));
    assert!(!has(&direct, "duplicate") && !has(&direct, "conflict"));
    let pcr = row(&mut tx, &f, "PCR", "5", "mg/L", Some("< 5")).await?;
    commit(&mut tx, &pcr, None).await?;
    let censored = row(&mut tx, &f, "PCR", "<5", "mg/L", Some("< 5")).await?;
    assert!(has(&censored, "conflict") && !has(&censored, "duplicate"));
    Ok(())
}

async fn document(c: &mut PgConnection, f: &Family, data: u8) -> anyhow::Result<Uuid> {
    let sha = format!("{data:064x}");
    let mut doc = NewDocument::upload(f.household_id, Some(f.profile("alex")), &sha, "application/pdf", 1);
    doc.filename = Some("r.pdf");
    Ok(create_document(c, &doc).await?.id)
}

#[tokio::test]
async fn test_rows_of_one_source_are_never_duplicates() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let doc = document(&mut tx, &f, b'1').await?;
    let first = row(&mut tx, &f, "Glucosa", "92", "mg/dL", Some("70 - 110")).await?;
    commit(&mut tx, &first, Some(doc)).await?;
    let same_doc = Opt {
        document_id: Some(doc),
        ..Default::default()
    };
    let again = row_with(&mut tx, &f, "Glucosa", "92", Some("mg/dL"), Some("70 - 110"), same_doc).await?;
    assert!(!has(&again, "duplicate"));
    let other_doc = Opt {
        document_id: Some(document(&mut tx, &f, b'2').await?),
        ..Default::default()
    };
    let other = row_with(&mut tx, &f, "Glucosa", "92", Some("mg/dL"), Some("70 - 110"), other_doc).await?;
    assert!(has(&other, "duplicate"));
    Ok(())
}

// ------------------------------------------- test_norm_resolve (household aliases) ----

async fn resolve(
    c: &mut PgConnection,
    name: &str,
    unit: &str,
    household_id: Option<Uuid>,
) -> anyhow::Result<(String, Option<String>)> {
    let q = ResolveQuery {
        lang: "es",
        unit: Some(unit),
        household_id,
        ..Default::default()
    };
    let r = resolve_analyte(Some(c), name, &q).await?;
    Ok((r.alias_group, r.loinc))
}

fn alias<'a>(household_id: Uuid, alias: &'a str, alias_group: &'a str, loinc: Option<&'a str>) -> AliasIn<'a> {
    AliasIn {
        household_id,
        alias,
        alias_group,
        lang: Some("es"),
        loinc,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_household_alias_resolves_only_in_its_household() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let hh = Some(f.household_id);
    assert_eq!(resolve(&mut tx, "Coles LDL hoja", "mg/dL", hh).await?.1, None);
    add_alias(&mut tx, &alias(f.household_id, "Coles LDL hoja", "ldl_c", None)).await?;
    // no method named: the method-agnostic code
    assert_eq!(
        resolve(&mut tx, "Coles LDL hoja", "mg/dL", hh).await?,
        ("ldl_c".into(), Some("2089-1".into()))
    );
    add_alias(
        &mut tx,
        &alias(f.household_id, "Coles LDL hoja", "ldl_c", Some("13457-7")),
    )
    .await?;
    // the alias code says calculated; the unit says molar
    assert_eq!(
        resolve(&mut tx, "Coles LDL hoja", "mmol/L", hh).await?,
        ("ldl_c".into(), Some("39469-2".into()))
    );
    assert_eq!(resolve(&mut tx, "Coles LDL hoja", "mg/dL", None).await?.1, None);
    let stranger = create_household(&mut tx, "Ortega (other household)").await?;
    assert_eq!(
        resolve(&mut tx, "Coles LDL hoja", "mg/dL", Some(stranger)).await?.1,
        None
    );
    Ok(())
}

#[tokio::test]
async fn test_add_alias_validates() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let hh = f.household_id;
    for a in [
        alias(hh, "x", "no_such_group", None),
        alias(hh, "Perfil lipídico", "ldl_c", None),
        alias(hh, "x", "ldl_c", Some("2345-7")),
    ] {
        let e = add_alias(&mut tx, &a).await;
        assert!(matches!(e, Err(Error::Invalid(_))), "{}: {e:?}", a.alias);
    }
    Ok(())
}

// ---------------------------------------------------------- test_norm_rederive ----

async fn commit_row(
    c: &mut PgConnection,
    f: &Family,
    name: &str,
    value: &str,
    unit: &str,
    rng: &str,
) -> anyhow::Result<Committed> {
    let n = row(c, f, name, value, unit, Some(rng)).await?;
    commit(c, &n, None).await
}

async fn observation(c: &mut PgConnection, sql: &str, id: Uuid) -> anyhow::Result<sqlx::postgres::PgRow> {
    Ok(sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id).fetch_one(c).await?)
}

#[tokio::test]
async fn test_new_alias_relinks_history_once() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let alex = f.profile("alex");
    let old = commit_row(&mut tx, &f, "Coles LDL calc.", "97", "mg/dL", "< 130").await?;
    let keep = commit_row(&mut tx, &f, "Ferritina", "120", "ng/mL", "30 - 400").await?;
    let r = observation(
        &mut tx,
        "select alias_group, loinc from observation where id = $1",
        old.id,
    )
    .await?;
    assert_eq!(r.try_get::<Option<String>, _>("loinc")?, None);
    assert_eq!(r.try_get::<String, _>("alias_group")?, "coles_ldl_calc");
    assert_eq!(rederive(&mut tx, Some(alex), None).await?, 0); // nothing changed yet

    add_alias(
        &mut tx,
        &alias(f.household_id, "Coles LDL calc.", "ldl_c", Some("13457-7")),
    )
    .await?;
    assert_eq!(rederive(&mut tx, Some(alex), None).await?, 1);

    let cols = "status, printed_name, printed_value, printed_unit, printed_ref_range, value_num, src_id, accepted_by, \
                effective_at, alias_group, loinc, value_canonical, derivation_version";
    let before = observation(
        &mut tx,
        &format!("select {cols} from observation where id = $1"),
        old.id,
    )
    .await?;
    let after = observation(
        &mut tx,
        &format!("select {cols} from observation where supersedes = $1"),
        old.id,
    )
    .await?;
    assert_eq!(before.try_get::<String, _>("status")?, "superseded");
    assert_eq!(after.try_get::<String, _>("status")?, "current");
    assert_eq!(after.try_get::<String, _>("alias_group")?, "ldl_c");
    assert_eq!(after.try_get::<Option<String>, _>("loinc")?.as_deref(), Some("13457-7"));
    assert_eq!(after.try_get::<Option<Decimal>, _>("value_canonical")?, Some(d("97")));
    for col in ["printed_name", "printed_value", "src_id", "accepted_by"] {
        assert_eq!(
            after.try_get::<String, _>(col)?,
            before.try_get::<String, _>(col)?,
            "{col}"
        );
    }
    for col in ["printed_unit", "printed_ref_range"] {
        assert_eq!(
            after.try_get::<Option<String>, _>(col)?,
            before.try_get::<Option<String>, _>(col)?,
            "{col}"
        );
    }
    assert_eq!(
        after.try_get::<Option<Decimal>, _>("value_num")?,
        before.try_get::<Option<Decimal>, _>("value_num")?
    );
    assert_eq!(
        after.try_get::<DateTime<Utc>, _>("effective_at")?,
        before.try_get::<DateTime<Utc>, _>("effective_at")?
    );
    assert_eq!(after.try_get::<String, _>("derivation_version")?, derivation_version());
    let kept = observation(&mut tx, "select status from observation where id = $1", keep.id).await?;
    assert_eq!(kept.try_get::<String, _>("status")?, "current");

    let payload: Value =
        sqlx::query_scalar("select payload from review_item where kind = 'rederive_diff' and profile_id = $1")
            .bind(alex)
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(payload["rows"][0]["changed"]["loinc"], json!([null, "13457-7"]));
    assert_eq!(payload["derivation_version"], json!(derivation_version()));
    let jobs: i64 =
        sqlx::query_scalar("select count(*) from job where kind = 'state.rebuild' and payload->>'profile_id' = $1")
            .bind(alex.to_string())
            .fetch_one(&mut *tx)
            .await?;
    assert_eq!(jobs, 1);
    assert_eq!(rederive(&mut tx, Some(alex), None).await?, 0); // idempotent
    Ok(())
}

/// A row written with an older normalizer (no canonical value) gets one.
#[tokio::test]
async fn test_rederive_fills_a_stale_derived_layer() -> R {
    let db = TestDb::new().await?;
    let mut tx = db.pool.begin().await?;
    let f = family(&mut tx).await?;
    let alex = f.profile("alex");
    let o = ObservationIn::from_json(json!({
        "profile_id": alex,
        "printed_name": "Colesterol LDL",
        "printed_value": "3,4",
        "printed_unit": "mmol/L",
        "value_num": "3.4",
        "alias_group": "ldl_c",
        "derivation_version": "test",
        "effective_at": "2026-09-14T08:00:00+00:00",
    }))?;
    let c = commit_observation(
        &mut tx,
        &o,
        &SrcRef::New(SrcIn::manual(None)),
        CommitOptions::new("person"),
    )
    .await?;
    assert_eq!(rederive(&mut tx, Some(alex), None).await?, 1);
    let new = observation(
        &mut tx,
        "select loinc, unit_ucum, unit_canonical, value_canonical from observation where supersedes = $1",
        c.id,
    )
    .await?;
    assert_eq!(
        (
            new.try_get::<Option<String>, _>("loinc")?,
            new.try_get::<Option<String>, _>("unit_ucum")?,
            new.try_get::<Option<String>, _>("unit_canonical")?
        ),
        (Some("39469-2".into()), Some("mmol/L".into()), Some("mg/dL".into()))
    );
    let v: Option<Decimal> = new.try_get("value_canonical")?;
    assert_eq!(v.map(|v| v.round_dp(1)), Some(d("131.5")));
    Ok(())
}

/// The job runs rederive for one profile in its own transaction and reports the count.
#[tokio::test]
async fn test_rederive_job() -> R {
    let db = TestDb::new().await?;
    let mut c = db.pool.acquire().await?;
    let f = family(&mut c).await?;
    commit_row(&mut c, &f, "Coles LDL calc.", "97", "mg/dL", "< 130").await?;
    add_alias(
        &mut c,
        &alias(f.household_id, "Coles LDL calc.", "ldl_c", Some("13457-7")),
    )
    .await?;
    drop(c);
    ownhealth_normalize::register();
    let handler =
        ownhealth_core::jobs::handler_for("normalize.rederive").ok_or_else(|| anyhow::anyhow!("no handler"))?;
    let dir = std::env::temp_dir().join(format!("oh-norm-{}", ownhealth_core::ids::token_hex(4)));
    let settings = std::sync::Arc::new(db.settings(&dir)?);
    let ctx = ownhealth_core::jobs::JobContext::new(
        db.pool.clone(),
        settings,
        ownhealth_core::blobs::BlobStore::new(dir.join("blobs"))?,
    );
    let job = |payload: Value| ownhealth_core::jobs::Job {
        id: 1,
        queue: "core".into(),
        kind: "normalize.rederive".into(),
        payload,
        attempts: 0,
        max_attempts: 5,
        dedupe_key: None,
    };
    let out = handler(ctx.clone(), job(json!({"profile_id": f.profile("alex").to_string()}))).await;
    assert_eq!(out.map_err(|e| anyhow::anyhow!("{e}"))?, Some(json!({"changed": 1})));
    let out = handler(ctx.clone(), job(json!({}))).await;
    assert_eq!(out.map_err(|e| anyhow::anyhow!("{e}"))?, Some(json!({"changed": 0})));
    let bad = handler(ctx, job(json!({"profile_id": "nope"}))).await;
    assert!(bad.is_err());
    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
