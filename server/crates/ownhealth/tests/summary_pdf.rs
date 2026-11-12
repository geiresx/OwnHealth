//! The printed doctor summary (`ownhealth_rules::summary` + `ownhealth_ops::summary_pdf`), read back
//! the way a reader would (PDFium): the Rivera family and a profile with five years of monthly labs,
//! for every specialty in es and en, fit on two A4 pages in under 5 s; every line that prints a value
//! names its src id; two renders give the same bytes; the text layer is information, not advice.

#[path = "../../ownhealth-rules/tests/common/mod.rs"]
mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::time::Instant;

use chrono::{NaiveDate, TimeZone, Utc};
use common::{Seeded, as_of, seeded};
use ownhealth_core::access::{ProfileIn, Scopes, all_scopes, create_household, create_profile, scope_set};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, create_src};
use ownhealth_ingest_pdf::textcheck::assert_text_layer;
use ownhealth_ingest_pdf::textlayer::read_pdf;
use ownhealth_ops::summary_pdf::{MAX_PAGES, render};
use ownhealth_rules::summary::{self, LANGS, Options, SPECIALTIES};
use ownhealth_rules::wording::{SKIP_KEYS, imperative_phrases, lint};
use ownhealth_rules::{cited, load_pack};
use serde_json::{Value, json};
use uuid::Uuid;

type R = anyhow::Result<()>;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/summary");
const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];
const BUDGET_S: f64 = 5.0;

fn opts(specialty: &str, lang: &str) -> Options {
    Options {
        lang: lang.into(),
        specialty: specialty.into(),
        sensitive_open: false,
        as_of: as_of(),
    }
}

async fn model(s: &Seeded, pid: Uuid, scopes: &Scopes, o: &Options) -> anyhow::Result<Value> {
    let mut tx = s.pool().begin().await?;
    let m = summary::build(&mut tx, pid, scopes, o).await?;
    tx.commit().await?;
    Ok(m)
}

/// Every row that prints a lab value or a ranked flag names its own src id on its first line: a
/// marker row starts with the series name, a flag row with its rank. Every line is information.
fn src_check(model: &Value) -> impl FnMut(&str) -> Result<(), String> + use<> {
    let mut rows: Vec<(String, Vec<String>)> = Vec::new();
    for m in model["markers"].as_array().into_iter().flatten() {
        if m.get("not_measured").is_none() {
            let mut ids = vec![m["src"].as_str().unwrap_or("src_missing").to_string()];
            ids.extend(m["previous"]["src"].as_str().map(str::to_string));
            rows.push((format!("{} ", m["name"].as_str().unwrap_or_default()), ids));
        }
    }
    for f in model["flags"].as_array().into_iter().flatten() {
        let first = f["src"][0].as_str().unwrap_or("src_missing").to_string();
        rows.push((format!("{} ", f["rank"]), vec![first]));
    }
    move |line: &str| {
        for (start, ids) in &rows {
            if line.starts_with(start.as_str()) && ids.iter().any(|id| !line.contains(id.as_str())) {
                return Err(format!("a value without its src id ({ids:?})"));
            }
        }
        if let Some(hit) = imperative_phrases(line).first() {
            return Err(format!("reads as advice: {hit}"));
        }
        Ok(())
    }
}

fn pages(pdf: &[u8]) -> usize {
    read_pdf(pdf).map_or(0, |m| m.pages.len())
}

/// Print one model and hold it to every rule; returns the text lines.
fn check_sheet(model: &Value, what: &str) -> Vec<String> {
    let sheet = render(model);
    assert!(sheet.pages <= MAX_PAGES, "{what}: {} pages", sheet.pages);
    assert_eq!(
        pages(&sheet.pdf),
        sheet.pages,
        "{what}: PDFium reads another page count"
    );
    assert_eq!(render(model).pdf, sheet.pdf, "{what}: two renders differ");
    let lines = assert_text_layer(&sheet.pdf, src_check(model));
    let footer = format!("1/{}", sheet.pages);
    assert!(
        lines.iter().any(|l| l.contains(&footer)),
        "{what}: no footer {footer}: {lines:#?}"
    );
    let hash = model["state_hash"].as_str().unwrap_or_default();
    let prefix: String = hash.split_once(':').map_or(hash, |(_, h)| h).chars().take(12).collect();
    assert!(
        lines.iter().any(|l| l.contains(&prefix)),
        "{what}: the footer has no state hash"
    );
    lines
}

