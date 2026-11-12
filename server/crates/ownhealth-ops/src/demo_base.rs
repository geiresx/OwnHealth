//! Base demo data (`demo/base.py`), the demo hook `base` with order 0: lab reports with real
//! text-layer PDFs, records, a note, 90 days of wearables and an open review queue. Values follow the
//! Alex Rivera fixture (architecture.md section 16). All synthetic; the only lab is the fictional LabNorte.

use std::collections::BTreeMap;
use std::str::FromStr;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use ownhealth_core::acceptance::{Check, DEFAULT_THRESHOLD};
use ownhealth_core::demo::DemoContext;
use ownhealth_core::demo_pdf::{LabReport, LabRow, PAGE_H, PAGE_W, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::ids::new_src_id;
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_core::review::{CandidateIn, ReviewItemIn, create_extraction_run, create_item, submit_candidate};
use ownhealth_core::{Error, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use ownhealth_core::pyrandom::{PyRandom, round1};

const LAB: &str = "LabNorte";
const DERIVATION: &str = "demo-seed";
const TITLE: &str = "Informe de resultados de laboratorio";

struct Analyte {
    group: &'static str,
    loinc: &'static str,
    name: &'static str,
    unit: &'static str,
    ucum: &'static str,
    r#ref: &'static str,
    low: Option<&'static str>,
    high: Option<&'static str>,
    page: usize,
    specimen: &'static str,
}

#[allow(clippy::too_many_arguments)] // one row of the Python tuple table
const fn a(
    group: &'static str,
    loinc: &'static str,
    name: &'static str,
    unit: &'static str,
    ucum: &'static str,
    r#ref: &'static str,
    low: Option<&'static str>,
    high: Option<&'static str>,
    page: usize,
    specimen: &'static str,
) -> Analyte {
    Analyte {
        group,
        loinc,
        name,
        unit,
        ucum,
        r#ref,
        low,
        high,
        page,
        specimen,
    }
}

/// In the Python dict's order: a row's place on its page follows this list.
const ANALYTES: [Analyte; 17] = [
    a(
        "hemoglobin",
        "718-7",
        "Hemoglobina",
        "g/dL",
        "g/dL",
        "13.5 - 17.5",
        Some("13.5"),
        Some("17.5"),
        1,
        "blood",
    ),
    a(
        "platelets",
        "777-3",
        "Plaquetas",
        "x10^9/L",
        "10*9/L",
        "150 - 400",
        Some("150"),
        Some("400"),
        1,
        "blood",
    ),
    a(
        "mcv",
        "787-2",
        "VCM",
        "fL",
        "fL",
        "80 - 100",
        Some("80"),
        Some("100"),
        1,
        "blood",
    ),
    a(
        "alt",
        "1742-6",
        "ALT (GPT)",
        "U/L",
        "U/L",
        "7 - 41",
        Some("7"),
        Some("41"),
        1,
        "serum",
    ),
    a(
        "ggt",
        "2324-2",
        "GGT",
        "U/L",
        "U/L",
        "8 - 61",
        Some("8"),
        Some("61"),
        1,
        "serum",
    ),
    a(
        "creatinine",
        "2160-0",
        "Creatinina",
        "mg/dL",
        "mg/dL",
        "0.70 - 1.20",
        Some("0.70"),
        Some("1.20"),
        1,
        "serum",
    ),
    a(
        "egfr",
        "98979-8",
        "Filtrado glomerular (CKD-EPI 2021)",
        "mL/min/1.73m2",
        "mL/min/{1.73_m2}",
        "> 90",
        Some("90"),
        None,
        1,
        "serum",
    ),
    a(
        "tsh",
        "3016-3",
        "TSH",
        "mUI/L",
        "m[IU]/L",
        "0.40 - 4.00",
        Some("0.40"),
        Some("4.00"),
        1,
        "serum",
    ),
    a(
        "ft4",
        "3024-7",
        "T4 libre",
        "ng/dL",
        "ng/dL",
        "0.90 - 1.70",
        Some("0.90"),
        Some("1.70"),
        1,
        "serum",
    ),
    a(
        "hba1c",
        "4548-4",
        "Hemoglobina glicada (HbA1c)",
        "%",
        "%",
        "4.0 - 5.6",
        Some("4.0"),
        Some("5.6"),
        1,
        "blood",
    ),
    a(
        "urine_acr",
        "9318-7",
        "Cociente albúmina/creatinina",
        "mg/g",
        "mg/g",
        "< 30",
        None,
        Some("30"),
        1,
        "urine",
    ),
    a(
        "ldl_c",
        "13457-7",
        "Colesterol LDL",
        "mg/dL",
        "mg/dL",
        "< 130",
        None,
        Some("130"),
        2,
        "serum",
    ),
    a(
        "lpa",
        "43583-4",
        "Lipoproteína (a)",
        "nmol/L",
        "nmol/L",
        "< 75",
        None,
        Some("75"),
        2,
        "serum",
    ),
    a(
        "crp_hs",
        "30522-7",
        "Proteína C reactiva ultrasensible",
        "mg/L",
        "mg/L",
        "< 3.0",
        None,
        Some("3.0"),
        2,
        "serum",
    ),
    a(
        "vitamin_d_25oh",
        "62292-8",
        "Vitamina D (25-OH)",
        "ng/mL",
        "ng/mL",
        "30 - 100",
        Some("30"),
        Some("100"),
        2,
        "serum",
    ),
    a(
        "ferritin",
        "2276-4",
        "Ferritina",
        "ng/mL",
        "ng/mL",
        "30 - 400",
        Some("30"),
        Some("400"),
        2,
        "serum",
    ),
    a(
        "vitamin_b12",
        "2132-9",
        "Vitamina B12",
        "pg/mL",
        "pg/mL",
        "200 - 900",
        Some("200"),
        Some("900"),
        2,
        "serum",
    ),
];

type Series = &'static [(&'static str, &'static [(&'static str, &'static str)])];

const ALEX_SERIES: Series = &[
    (
        "ldl_c",
        &[
            ("2021-11-09", "163"),
            ("2022-09-15", "96"),
            ("2023-03-21", "88"),
            ("2023-10-04", "92"),
            ("2024-05-22", "84"),
            ("2025-01-20", "79"),
            ("2025-09-08", "101"),
            ("2026-03-02", "118"),
            ("2026-09-14", "97"),
        ],
    ),
    ("lpa", &[("2023-03-21", "138"), ("2025-09-08", "142")]),
    (
        "hba1c",
        &[
            ("2022-09-15", "5.3"),
            ("2023-03-21", "5.4"),
            ("2025-01-20", "5.4"),
            ("2026-03-02", "5.5"),
            ("2026-09-14", "5.6"),
        ],
    ),
    (
        "alt",
        &[
            ("2023-03-21", "31"),
            ("2023-10-04", "28"),
            ("2024-05-22", "26"),
            ("2025-01-20", "22"),
            ("2026-03-02", "27"),
            ("2026-09-14", "24"),
        ],
    ),
    (
        "ggt",
        &[
            ("2023-10-04", "38"),
            ("2024-05-22", "35"),
            ("2025-01-20", "29"),
            ("2026-03-02", "33"),
            ("2026-09-14", "31"),
        ],
    ),
    (
        "egfr",
        &[
            ("2023-10-04", "99"),
            ("2024-05-22", "97"),
            ("2025-01-20", "98"),
            ("2026-03-02", "95"),
            ("2026-09-14", "94"),
        ],
    ),
    (
        "creatinine",
        &[
            ("2023-10-04", "0.92"),
            ("2024-05-22", "0.94"),
            ("2025-01-20", "0.93"),
            ("2026-03-02", "0.97"),
            ("2026-09-14", "0.98"),
        ],
    ),
    (
        "urine_acr",
        &[("2023-03-21", "6"), ("2023-10-04", "9"), ("2024-05-22", "8")],
    ),
    (
        "tsh",
        &[
            ("2022-09-15", "1.8"),
            ("2023-10-04", "2.4"),
            ("2024-05-22", "1.9"),
            ("2025-01-20", "2.1"),
        ],
    ),
    (
        "ft4",
        &[
            ("2022-09-15", "1.1"),
            ("2023-10-04", "1.3"),
            ("2024-05-22", "1.2"),
            ("2025-01-20", "1.2"),
        ],
    ),
    (
        "hemoglobin",
        &[
            ("2024-05-22", "14.6"),
            ("2025-01-20", "14.9"),
            ("2025-09-08", "15.3"),
            ("2026-03-02", "15.0"),
            ("2026-09-14", "15.1"),
        ],
    ),
    (
        "platelets",
        &[
            ("2024-05-22", "244"),
            ("2025-01-20", "238"),
            ("2025-09-08", "226"),
            ("2026-03-02", "229"),
            ("2026-09-14", "231"),
        ],
    ),
    (
        "mcv",
        &[
            ("2024-05-22", "89"),
            ("2025-01-20", "90"),
            ("2025-09-08", "90"),
            ("2026-03-02", "91"),
            ("2026-09-14", "91"),
        ],
    ),
    (
        "crp_hs",
        &[
            ("2024-05-22", "0.9"),
            ("2025-01-20", "1.3"),
            ("2025-09-08", "0.8"),
            ("2026-03-02", "1.1"),
            ("2026-09-14", "2.4"),
        ],
    ),
    (
        "vitamin_d_25oh",
        &[
            ("2024-05-22", "33"),
            ("2025-01-20", "29"),
            ("2025-09-08", "35"),
            ("2026-03-02", "31"),
            ("2026-09-14", "24"),
        ],
    ),
    (
        "ferritin",
        &[
            ("2024-05-22", "88"),
            ("2025-01-20", "104"),
            ("2025-09-08", "91"),
            ("2026-03-02", "99"),
            ("2026-09-14", "96"),
        ],
    ),
    (
        "vitamin_b12",
        &[
            ("2025-01-20", "480"),
            ("2025-09-08", "455"),
            ("2026-03-02", "530"),
            ("2026-09-14", "512"),
        ],
    ),
];

/// The citation ids the docs use (architecture.md section 16). Everything else gets a generated id.
const ALEX_SRC_IDS: [((&str, &str), &str); 9] = [
    (("vitamin_d_25oh", "2026-09-14"), "src_a12"),
    (("lpa", "2025-09-08"), "src_a07"),
    (("tsh", "2025-01-20"), "src_a05"),
    (("ldl_c", "2021-11-09"), "src_1a0"),
    (("ldl_c", "2022-09-15"), "src_2b4"),
    (("ldl_c", "2023-03-21"), "src_2c1"),
    (("ldl_c", "2023-10-04"), "src_3c8"),
    (("ldl_c", "2024-05-22"), "src_3d2"),
    (("ldl_c", "2026-03-02"), "src_9f3"),
];
/// Direct LDL, LOINC 18262-6.
const DIRECT_LDL_DATES: [&str; 2] = ["2023-10-04", "2024-05-22"];

const OTHER_SERIES: [(&str, Series); 3] = [
    (
        "sam",
        &[
            ("ldl_c", &[("2026-06-10", "122")]),
            ("hba1c", &[("2026-06-10", "5.2")]),
            ("vitamin_d_25oh", &[("2026-06-10", "41")]),
            ("tsh", &[("2026-06-10", "1.6")]),
            ("ferritin", &[("2025-06-12", "28"), ("2026-06-10", "18")]),
            ("hemoglobin", &[("2026-06-10", "12.6")]),
        ],
    ),
    (
        "carmen",
        &[
            ("ldl_c", &[("2026-07-22", "110")]),
            ("hba1c", &[("2025-07-15", "6.1"), ("2026-07-22", "6.4")]),
            ("egfr", &[("2025-07-15", "64"), ("2026-07-22", "58")]),
            ("creatinine", &[("2026-07-22", "0.98")]),
            ("vitamin_d_25oh", &[("2026-07-22", "34")]),
        ],
    ),
    (
        "lucia",
        &[
            ("hemoglobin", &[("2026-02-18", "12.9")]),
            ("ferritin", &[("2026-02-18", "35")]),
            ("vitamin_d_25oh", &[("2026-02-18", "32")]),
        ],
    ),
];

type Range = (&'static str, Option<&'static str>, Option<&'static str>);

/// Reference ranges that differ for the other profiles (sex, age).
const RANGE_OVERRIDES: [((&str, &str), Range); 5] = [
    (("sam", "ferritin"), ("15 - 150", Some("15"), Some("150"))),
    (("sam", "hemoglobin"), ("12.0 - 15.5", Some("12.0"), Some("15.5"))),
    (("carmen", "egfr"), ("> 60", Some("60"), None)),
    (("lucia", "hemoglobin"), ("11.5 - 15.5", Some("11.5"), Some("15.5"))),
    (("lucia", "ferritin"), ("15 - 150", Some("15"), Some("150"))),
];

fn patient(slug: &str) -> (&'static str, &'static str) {
    match slug {
        "alex" => ("RIVERA, ALEX", "14/05/1979"),
        "sam" => ("RIVERA, SAM", "02/02/1981"),
        "lucia" => ("RIVERA, LUCIA", "21/06/2017"),
        _ => ("ORTEGA, CARMEN", "30/01/1955"),
    }
}

fn analyte(group: &str) -> Result<(usize, &'static Analyte)> {
    ANALYTES
        .iter()
        .enumerate()
        .find(|(_, a)| a.group == group)
        .ok_or_else(|| Error::Invalid(format!("demo: unknown analyte {group}")))
}

fn range_for(slug: &str, a: &Analyte) -> Range {
    RANGE_OVERRIDES
        .iter()
        .find(|((s, g), _)| *s == slug && *g == a.group)
        .map_or((a.r#ref, a.low, a.high), |(_, r)| *r)
}

fn dec(s: &str) -> Result<Decimal> {
    Decimal::from_str(s).map_err(|e| Error::Invalid(format!("demo: bad decimal {s}: {e}")))
}

fn opt_dec(s: Option<&str>) -> Result<Option<Decimal>> {
    s.map(dec).transpose()
}

/// The src id each documented literal (`src_a12`, ...) actually got in this seed.
type Assigned = BTreeMap<&'static str, String>;

/// Use a documented src id only once per database (a forced second seed gets generated ids).
async fn free(conn: &mut PgConnection, sid: Option<&str>) -> Result<Option<String>> {
    let Some(sid) = sid else { return Ok(None) };
    let taken: Option<i32> = sqlx::query_scalar("select 1 from src where id = $1")
        .bind(sid)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(taken.is_none().then(|| sid.to_string()))
}

fn dt(day: &str, hour: u32) -> Result<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|e| Error::Invalid(format!("demo: {day}: {e}")))?;
    d.and_hms_opt(hour, 0, 0)
        .map(|t| Utc.from_utc_datetime(&t))
        .ok_or_else(|| Error::Invalid(format!("demo: bad hour {hour}")))
}

fn interp(value: Decimal, low: Option<Decimal>, high: Option<Decimal>) -> &'static str {
    if low.is_some_and(|l| value < l) {
        return "L";
    }
    if let Some(h) = high
        && (value > h || (low.is_none() && value >= h))
    {
        return "H";
    }
    "N"
}

/// Spanish decimal comma on the printed report.
fn display(v: &str) -> String {
    v.replace('.', ",")
}

async fn add_document(
    conn: &mut PgConnection,
    ctx: &DemoContext,
    slug: &str,
    day: &str,
    pdf: &[u8],
    pages: usize,
) -> Result<Uuid> {
    let sha = ctx.blobs.put_bytes(pdf)?;
    let filename = format!("{LAB}_{day}.pdf");
    let doc = create_document(
        conn,
        &NewDocument {
            filename: Some(&filename),
            channel: "demo",
            pages: Some(pages as i32),
            doc_type: Some("lab_report"),
            ..NewDocument::upload(
                ctx.family.household_id,
                Some(ctx.family.profile(slug)),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?;
    Ok(doc.id)
}

async fn add_page(conn: &mut PgConnection, doc: Uuid, page_no: i32, text: &str) -> Result<()> {
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, text_content, fts) \
         values ($1, $2, $3, $4, true, $5, to_tsvector('simple', $5))",
    )
    .bind(doc)
    .bind(page_no)
    .bind(PAGE_W as f32)
    .bind(PAGE_H as f32)
    .bind(text)
    .execute(conn)
    .await?;
    Ok(())
}

async fn seed_reports(
    conn: &mut PgConnection,
    ctx: &mut DemoContext,
    slug: &str,
    series: Series,
    src_ids: &[((&str, &str), &'static str)],
    assigned: &mut Assigned,
) -> Result<()> {
    let profile_id = ctx.family.profile(slug);
    let value_of = |group: &str, day: &str| -> Option<&'static str> {
        series
            .iter()
            .find(|(g, _)| *g == group)
            .and_then(|(_, pts)| pts.iter().find(|(d, _)| *d == day).map(|(_, v)| *v))
    };
    let mut by_date: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (group, points) in series {
        for (day, _) in *points {
            by_date.entry(day).or_default().push(group);
        }
    }
    for (n, (day, groups)) in by_date.into_iter().enumerate() {
        let n = n + 1;
        let mut sorted = Vec::new();
        for g in groups {
            let (idx, a) = analyte(g)?;
            sorted.push((a.page, idx, a));
        }
        sorted.sort_by_key(|(page, idx, _)| (*page, *idx));
        let mut pages: Vec<Vec<(&Analyte, LabRow)>> = vec![Vec::new(), Vec::new()];
        for (page, _, a) in sorted {
            let (r, low, high) = range_for(slug, a);
            let value = value_of(a.group, day).unwrap_or_default();
            let flag = interp(dec(value)?, opt_dec(low)?, opt_dec(high)?);
            let row = LabRow::new(a.name, &display(value), a.unit, r).flagged(if flag == "N" { "" } else { flag });
            pages[page - 1].push((a, row));
        }
        pages.retain(|p| !p.is_empty());
        let (patient, birth) = patient(slug);
        let report_id = format!("LN-{}-{}{n}", day.replace('-', ""), slug[..2].to_uppercase());
        let collected = format!("{}/{}/{}", &day[8..10], &day[5..7], &day[0..4]);
        let (pdf, boxes) = lab_report_pdf(&LabReport {
            lab: LAB,
            title: TITLE,
            patient,
            birth,
            collected: &collected,
            report_id: &report_id,
            pages: pages
                .iter()
                .map(|p| p.iter().map(|(_, r)| r.clone()).collect())
                .collect(),
        });
        let doc = add_document(conn, ctx, slug, day, &pdf, pages.len()).await?;
        sqlx::query(
            "update document set language = 'es', provider_name = $1, provider_fingerprint = 'labnorte-es', \
             report_status = 'final', collected_at = $2, reported_at = $3, identity_match = 'matched' where id = $4",
        )
        .bind(LAB)
        .bind(dt(day, 8)?)
        .bind(dt(day, 8)? + Duration::days(1))
        .bind(doc)
        .execute(&mut *conn)
        .await?;
        ctx.documents.insert(format!("{slug}:{day}"), doc);
        for (page_idx, (page_rows, page_boxes)) in pages.iter().zip(&boxes).enumerate() {
            let page_no = page_idx as i32 + 1;
            let text: Vec<String> = page_rows
                .iter()
                .map(|(_, r)| {
                    format!("{} {} {} {} {}", r.name, r.value, r.unit, r.r#ref, r.flag)
                        .trim()
                        .to_string()
                })
                .collect();
            add_page(conn, doc, page_no, &text.join("\n")).await?;
            for ((a, row), bbox) in page_rows.iter().zip(page_boxes) {
                let (r, low, high) = range_for(slug, a);
                let value = dec(value_of(a.group, day).unwrap_or_default())?;
                let (low, high) = (opt_dec(low)?, opt_dec(high)?);
                let (mut loinc, mut method) = (a.loinc, None);
                if a.group == "ldl_c" {
                    if DIRECT_LDL_DATES.contains(&day) {
                        (loinc, method) = ("18262-6", Some("direct"));
                    } else {
                        method = Some("calculated");
                    }
                }
                let wanted = src_ids
                    .iter()
                    .find(|((g, d), _)| *g == a.group && *d == day)
                    .map(|(_, s)| *s);
                let src = SrcIn {
                    profile_id: Some(profile_id),
                    document_id: Some(doc),
                    page_no: Some(page_no),
                    bbox: Some(*bbox),
                    label: Some(format!("{LAB} · {day} · p{page_no}")),
                    id: free(conn, wanted).await?,
                    ..SrcIn::new("document_region")
                };
                let obs = ObservationIn::from_json(json!({
                    "profile_id": profile_id,
                    "printed_name": a.name,
                    "printed_value": row.value,
                    "printed_unit": a.unit,
                    "printed_ref_range": r,
                    "printed_flag": (!row.flag.is_empty()).then_some(&row.flag),
                    "value_num": value.to_string(),
                    "ref_low": low.map(|d| d.to_string()),
                    "ref_high": high.map(|d| d.to_string()),
                    "specimen": a.specimen,
                    "method": method,
                    "loinc": loinc,
                    "alias_group": a.group,
                    "unit_ucum": a.ucum,
                    "value_canonical": value.to_string(),
                    "unit_canonical": a.ucum,
                    "interpretation": interp(value, low, high),
                    "derivation_version": DERIVATION,
                    "effective_at": dt(day, 8)?.to_rfc3339(),
                    "document_id": doc,
                    "report_key": report_id,
                }))?;
                let done = commit_observation(conn, &obs, &SrcRef::New(src), CommitOptions::new("auto")).await?;
                if let Some(lit) = wanted {
                    assigned.insert(lit, done.src_id);
                }
            }
        }
    }
    Ok(())
}

async fn manual(conn: &mut PgConnection, user: Uuid, sid: Option<&str>, label: &str) -> Result<SrcRef> {
    Ok(SrcRef::New(SrcIn {
        label: Some(label.into()),
        id: free(conn, sid).await?,
        ..SrcIn::manual(Some(user))
    }))
}

async fn record(conn: &mut PgConnection, kind: &str, body: Value, src: SrcRef) -> Result<String> {
    let done = add_record(
        conn,
        &RecordIn::from_json(kind, body)?,
        &src,
        RecordOptions::new("person"),
    )
    .await?;
    Ok(done.src_id)
}

async fn seed_records(conn: &mut PgConnection, ctx: &DemoContext, assigned: &mut Assigned) -> Result<()> {
    let p = |slug: &str| ctx.family.profile(slug);
    let alex_user = ctx.family.user("alex");
    let entry = "manual"; // the UI shows it as "entered by hand" in its language
    let src = manual(conn, alex_user, Some("src_m01"), "Rx · 2022-06-20").await?;
    let body = json!({
        "profile_id": p("alex"), "name": "Atorvastatin", "ingredient": "atorvastatin", "strength": "20 mg",
        "dose_text": "1 tablet daily", "started": "2022-06-20", "stopped": "2025-12-01", "med_status": "stopped",
        "reason_text": "paused",
    });
    record(conn, "medication", body, src).await?;
    let src = manual(conn, alex_user, Some("src_m02"), "Rx · 2026-03-10").await?;
    let body = json!({
        "profile_id": p("alex"), "name": "Atorvastatin", "ingredient": "atorvastatin", "strength": "20 mg",
        "dose_text": "1 tablet daily", "started": "2026-03-10",
    });
    assigned.insert("src_m02", record(conn, "medication", body, src).await?);
    let src = manual(conn, alex_user, Some("src_m05"), "Rx · 2026-04-01").await?;
    let body = json!({
        "profile_id": p("alex"), "name": "Omeprazole", "ingredient": "omeprazole", "strength": "20 mg",
        "dose_text": "1 capsule before breakfast", "started": "2026-04-01",
    });
    assigned.insert("src_m05", record(conn, "medication", body, src).await?);
    let src = manual(conn, alex_user, None, "Rx · 2024-02-01").await?;
    let body = json!({
        "profile_id": p("carmen"), "name": "Metformin", "ingredient": "metformin", "strength": "850 mg",
        "dose_text": "1 tablet twice daily", "started": "2024-02-01",
    });
    record(conn, "medication", body, src).await?;
    let src = manual(conn, alex_user, Some("src_c02"), entry).await?;
    let body = json!({
        "profile_id": p("alex"), "text": "Hypercholesterolaemia", "code_system": "icd10", "code": "E78.0",
        "verification_status": "confirmed", "onset": "2021-11-09",
    });
    record(conn, "condition", body, src).await?;
    let src = manual(conn, alex_user, None, entry).await?;
    let body = json!({
        "profile_id": p("carmen"), "text": "Type 2 diabetes mellitus", "code_system": "icd10", "code": "E11.9",
        "verification_status": "confirmed", "onset": "2024-01-15",
    });
    record(conn, "condition", body, src).await?;
    let src = manual(conn, alex_user, None, entry).await?;
    let body =
        json!({"profile_id": p("sam"), "substance": "Penicillin", "criticality": "high", "reaction_text": "hives"});
    record(conn, "allergy", body, src).await?;
    let src = manual(conn, alex_user, None, entry).await?;
    let body = json!({"profile_id": p("alex"), "vaccine": "Td (tetanus, diphtheria)", "occurred": "2016-05-10"});
    record(conn, "immunization", body, src).await?;
    let src = manual(conn, alex_user, None, entry).await?;
    let body = json!({"profile_id": p("alex"), "vaccine": "Influenza 2025-26", "occurred": "2025-10-21"});
    record(conn, "immunization", body, src).await?;

    let sid = free(conn, Some("src_n41")).await?.unwrap_or_else(new_src_id);
    sqlx::query(
        "insert into src (id, profile_id, kind, actor_kind, actor_id, label) values \
         ($1, $2, 'manual', 'user', $3, 'note · 2026-10-30')",
    )
    .bind(&sid)
    .bind(p("alex"))
    .bind(alex_user)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "insert into note (profile_id, kind, author_kind, author_id, lang, attach_to, body, effective_at, \
         recorded_at, src_id) values ($1, 'priorities', 'person', $2, 'en', 'status', \
         'Recheck vitamin D in winter.', $3, $3, $4)",
    )
    .bind(p("alex"))
    .bind(alex_user)
    .bind(dt("2026-10-30", 19)?)
    .bind(&sid)
    .execute(&mut *conn)
    .await?;
    assigned.insert("src_n41", sid);
    Ok(())
}

/// The demo's ECG report: one synthetic page (built outside the async seed, as the writer is not `Send`).
const ECG_LINES: [&str; 5] = [
    "Hospital Norte · Cardiología",
    "Electrocardiograma de 12 derivaciones",
    "Paciente: RIVERA, ALEX · Fecha: 14/02/2024",
    "Ritmo sinusal, 64 lpm. PR 160 ms, QRS 92 ms, QTc 412 ms.",
    "Documento de demostración con datos sintéticos.",
];

fn ecg_pdf() -> Vec<u8> {
    use ownhealth_core::pdfwrite::{Font, Writer};
    let mut w = Writer::new();
    for (i, line) in ECG_LINES.iter().enumerate() {
        let (font, size) = if i < 2 {
            (Font::Bold, 12.0)
        } else {
            (Font::Regular, 10.0)
        };
        w.paragraph(line, font, size, 16.0);
    }
    w.finish().pdf
}

/// Visits (slice 6): Alex's traumatology visit, whose report never arrived, on the timeline, and a
/// dated ECG report from the same hospital, so the timeline shows a document event.
async fn seed_visits(conn: &mut PgConnection, ctx: &DemoContext) -> Result<()> {
    let src = manual(conn, ctx.family.user("alex"), None, "manual").await?;
    let body = json!({
        "profile_id": ctx.family.profile("alex"), "period_start": "2025-11-10T09:30:00+00:00",
        "reason_text": "Right knee pain", "provider_name": "Traumatología · Hospital Norte",
    });
    record(conn, "visit", body, src).await?;
    // A dated report that is not a lab report, so the timeline shows a document event.
    let day = "2024-02-14";
    let pdf = ecg_pdf();
    let sha = ctx.blobs.put_bytes(&pdf)?;
    let filename = format!("ECG_Hospital_Norte_{day}.pdf");
    let doc = create_document(
        conn,
        &NewDocument {
            filename: Some(&filename),
            channel: "demo",
            pages: Some(1),
            doc_type: Some("ecg"),
            ..NewDocument::upload(
                ctx.family.household_id,
                Some(ctx.family.profile("alex")),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?;
    sqlx::query(
        "update document set language = 'es', provider_name = 'Hospital Norte', collected_at = $1 where id = $2",
    )
    .bind(dt(day, 10)?)
    .bind(doc.id)
    .execute(&mut *conn)
    .await?;
    add_page(conn, doc.id, 1, &ECG_LINES.join("\n")).await?;
    Ok(())
}

/// metric: (start value, end value, noise), in the Python dict's order (the draws depend on it).
const WEARABLE_METRICS: [(&str, f64, f64, f64); 5] = [
    ("hr_resting", 58.0, 54.0, 1.5),      // bpm
    ("hrv_rmssd", 44.0, 48.0, 4.0),       // ms
    ("steps", 8100.0, 9240.0, 1800.0),    // count per day
    ("sleep_asleep", 420.0, 432.0, 35.0), // minutes per night
    ("vo2max", 41.2, 44.1, 0.3),          // mL/kg/min
];

/// The 90 daily samples per metric Python wrote: (metric, day index, value).
fn wearable_values() -> Vec<(&'static str, i64, f64)> {
    let mut rng = PyRandom::new(20_261_107);
    let days = 90i64;
    let mut out = Vec::new();
    for (metric, a, b, noise) in WEARABLE_METRICS {
        for i in 0..days {
            if metric == "vo2max" && i % 7 != 0 {
                continue;
            }
            let v = a + (b - a) * i as f64 / (days - 1) as f64 + rng.uniform(-noise, noise);
            out.push((metric, i, round1(v)));
        }
    }
    out
}

async fn seed_wearables(conn: &mut PgConnection, ctx: &DemoContext) -> Result<()> {
    let profile_id = ctx.family.profile("alex");
    let source_id: i16 = sqlx::query_scalar(
        "insert into wearable_source (profile_id, provider, device) values ($1, 'apple_health', 'Apple Watch') \
         returning id",
    )
    .bind(profile_id)
    .fetch_one(&mut *conn)
    .await?;
    let start = ctx.as_of - Duration::days(89);
    let (mut metrics, mut times, mut values) = (Vec::new(), Vec::new(), Vec::new());
    for (metric, i, v) in wearable_values() {
        let day = start + Duration::days(i);
        let t = day
            .and_hms_opt(6, 0, 0)
            .map(|t| Utc.from_utc_datetime(&t))
            .ok_or_else(|| Error::Invalid("demo: bad wearable time".into()))?;
        metrics.push(metric.to_string());
        times.push(t);
        values.push(v);
    }
    sqlx::query(
        "insert into wearable_sample (profile_id, metric, t, v, tz_offset_min, source_id) \
         select $1, m, t, v, 60, $5 from unnest($2::text[], $3::timestamptz[], $4::float8[]) as x(m, t, v)",
    )
    .bind(profile_id)
    .bind(&metrics)
    .bind(&times)
    .bind(&values)
    .bind(source_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, \
         v_p10, v_p50, v_p90, coverage) select profile_id, metric, source_id, 'day', date_trunc('day', t), \
         count(*), min(v), max(v), avg(v), sum(v), percentile_cont(0.1) within group (order by v), \
         percentile_cont(0.5) within group (order by v), percentile_cont(0.9) within group (order by v), 1.0 \
         from wearable_sample where profile_id = $1 group by 1, 2, 3, 5",
    )
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn failed(name: &str, reason: &str, detail: &str) -> Check {
    Check {
        detail: Some(detail.into()),
        ..Check::fail(name, reason)
    }
}

/// A new LabNorte report whose rows wait for review, and one agent proposal: 7 open items.
async fn seed_review_queue(conn: &mut PgConnection, ctx: &mut DemoContext, vitamin_d: &str) -> Result<()> {
    let profile_id = ctx.family.profile("alex");
    let day = "2026-11-03";
    #[allow(clippy::type_complexity)]
    let rows: [(&str, &str, &str, &str, &str, Option<&str>, f64, Vec<Check>); 6] = [
        (
            "Glucosa",
            "92",
            "mg/dL",
            "70 - 110",
            "glucose",
            Some("2345-7"),
            0.91,
            vec![],
        ),
        (
            "Colesterol HDL",
            "52",
            "mg/dL",
            "> 40",
            "hdl_c",
            Some("2085-9"),
            0.97,
            vec![failed(
                "grounding",
                "not_grounded",
                "value token not inside the row box",
            )],
        ),
        (
            "Triglicéridos",
            "110",
            "mg/dL",
            "< 150 mg/dl",
            "triglycerides",
            Some("2571-8"),
            0.96,
            vec![failed("range", "range_parse", "unit inside the range cell")],
        ),
        (
            "Hierro",
            "162",
            "mg/dL",
            "65 - 175",
            "iron",
            Some("2498-4"),
            0.95,
            vec![
                failed("plausibility", "implausible", "iron 162 mg/dL is above the hard limit"),
                failed("factor_1000", "suspect_separator", "162 µg/dL would be in range"),
            ],
        ),
        (
            "Leucocitos",
            "6800",
            "x10^3/µL",
            "4.0 - 11.0",
            "wbc",
            Some("6690-2"),
            0.96,
            vec![failed(
                "factor_1000",
                "suspect_separator",
                "6.8 x10^3/µL would be in range",
            )],
        ),
        (
            "Índice aterogénico",
            "3.1",
            "",
            "< 4.5",
            "atherogenic_index",
            None,
            0.93,
            vec![failed("loinc", "loinc_unknown", "no LOINC code for this name")],
        ),
    ];
    let (patient, birth) = patient("alex");
    let (pdf, boxes) = lab_report_pdf(&LabReport {
        lab: LAB,
        title: TITLE,
        patient,
        birth,
        collected: "03/11/2026",
        report_id: "LN-20261103-AL",
        pages: vec![
            rows.iter()
                .map(|(n, v, u, r, ..)| LabRow::new(n, &display(v), u, r))
                .collect(),
        ],
    });
    let doc = add_document(conn, ctx, "alex", day, &pdf, 1).await?;
    sqlx::query(
        "update document set language = 'es', provider_name = $1, report_status = 'final', collected_at = $2, \
         identity_match = 'matched' where id = $3",
    )
    .bind(LAB)
    .bind(dt(day, 8)?)
    .bind(doc)
    .execute(&mut *conn)
    .await?;
    let text: Vec<String> = rows
        .iter()
        .map(|(n, v, u, r, ..)| format!("{n} {} {u} {r}", display(v)))
        .collect();
    add_page(conn, doc, 1, &text.join("\n")).await?;
    ctx.documents.insert(format!("alex:{day}"), doc);
    let run = create_extraction_run(
        conn,
        doc,
        "rules:generic-table",
        "demo",
        b"demo",
        Some("demo-pdf"),
        Some("1"),
    )
    .await?;
    let first_page = boxes.first().cloned().unwrap_or_default();
    for ((name, value, unit, r, group, loinc, conf, checks), bbox) in rows.into_iter().zip(first_page) {
        let cells = json!({
            "name": {"text": name, "conf": 0.99},
            "value": {"text": display(value), "conf": conf},
            "unit": {"text": unit, "conf": 0.99},
            "ref_range": {"text": r, "conf": 0.98},
        });
        let unit_or_none = (!unit.is_empty()).then_some(unit);
        let observation = ObservationIn::from_json(json!({
            "profile_id": profile_id,
            "printed_name": name,
            "printed_value": display(value),
            "printed_unit": unit_or_none,
            "printed_ref_range": r,
            "value_num": value,
            "loinc": loinc,
            "alias_group": group,
            "unit_ucum": unit_or_none,
            "derivation_version": DERIVATION,
            "effective_at": dt(day, 8)?.to_rfc3339(),
        }))?;
        let cand = CandidateIn {
            page_no: 1,
            bbox,
            cells,
            row_conf: conf,
            observation,
            checks,
        };
        submit_candidate(conn, run, &cand, "rules_text", "matched", DEFAULT_THRESHOLD, false).await?;
    }
    create_item(
        conn,
        &ReviewItemIn {
            kind: "agent_proposal".into(),
            household_id: Some(ctx.family.household_id),
            profile_id: Some(profile_id),
            payload: json!({
                "kind": "note",
                "payload": {
                    "text": "Ask about a winter vitamin D recheck at the next visit.",
                    "attach_to": vitamin_d,
                    "lang": "en",
                },
                "token_label": "Claude Code · laptop",
                "based_on": [vitamin_d],
            }),
            context_src_ids: Some(vec![vitamin_d.into()]),
            ..ReviewItemIn::default()
        },
    )
    .await?;
    Ok(())
}

pub async fn seed_base(conn: &mut PgConnection, ctx: &mut DemoContext) -> Result<()> {
    let mut assigned = Assigned::new();
    seed_reports(conn, ctx, "alex", ALEX_SERIES, &ALEX_SRC_IDS, &mut assigned).await?;
    for (slug, series) in OTHER_SERIES {
        seed_reports(conn, ctx, slug, series, &[], &mut assigned).await?;
    }
    seed_records(conn, ctx, &mut assigned).await?;
    seed_wearables(conn, ctx).await?;
    let id = |lit: &str| {
        assigned
            .get(lit)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("demo: {lit} was never seeded")))
    };
    seed_review_queue(conn, ctx, &id("src_a12")?).await?;
    sqlx::query(
        "insert into household_setting (household_id, key, value) values ($1, 'age_of_majority', $2) \
         on conflict do nothing",
    )
    .bind(ctx.family.household_id)
    .bind(json!(18))
    .execute(&mut *conn)
    .await?;
    for (k, v) in [
        ("alex_vitamin_d", "src_a12"),
        ("alex_lpa", "src_a07"),
        ("alex_tsh", "src_a05"),
        ("alex_atorvastatin", "src_m02"),
        ("alex_omeprazole", "src_m05"),
        ("alex_note", "src_n41"),
    ] {
        ctx.src_ids.insert(k.into(), id(v)?);
    }
    seed_visits(conn, ctx).await?;
    Ok(())
}

/// v0.3 family history: Alex's relatives as the mockup draws them (synthetic). Its own demo hook
/// (`family`, after every other one), so the 0.3.0 seed stays exactly what it was.
pub async fn seed_family(conn: &mut PgConnection, ctx: &mut DemoContext) -> Result<()> {
    use ownhealth_core::family::{self, FamilyCondition, MemberIn};
    let alex = ctx.family.profile("alex");
    let user = ctx.family.user("alex");
    let condition = |kind: &str, onset_age: i32| FamilyCondition {
        kind: kind.into(),
        text: None,
        code_system: None,
        code: None,
        onset_age: Some(onset_age),
        onset_approx: false,
    };
    // relationship, born, death age, conditions, linked profile
    type Relative = (
        &'static str,
        Option<i32>,
        Option<i32>,
        Vec<FamilyCondition>,
        Option<&'static str>,
    );
    let rows: [Relative; 7] = [
        ("FTH", Some(1949), Some(64), vec![condition("mi", 52)], None),
        ("MTH", Some(1955), None, vec![condition("t2d", 69)], Some("carmen")),
        ("SIS", Some(1982), None, Vec::new(), None),
        ("SPS", Some(1981), None, Vec::new(), Some("sam")),
        ("DAU", Some(2017), None, Vec::new(), Some("lucia")),
        ("FTHINLAW", Some(1950), None, Vec::new(), None),
        ("MTHINLAW", Some(1953), None, Vec::new(), None),
    ];
    for (relationship, born_year, death_age, conditions, link) in rows {
        let mut m = MemberIn::new(alex, relationship);
        m.born_year = born_year;
        m.death_age = death_age;
        m.conditions = conditions;
        if relationship == "SPS" {
            m.sex_at_birth = Some("female".into());
        }
        let src = SrcRef::New(SrcIn {
            profile_id: Some(alex),
            label: Some("manual".into()),
            ..SrcIn::manual(Some(user))
        });
        family::add(
            &mut *conn,
            &m,
            &src,
            "person",
            None,
            link.map(|l| ctx.family.profile(l)),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn wearable_values_are_the_ones_python_drew() {
        let vals = wearable_values();
        assert_eq!(vals.len(), 373);
        let printed: Vec<String> = vals.iter().map(|(_, _, v)| format!("{v:?}")).collect();
        assert_eq!(printed[..3], ["58.2", "57.6", "58.8"]);
        // sha256 of ",".join(repr(v)) over base.py's samples, in insertion order
        let digest = hex::encode(Sha256::digest(printed.join(",").as_bytes()));
        assert_eq!(
            digest,
            "a8ea035a8eafb3668551d90ba194f50f2133397b33b7991de2889d74c006b175"
        );
    }

    #[test]
    fn interp_matches_python() {
        let d = |s: &str| dec(s).ok();
        assert_eq!(interp(Decimal::from(24), d("30"), d("100")), "L");
        assert_eq!(interp(Decimal::from(142), None, d("75")), "H");
        assert_eq!(interp(Decimal::from(75), None, d("75")), "H"); // "< 75" excludes 75
        assert_eq!(interp(Decimal::from(90), d("90"), None), "N");
    }
}