#[test]
fn the_data_files_load_through_the_cited_loader() -> R {
    let mut versions = Vec::new();
    for (name, _) in summary::FILES {
        let f = cited::load(&Path::new(DATA).join(name))?;
        versions.push(f.version);
    }
    assert_eq!(versions.join(", "), summary::version()?);
    let spec = cited::load(&Path::new(DATA).join("specialties.yaml"))?;
    let ids: Vec<&str> = spec.rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids, SPECIALTIES);
    Ok(())
}

/// Status words come from the engine only: every series a specialty lists is a marker of a pack system.
#[test]
fn every_specialty_series_is_a_pack_marker() -> R {
    let pack = load_pack()?;
    let markers: Vec<String> = pack
        .systems
        .iter()
        .flat_map(|s| s.markers.iter().map(ownhealth_rules::pack::System::marker_key))
        .collect();
    for (id, list) in summary::specialty_markers()? {
        assert!(!list.is_empty(), "{id}");
        for g in list {
            assert!(pack.analytes.contains_key(&g), "{id}: {g} is not an analyte");
            assert!(markers.contains(&g), "{id}: {g} is in no system");
        }
    }
    Ok(())
}

#[tokio::test]
async fn the_rivera_family_prints_on_two_pages_with_every_value_cited() -> R {
    let s = seeded().await?;
    let scopes = all_scopes();
    for slug in SLUGS {
        for specialty in SPECIALTIES {
            for lang in LANGS {
                let what = format!("{slug} {specialty} {lang}");
                let t = Instant::now();
                let m = model(&s, s.pid(slug), &scopes, &opts(specialty, lang)).await?;
                render(&m);
                let took = t.elapsed().as_secs_f64();
                assert!(took < BUDGET_S, "{what}: {took:.2} s");
                let lines = check_sheet(&m, &what);
                assert_eq!(lint(&m, &SKIP_KEYS), Vec::new(), "{what}");
                // every marker either prints a value with its range, date and src, or reads "not measured"
                for r in m["markers"].as_array().into_iter().flatten() {
                    if r.get("not_measured").is_some() {
                        assert!(r.get("value").is_none(), "{what}");
                        continue;
                    }
                    for k in ["value", "date", "src"] {
                        assert!(r[k].is_string(), "{what}: {k} of {r}");
                    }
                    assert!(r.get("range").is_some(), "{what}: range of {r}");
                    if r["previous"].is_object() {
                        assert!(r["previous"]["src"].is_string(), "{what}: previous src of {r}");
                    }
                }
                let gap = if lang == "es" { "no medido" } else { "not measured" };
                let gaps = m["markers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r.get("not_measured").is_some());
                if gaps.count() > 0 {
                    assert!(lines.iter().any(|l| l.contains(gap)), "{what}: no gap line");
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn alex_shows_values_ranges_previous_values_and_flags() -> R {
    let s = seeded().await?;
    let m = model(&s, s.pid("alex"), &all_scopes(), &opts("cardiology", "es")).await?;
    let ldl = m["markers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["group"] == "ldl_c")
        .cloned()
        .unwrap_or(Value::Null);
    assert!(ldl["previous"].is_object(), "{ldl}");
    assert!(m["flags"].as_array().is_some_and(|f| !f.is_empty()), "{m}");
    assert_eq!(m["slots"], json!({"calculators": [], "due": [], "family": []}));
    let lines = check_sheet(&m, "alex cardiology es");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("Resumen para el médico · Cardiología")),
        "{lines:#?}"
    );
    // the appendix maps each printed id to its document, page and date
    let cited = summary::cited_srcs(&m);
    for id in &cited {
        assert!(lines.iter().any(|l| l.contains(id.as_str())), "{id} not printed");
    }
    assert!(lines.iter().any(|l| l.contains("LabNorte · ")), "{lines:#?}");
    Ok(())
}

/// The appendix names a hand-entered source in words with its date, and lists documents newest first
/// with the pages of one report together; a Spanish sheet writes its lab ranges with a decimal comma.
#[tokio::test]
async fn the_appendix_reads_in_words_by_date_and_ranges_use_the_sheets_decimals() -> R {
    let s = seeded().await?;
    for (lang, by_hand, sep) in [("es", "introducido a mano · ", ','), ("en", "entered by hand · ", '.')] {
        let m = model(&s, s.pid("alex"), &all_scopes(), &opts("hematology", lang)).await?;
        let lines = check_sheet(&m, lang);
        let at = lines
            .iter()
            .rposition(|l| l == "Fuentes" || l == "Sources")
            .ok_or_else(|| anyhow::anyhow!("no appendix"))?;
        let appendix = &lines[at..];
        assert!(!appendix.iter().any(|l| l.starts_with("manual ")), "{appendix:#?}");
        assert!(appendix.iter().any(|l| l.starts_with(by_hand)), "{lang}: {appendix:#?}");
        // LabNorte · 2026-09-14 · p1 ... : dates never go up, pages go up within a date
        let docs: Vec<(String, String)> = appendix
            .iter()
            .filter_map(|l| l.strip_prefix("LabNorte · "))
            .filter_map(|l| {
                let mut p = l.split(" · ");
                Some((p.next()?.to_string(), p.next()?.split(' ').next()?.to_string()))
            })
            .collect();
        assert!(docs.len() > 1, "{appendix:#?}");
        for w in docs.windows(2) {
            assert!(
                w[0].0 > w[1].0 || (w[0].0 == w[1].0 && w[0].1 < w[1].1),
                "{lang}: {docs:?}"
            );
        }
        let other = if sep == ',' { '.' } else { ',' };
        let mut ranges = 0;
        for r in m["markers"].as_array().into_iter().flatten() {
            let Some(range) = r["range"].as_str() else { continue };
            let c: Vec<char> = range.chars().collect();
            let mixed = c
                .windows(3)
                .any(|w| w[0].is_ascii_digit() && w[1] == other && w[2].is_ascii_digit());
            assert!(!mixed, "{lang}: {} range {range}", r["name"]);
            ranges += 1;
        }
        assert!(ranges > 0, "{lang}: no ranges");
    }
    Ok(())
}

/// A sheet dated in the past leaves out an allergy recorded as starting after that date.
#[tokio::test]
async fn an_allergy_that_starts_after_the_print_date_is_left_out() -> R {
    let s = seeded().await?;
    let names = |m: &Value| -> Vec<String> {
        m["allergies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["substance"].as_str().map(str::to_string))
            .collect()
    };
    let before = model(&s, s.pid("sam"), &all_scopes(), &opts("gp", "en")).await?;
    let first = names(&before)
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("sam has no allergy"))?;
    // a fixture edit, past the trigger that keeps clinical rows immutable
    let mut tx = s.pool().begin().await?;
    sqlx::query("set local session_replication_role = replica")
        .execute(&mut *tx)
        .await?;
    sqlx::query("update allergy set effective_at = $3 where profile_id = $1 and substance = $2")
        .bind(s.pid("sam"))
        .bind(&first)
        .bind(
            (as_of() + chrono::Duration::days(1))
                .and_hms_opt(9, 0, 0)
                .map(|t| t.and_utc()),
        )
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let after = model(&s, s.pid("sam"), &all_scopes(), &opts("gp", "en")).await?;
    assert!(!names(&after).contains(&first), "{first} printed before it started");
    assert_eq!(names(&after).len() + 1, names(&before).len());
    Ok(())
}

/// The check above is not one that cannot fail: a marker printed without its src id is refused.
#[tokio::test]
async fn a_value_without_its_src_fails_the_check() -> R {
    let s = seeded().await?;
    let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", "en")).await?;
    let clean = m.clone();
    let row = m["markers"]
        .as_array_mut()
        .and_then(|a| {
            a.iter_mut()
                .find(|r| r.get("not_measured").is_none() && r["flag_rank"].is_null())
        })
        .ok_or_else(|| anyhow::anyhow!("no unflagged measured marker"))?;
    row["src"] = Value::Null;
    let pdf = render(&m).pdf;
    let refused = catch_unwind(AssertUnwindSafe(|| assert_text_layer(&pdf, src_check(&clean))));
    assert!(refused.is_err(), "a value printed without its src id passed");
    Ok(())
}

/// The PDF font is WinAnsi: a name in Chinese or Arabic letters cannot print, and each such letter
/// shows as '?'. The sheet says so beside the name, so the paper never shows a name that looks whole.
#[tokio::test]
async fn a_name_the_font_cannot_print_is_marked_beside_it() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        let says = if lang == "es" {
            "esta hoja no puede imprimir"
        } else {
            "this sheet cannot print"
        };
        let lines = check_sheet(&m, "latin name");
        assert!(!lines.iter().any(|l| l.contains(says)), "{lines:#?}");
        m["person"]["name"] = json!("李明 Li Ming");
        let lines = check_sheet(&m, "chinese name");
        let at = lines
            .iter()
            .position(|l| l.contains("?? Li Ming"))
            .ok_or_else(|| anyhow::anyhow!("no name line: {lines:#?}"))?;
        assert!(lines[at + 1].contains(says), "{lines:#?}");
    }
    Ok(())
}

#[tokio::test]
async fn a_viewer_without_lab_records_or_genome_scopes_gets_them_withheld() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let m = model(&s, s.pid("alex"), &scope_set(["read:summary"]), &opts("gp", lang)).await?;
        assert_eq!(m["withheld"]["by_scope"], json!(["labs", "records", "genome"]));
        for k in [
            "markers",
            "flags",
            "conditions",
            "medications",
            "allergies",
            "vaccines",
            "genome",
        ] {
            assert_eq!(m[k], json!([]), "{k}");
        }
        let lines = check_sheet(&m, "viewer");
        let says = if lang == "es" {
            "esta cuenta no puede leer"
        } else {
            "this account cannot read"
        };
        assert_eq!(lines.iter().filter(|l| l.contains(says)).count(), 3, "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("mg/dL")), "{lines:#?}");
    }
    Ok(())
}

/// Carmen Ortega with five years of monthly draws of 40 series, the newest all above range.
#[tokio::test]
async fn five_years_of_labs_print_on_two_pages_in_time() -> R {
    let s = seeded().await?;
    let pid = five_year_profile(&s).await?;
    for specialty in SPECIALTIES {
        for lang in LANGS {
            let what = format!("five years {specialty} {lang}");
            let t = Instant::now();
            let m = model(&s, pid, &all_scopes(), &opts(specialty, lang)).await?;
            render(&m);
            let took = t.elapsed().as_secs_f64();
            assert!(took < BUDGET_S, "{what}: {took:.2} s");
            assert!(
                m["flags"].as_array().map_or(0, Vec::len) >= 15,
                "{what}: the fixture must flag"
            );
            let lines = check_sheet(&m, &what);
            assert!(lines.iter().any(|l| l.starts_with("1 ")), "{what}: flag 1 is missing");
        }
    }
    Ok(())
}

/// More than two pages of content: rows are cut from the bottom of the ranking, the cut section
/// says how many more the record holds, and the top of the ranking stays.
#[tokio::test]
async fn an_overflow_is_cut_by_rank_with_a_count_of_what_is_left_out() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        let flags = m["flags"].as_array().cloned().unwrap_or_default();
        let first = flags
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("alex has no flags"))?;
        let many: Vec<Value> = (1..=150)
            .map(|rank| {
                let mut f = flags[(rank - 1) % flags.len()].clone();
                f["rank"] = json!(rank);
                f
            })
            .collect();
        m["flags"] = json!(many);
        let sheet = render(&m);
        assert_eq!(sheet.pages, MAX_PAGES, "{lang}");
        assert!(sheet.cut > 0, "{lang}: nothing was cut");
        let lines = check_sheet(&m, lang);
        let more = if lang == "es" {
            "más en el registro"
        } else {
            "more in the record"
        };
        assert!(lines.iter().any(|l| l.contains(more)), "{lang}: {lines:#?}");
        let top = format!("1 {}", first["text"].as_str().unwrap_or_default());
        let top: String = top.chars().take(30).collect();
        assert!(
            lines.iter().any(|l| l.starts_with(&top)),
            "{lang}: flag 1 was cut: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("150 ")),
            "{lang}: the last flag printed"
        );
    }
    Ok(())
}

/// Conditions, allergies and medicines alone can fill more than two pages: their tails are cut too,
/// each with its count, and the src ids the sheet reports (for the audit row) are exactly the ones
/// on the paper, never those of a cut row.
#[tokio::test]
async fn long_record_lists_are_cut_too_and_only_printed_ids_are_reported() -> R {
    let s = seeded().await?;
    for lang in LANGS {
        let mut m = model(&s, s.pid("alex"), &all_scopes(), &opts("gp", lang)).await?;
        let rows = |kind: &str, n: usize| -> Vec<Value> {
            (0..n)
                .map(|i| match kind {
                    "medications" => json!({"name": format!("Medicine {i:03}"), "schedule": "once a day",
                        "since": "2024-01-01", "src": format!("src_fake_med_{i:03}")}),
                    "allergies" => json!({"substance": format!("Substance {i:03}"), "criticality": "low",
                        "reaction": "rash", "src": format!("src_fake_alg_{i:03}")}),
                    _ => json!({"name": format!("Condition {i:03}"), "since": "2020-01-01",
                        "src": format!("src_fake_cnd_{i:03}")}),
                })
                .collect()
        };
        m["medications"] = json!(rows("medications", 130));
        m["allergies"] = json!(rows("allergies", 60));
        m["conditions"] = json!(rows("conditions", 60));
        let sheet = render(&m);
        assert!(sheet.pages <= MAX_PAGES, "{lang}: {} pages", sheet.pages);
        let lines = check_sheet(&m, lang);
        let more = if lang == "es" {
            "más en el registro"
        } else {
            "more in the record"
        };
        assert!(lines.iter().any(|l| l.contains(more)), "{lang}: {lines:#?}");
        let on_paper: Vec<&str> = lines
            .iter()
            .flat_map(|l| l.split_whitespace())
            .filter(|w| w.starts_with("src_"))
            .collect();
        for id in &sheet.printed {
            assert!(on_paper.contains(&id.as_str()), "{lang}: {id} reported but not printed");
        }
        for id in &on_paper {
            assert!(
                sheet.printed.iter().any(|p| p == id),
                "{lang}: {id} printed but not reported"
            );
        }
        assert!(
            !sheet.printed.iter().any(|p| p == "src_fake_med_129"),
            "{lang}: a cut medicine's src is reported"
        );
    }
    Ok(())
}

async fn five_year_profile(s: &Seeded) -> anyhow::Result<Uuid> {
    let mut groups: Vec<(String, Option<String>)> = load_pack()?
        .analytes
        .iter()
        .map(|(g, a)| (g.clone(), a["loinc"].get(0).map(ownhealth_rules::pack::text)))
        .collect();
    let n = groups.len();
    groups.extend((0..40 - n).map(|i| (format!("extra_{i:02}"), None)));
    let mut tx = s.pool().begin().await?;
    let h = create_household(&mut tx, "Ortega (summary)").await?;
    let pid = create_profile(
        &mut tx,
        &ProfileIn {
            household_id: h,
            slug: "carmen5".into(),
            display_name: "Carmen Ortega".into(),
            relationship: Some("self".into()),
            birth_date: NaiveDate::from_ymd_opt(1955, 1, 30),
            sex_at_birth: Some("female".into()),
            country: Some("ES".into()),
            user_id: None,
            unit_pref: None,
        },
    )
    .await?;
    for m in 0..60 {
        let (y, mo) = ((2021 * 12 + 10 + m) / 12, (2021 * 12 + 10 + m) % 12); // 2021-11 .. 2026-10
        let day = Utc
            .with_ymd_and_hms(y, mo as u32 + 1, 3, 8, 0, 0)
            .single()
            .unwrap_or_default();
        let src = SrcIn {
            profile_id: Some(pid),
            label: Some(format!("LabNorte · {} · p1", day.date_naive())),
            ..SrcIn::new("manual")
        };
        let src = create_src(&mut tx, &src).await?;
        for (i, (g, loinc)) in groups.iter().enumerate() {
            let v = if m == 59 {
                90 + i
            } else {
                50 + (m as usize * 7 + i * 13) % 40
            };
            let obs = ObservationIn::from_json(json!({
                "profile_id": pid, "printed_name": g, "printed_value": v.to_string(), "printed_unit": "U/L",
                "printed_ref_range": "40-80", "value_num": v.to_string(), "ref_low": "40", "ref_high": "80",
                "loinc": loinc, "alias_group": g, "unit_ucum": "U/L", "value_canonical": v.to_string(),
                "unit_canonical": "U/L", "interpretation": if v > 80 { "H" } else { "N" },
                "derivation_version": "summary", "effective_at": day.to_rfc3339(),
            }))?;
            commit_observation(&mut tx, &obs, &SrcRef::Id(src.clone()), CommitOptions::new("auto")).await?;
        }
    }
    tx.commit().await?;
    Ok(pid)
}
