//! Uploads and their import jobs, end to end through `POST /api/imports` and `/api/jobs`: a
//! LabNorte PDF, a FHIR IPS bundle, an Apple Health export and an OwnGenome facts file, each on a
//! profile the test made, then what each import put in the store and the review queue.
//! Python: tests/pdf/test_pdf_import.py, tests/imports/, tests/genome/test_genome_ingest.py.

use std::io::Write;

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{Client, fixture, patient_line, pdf};
use serde_json::{Value, json};

fn labnorte(name: &str, collected: &str, rows: &[pdf::Row]) -> Vec<u8> {
    pdf::lab_report(&patient_line(name), "02/04/1980", collected, "LN-CT", rows)
}

fn items(v: &Value) -> Vec<Value> {
    v["items"].as_array().cloned().unwrap_or_default()
}

/// The IPS fixture with its Patient renamed to the test's profile.
pub fn ips_for(name: &str) -> anyhow::Result<Vec<u8>> {
    let mut ips: Value = serde_json::from_slice(&fixture("fhir/ips-alex.json")?)?;
    let (given, family) = name.split_once(' ').context("two-word name")?;
    for e in ips["entry"].as_array_mut().context("entry")? {
        if e["resource"]["resourceType"] == "Patient" {
            e["resource"]["name"] = json!([{"family": family, "given": [given]}]);
            e["resource"]["birthDate"] = json!("1980-04-02");
        }
    }
    Ok(serde_json::to_vec(&ips)?)
}

/// A handmade Apple Health export.zip with steps from the iPhone and a resting heart rate.
pub fn apple_export() -> anyhow::Result<Vec<u8>> {
    let dev = |d: &str| format!("&lt;&lt;HKDevice: 0x1&gt;, name:{d}, model:{d}&gt;");
    let rec = |t: &str, unit: &str, d: &str, start: &str, end: &str, v: &str| {
        format!(
            " <Record type=\"{t}\" sourceName=\"x\" unit=\"{unit}\" device=\"{}\" startDate=\"{start}\" endDate=\"{end}\" value=\"{v}\"/>\n",
            dev(d)
        )
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<HealthData locale=\"es_ES\">\n <ExportDate value=\"2026-11-07 08:00:00 +0100\"/>\n <Me HKCharacteristicTypeIdentifierDateOfBirth=\"1980-04-02\"/>\n{}{}{}</HealthData>\n",
        rec(
            "HKQuantityTypeIdentifierStepCount",
            "count",
            "iPhone",
            "2026-11-01 08:00:00 +0100",
            "2026-11-01 08:30:00 +0100",
            "1200"
        ),
        rec(
            "HKQuantityTypeIdentifierStepCount",
            "count",
            "iPhone",
            "2026-11-01 18:00:00 +0100",
            "2026-11-01 18:30:00 +0100",
            "800"
        ),
        rec(
            "HKQuantityTypeIdentifierRestingHeartRate",
            "count/min",
            "Apple Watch",
            "2026-11-01 07:00:00 +0100",
            "2026-11-01 07:00:00 +0100",
            "58"
        ),
    );
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file(
            "apple_health_export/export.xml",
            zip::write::SimpleFileOptions::default(),
        )?;
        z.write_all(xml.as_bytes())?;
        z.finish()?;
    }
    Ok(buf.into_inner())
}

/// Alex's genome facts fixture as another sample, so it can go on a new profile.
pub fn genome_facts_for(slug: &str) -> anyhow::Result<Vec<u8>> {
    let mut g: Value = serde_json::from_slice(&fixture("genome/alex-rivera.genome-facts.json")?)?;
    g["subject"]["sample_id"] = json!(format!("CT-{slug}"));
    g["provenance"]["commit"] = json!(slug);
    Ok(serde_json::to_vec(&g)?)
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn a_labnorte_pdf_auto_accepts_clean_rows_and_queues_the_rest() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, name) = c.new_patient().await?;
    let rows = [
        pdf::Row("Colesterol LDL", "162", "mg/dL", "< 130", "H"),
        pdf::Row("Glucosa", "92", "mg/dL", "70 - 110", ""),
        pdf::Row("Leucocitos", "6800", "x10^3/µL", "4.0 - 11.0", ""),
    ];
    let doc = labnorte(&name, "14/09/2026", &rows);
    let (up, jobs) = c
        .import(&p, &[("labnorte.pdf", &doc, "application/pdf")])
        .await?;
    assert_shape("upload_pdf", &up)?;
    let item = &up["items"][0];
    assert_eq!(
        (
            item["kind"].clone(),
            item["filename"].clone(),
            item["duplicate"].clone()
        ),
        (json!("pdf"), json!("labnorte.pdf"), json!(false))
    );
    assert_eq!(item["sha256"].as_str().map(str::len), Some(64));
    assert_shape("jobs_pdf_done", &json!({"jobs": jobs}))?;
    let job = &jobs[0];
    assert_eq!(
        (
            job["kind"].clone(),
            job["state"].clone(),
            job["attempts"].clone()
        ),
        (json!("import.pdf"), json!("done"), json!(1))
    );
    assert_eq!(
        job["result"],
        json!({"queued": 1, "reasons": {"implausible": 1, "suspect_separator": 1}, "skipped": 0, "accepted": 2, "document_id": item["document_id"]})
    );

    // two rows are facts, accepted by the auto rule, citing the page region they came from
    let obs = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    let got: Vec<(String, String, String)> = items(&obs)
        .iter()
        .map(|o| {
            (
                o["alias_group"].as_str().unwrap_or_default().into(),
                o["interpretation"].as_str().unwrap_or_default().into(),
                o["accepted_by"].as_str().unwrap_or_default().into(),
            )
        })
        .collect();
    assert_eq!(got.len(), 2, "{obs}");
    assert!(
        got.contains(&("ldl_c".into(), "H".into(), "auto".into())),
        "{got:?}"
    );
    assert!(
        got.contains(&("glucose".into(), "N".into(), "auto".into())),
        "{got:?}"
    );
    let ldl = items(&obs)
        .into_iter()
        .find(|o| o["alias_group"] == "ldl_c")
        .context("ldl")?;
    assert_eq!(ldl["date"], "2026-09-14");
    assert_eq!(ldl["lab"], "LabNorte");
    assert_eq!(ldl["src_label"], "LabNorte · 2026-09-14 · p1");
    let src = c
        .get(&format!(
            "/api/sources/{}?include_text=true",
            ldl["src"].as_str().context("src")?
        ))
        .await?
        .expect(200)?
        .json;
    assert_eq!(src["kind"], "document_region");
    assert_eq!(src["location"]["page"], 1);
    assert_eq!(src["location"]["units"], "pt");
    assert_eq!(src["location"]["bbox"].as_array().map(Vec::len), Some(4));
    assert_eq!(src["text"]["trust"], "untrusted_text");
    assert_eq!(src["text"]["origin"], "pdf_text_layer");
    assert!(
        src["text"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("Colesterol LDL"))
    );

    // the white cell count with a misread thousands separator waits for a person
    let q = c
        .get(&format!("/api/review?profile={p}"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(q["open"], 1);
    assert_eq!(q["withheld"], 0);
    let it = &q["items"][0];
    assert_eq!(it["kind"], "candidate_row");
    assert_eq!(it["reasons"], json!(["implausible", "suspect_separator"]));
    assert_eq!(it["payload"]["identity"], "matched");
    assert_eq!(it["payload"]["observation"]["alias_group"], "wbc");
    assert_eq!(it["payload"]["cells"]["value"]["text"], "6800");
    assert_shape("review_item_pdf_row", it)?;

    // the document, its file, its page image and its word layer
    let d = item["document_id"].as_str().context("doc")?;
    let doc_json = c
        .get(&format!("/api/documents/{d}"))
        .await?
        .expect(200)?
        .json;
    assert_shape("document_uploaded", &doc_json)?;
    let f = c
        .get(&format!("/api/documents/{d}/file"))
        .await?
        .expect(200)?;
    assert_eq!(f.body, doc, "the original bytes come back");
    assert!(
        f.header("content-type")
            .is_some_and(|t| t.starts_with("application/pdf"))
    );
    let png = c
        .get(&format!("/api/documents/{d}/pages/1.png"))
        .await?
        .expect(200)?;
    assert!(png.body.starts_with(b"\x89PNG"), "not a PNG");
    assert_eq!(png.header("content-type").as_deref(), Some("image/png"));
    let words = c
        .get(&format!("/api/documents/{d}/pages/1/words"))
        .await?
        .expect(200)?
        .json;
    let texts: Vec<&str> = words["words"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["text"].as_str())
        .collect();
    for w in ["LabNorte", "Colesterol", "LDL", "162", "6800"] {
        assert!(texts.contains(&w), "word {w} missing: {texts:?}");
    }
    assert_eq!(
        (words["width_pt"].clone(), words["height_pt"].clone()),
        (json!(595.0), json!(842.0))
    );
    let r = c.get(&format!("/api/documents/{d}/pages/9/words")).await?;
    assert_eq!(r.status, 404);

    // the same bytes again are the same document, with no new job
    let again = c
        .upload(&p, &[("labnorte.pdf", &doc, "application/pdf")])
        .await?;
    assert_eq!(again["items"][0]["duplicate"], true);
    assert_eq!(again["items"][0]["document_id"], item["document_id"]);
    assert!(again["items"][0].get("job_id").is_none());

    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "import"
        && e["tool"] == "POST /api/imports"
        && e["profile"] == json!(p)
        && e["resource_type"] == "upload"
        && e["resource_ids"] == json!([item["sha256"]])));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn a_lab_slip_for_someone_else_is_held_for_identity() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, _) = c.new_patient().await?;
    let doc = pdf::lab_report(
        "GARCIA, OTRO",
        "01/01/1950",
        "14/09/2026",
        "LN-X",
        &[pdf::Row("Glucosa", "92", "mg/dL", "70 - 110", "")],
    );
    let (_, jobs) = c
        .import(&p, &[("otro.pdf", &doc, "application/pdf")])
        .await?;
    assert_eq!(jobs[0]["state"], "done");
    assert_eq!(jobs[0]["result"]["accepted"], 0, "{}", jobs[0]);
    let obs = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(obs["items"], json!([]));
    let q = c
        .get(&format!("/api/review?profile={p}"))
        .await?
        .expect(200)?
        .json;
    let kinds: Vec<&str> = q["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["kind"].as_str())
        .collect();
    assert!(!kinds.is_empty(), "nothing held for review");
    assert!(
        q["items"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|i| i["kind"] == "document_identity" || i["payload"]["identity"] == "mismatch"),
        "{q}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn a_fhir_ips_bundle_projects_records_and_queues_the_rest() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (p, name) = c.new_patient().await?;
    let (up, jobs) = c
        .import(
            &p,
            &[("ips.json", &ips_for(&name)?, "application/fhir+json")],
        )
        .await?;
    assert_eq!(up["items"][0]["kind"], "fhir_json");
    let job = &jobs[0];
    assert_eq!(
        (
            job["kind"].clone(),
            job["state"].clone(),
            job["filename"].clone()
        ),
        (
            json!("import.fhir_bundle"),
            json!("done"),
            json!("ips.json")
        )
    );
    assert_eq!(job["result"]["accepted"], 8);
    assert_eq!(job["result"]["queued"], 2);
    assert_eq!(job["result"]["skipped"], 1);
    assert_eq!(
        job["result"]["reasons"],
        json!({"unmapped": 1, "loinc_unknown": 1, "skip:status_entered-in-error": 1})
    );

    let cond = c
        .get(&format!("/api/profiles/{p}/records/condition"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(items(&cond).len(), 1);
    assert_eq!(cond["items"][0]["title"], "Hypertensive disorder");
    assert_eq!(cond["items"][0]["detail"]["code_system"], "snomed");
    assert_eq!(cond["items"][0]["detail"]["code"], "38341003");
    assert_eq!(cond["items"][0]["detail"]["accepted_by"], "import");
    let meds = c
        .get(&format!("/api/profiles/{p}/records/medication"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(meds["items"][0]["title"], "Enalapril 10 mg");
    assert_eq!(meds["items"][0]["detail"]["code"], "C09AA02");
    let allergy = c
        .get(&format!("/api/profiles/{p}/records/allergy"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        (
            allergy["items"][0]["detail"]["criticality"].clone(),
            allergy["items"][0]["detail"]["reaction_text"].clone()
        ),
        (json!("high"), json!("Rash"))
    );
    let vac = c
        .get(&format!("/api/profiles/{p}/records/immunization"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        (
            vac["items"][0]["detail"]["occurred"].clone(),
            vac["items"][0]["detail"]["lot"].clone()
        ),
        (json!("2025-10-20"), json!("SYN-0425"))
    );
    assert_eq!(
        items(
            &c.get(&format!("/api/profiles/{p}/records/procedure"))
                .await?
                .expect(200)?
                .json
        )
        .len(),
        1
    );
    assert_shape("records_fhir_condition", &cond)?;

    let obs = c
        .get(&format!("/api/profiles/{p}/observations"))
        .await?
        .expect(200)?
        .json;
    let mut names: Vec<String> = items(&obs)
        .iter()
        .filter_map(|o| o["printed"]["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "Diastolic blood pressure",
            "Hemoglobin A1c",
            "Systolic blood pressure"
        ]
    );
    let a1c = items(&obs)
        .into_iter()
        .find(|o| o["printed"]["name"] == "Hemoglobin A1c")
        .context("a1c")?;
    assert_eq!(
        (
            a1c["loinc"].clone(),
            a1c["printed"]["value"].clone(),
            a1c["interpretation"].clone()
        ),
        (json!("4548-4"), json!("5.9"), json!("H"))
    );
    let src = c
        .get(&format!(
            "/api/sources/{}",
            a1c["src"].as_str().context("src")?
        ))
        .await?
        .expect(200)?
        .json;
    assert_eq!(src["kind"], "fhir_import");
    assert_eq!(
        src["import_ref"],
        "ips-alex-2026/Observation/alex-rivera-hba1c"
    );

    let q = c
        .get(&format!("/api/review?profile={p}"))
        .await?
        .expect(200)?
        .json;
    let unmapped = items(&q)
        .into_iter()
        .find(|i| i["kind"] == "fhir_unmapped")
        .context("fhir_unmapped item")?;
    assert_eq!(unmapped["payload"]["resource_type"], "Encounter");
    assert_eq!(unmapped["payload"]["note"], "stored, not projected");
    let held = items(&q)
        .into_iter()
        .find(|i| i["kind"] == "candidate_row")
        .context("held row")?;
    assert_eq!(held["reasons"], json!(["loinc_unknown"]));
    assert!(
        held["payload"]["observation"]["fhir"]
            .to_string()
            .contains("Ignore previous")
    );
    assert_shape("review_item_fhir_unmapped", &unmapped)?;

    // the same bundle again runs again but doubles nothing
    let (_, again) = c
        .import(
            &p,
            &[("ips.json", &ips_for(&name)?, "application/fhir+json")],
        )
        .await?;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0]["state"], "done");
    assert_eq!(
        items(
            &c.get(&format!("/api/profiles/{p}/observations"))
                .await?
                .json
        )
        .len(),
        3
    );
    assert_eq!(
        items(
            &c.get(&format!("/api/profiles/{p}/records/condition"))
                .await?
                .json
        )
        .len(),
        1
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn an_apple_health_export_becomes_daily_wearable_values() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let (up, jobs) = c
        .import(&p, &[("export.zip", &apple_export()?, "application/zip")])
        .await?;
    assert_eq!(up["items"][0]["kind"], "apple_health_zip");
    assert_eq!(
        (jobs[0]["kind"].clone(), jobs[0]["state"].clone()),
        (json!("import.apple_health"), json!("done"))
    );
    assert_eq!(jobs[0]["result"]["accepted"], 3, "{}", jobs[0]);
    let s = c
        .get(&format!("/api/profiles/{p}/wearables/summary"))
        .await?
        .expect(200)?
        .json;
    assert_shape("wearables_summary_imported", &s)?;
    let metrics = s["metrics"].as_array().context("metrics")?;
    let hr = metrics
        .iter()
        .find(|m| m["metric"] == "hr_resting")
        .context("hr_resting")?;
    assert_eq!(
        (
            hr["latest"].clone(),
            hr["latest_date"].clone(),
            hr["agg"].clone()
        ),
        (json!(58.0), json!("2026-11-01"), json!("mean"))
    );
    let steps = c
        .get(&format!(
            "/api/profiles/{p}/wearables/steps?window=P30D&as_of=2026-11-07"
        ))
        .await?
        .expect(200)?
        .json;
    let day = steps["points"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|pt| pt["t"] == "2026-11-01T00:00:00Z")
        .context("steps day")?;
    assert_eq!(
        (
            day["value"].clone(),
            day["n"].clone(),
            day["min"].clone(),
            day["max"].clone()
        ),
        (json!(2000.0), json!(2), json!(800.0), json!(1200.0)),
        "{steps}"
    );
    assert_eq!(
        (steps["agg"].clone(), steps["resolution"].clone()),
        (json!("sum"), json!("day"))
    );
    assert_shape("wearables_steps_imported", &steps)?;
    // an unknown metric is an empty series named after itself, not an error
    let r = c
        .get(&format!("/api/profiles/{p}/wearables/no_such_metric"))
        .await?
        .expect(200)?;
    assert_eq!(
        (
            r.json["label"].clone(),
            r.json["unit"].clone(),
            r.json["points"].clone()
        ),
        (json!("no_such_metric"), json!(""), json!([]))
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn genome_facts_import_and_a_bad_file_becomes_a_job_failed_item() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let (up, jobs) = c
        .import(
            &p,
            &[("facts.json", &genome_facts_for(&p)?, "application/json")],
        )
        .await?;
    assert_eq!(up["items"][0]["kind"], "genome_facts");
    assert_eq!(
        (jobs[0]["kind"].clone(), jobs[0]["state"].clone()),
        (json!("import.genome_facts"), json!("done"))
    );
    assert_eq!(jobs[0]["result"]["accepted"], 14);
    let g = c
        .get(&format!("/api/profiles/{p}/genome"))
        .await?
        .expect(200)?
        .json;
    let tiles: Vec<(String, String)> = g["tiles"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            (
                t["key"].as_str().unwrap_or_default().into(),
                t["value"].as_str().unwrap_or_default().into(),
            )
        })
        .collect();
    assert_eq!(
        tiles,
        [
            ("pgx", "7"),
            ("carrier", "1"),
            ("hla", "1"),
            ("risk_marker", "1"),
            ("trait", "1"),
            ("prs", "1")
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .to_vec()
    );
    assert_eq!(g["dataset"]["assay"], "wgs");
    assert_eq!(
        g["dataset"]["raw_files"],
        json!({"vcf": false, "gvcf": false, "chip_sites": false})
    );
    assert_shape("genome_imported", &g)?;
    // the same sample on a second profile is refused
    let q = c.new_relative().await?;
    let (_, jobs) = c
        .import(
            &q,
            &[("facts.json", &genome_facts_for(&p)?, "application/json")],
        )
        .await?;
    assert_eq!(jobs[0]["state"], "dead", "{}", jobs[0]);

    let bad = serde_json::to_vec(&json!({"schema": "ownhealth.genome-facts/v9"}))?;
    let (_, jobs) = c
        .import(&p, &[("bad.json", &bad, "application/json")])
        .await?;
    let j = &jobs[0];
    assert_eq!(
        (j["state"].clone(), j["attempts"].clone()),
        (json!("dead"), json!(1))
    );
    let err = j["last_error"].as_str().context("last_error")?;
    assert!(
        err.starts_with("PermanentJobError: bad.json is not a valid genome-facts file:"),
        "{err}"
    );
    assert!(
        err.contains("unsupported genome-facts version 'ownhealth.genome-facts/v9'"),
        "{err}"
    );
    let q = c
        .get(&format!("/api/review?profile={p}&kind=job_failed"))
        .await?
        .expect(200)?
        .json;
    let item = &q["items"][0];
    assert_eq!(item["kind"], "job_failed");
    assert_eq!(item["payload"]["kind"], "import.genome_facts");
    assert_eq!(item["payload"]["error"], json!(err));
    assert_eq!(item["payload"]["job_id"], j["id"]);
    assert_shape("review_item_job_failed", item)?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn uploads_that_are_refused() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = c.new_relative().await?;
    let r = c
        .multipart(
            "/api/imports",
            &[("profile", &p)],
            &[("files", "notes.txt", b"hello", "text/plain")],
        )
        .await?;
    assert_eq!((r.status, r.code()), (415, Some("unsupported_file")));
    assert_eq!(
        r.json["detail"]["message"],
        "notes.txt: not a PDF, image, Apple Health export, FHIR bundle or genome facts file"
    );
    // a zip without export.xml is not an Apple Health export
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file("readme.txt", zip::write::SimpleFileOptions::default())?;
        z.write_all(b"x")?;
        z.finish()?;
    }
    let r = c
        .multipart(
            "/api/imports",
            &[("profile", &p)],
            &[("files", "other.zip", buf.get_ref(), "application/zip")],
        )
        .await?;
    assert_eq!((r.status, r.code()), (415, Some("unsupported_file")));
    // Sam's profile: Alex is a viewer and cannot import
    let doc = pdf::lab_report(
        "RIVERA, SAM",
        "01/01/1981",
        "14/09/2026",
        "LN-S",
        &[pdf::Row("Glucosa", "92", "mg/dL", "70 - 110", "")],
    );
    let r = c
        .multipart(
            "/api/imports",
            &[("profile", "sam")],
            &[("files", "sam.pdf", &doc, "application/pdf")],
        )
        .await?;
    assert_eq!(
        (r.status, r.code()),
        (403, Some("forbidden")),
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    // no profile field is the model's 422
    let r = c
        .multipart(
            "/api/imports",
            &[],
            &[("files", "sam.pdf", &doc, "application/pdf")],
        )
        .await?;
    assert_eq!(r.status, 422);
    // jobs list only the caller's profiles, newest first
    let jobs = c.get("/api/jobs?limit=5").await?.expect(200)?.json;
    let ids: Vec<i64> = jobs["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|j| j["id"].as_i64())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "{ids:?}");
    assert!(ids.len() <= 5);
    let r = c.get("/api/jobs?limit=0").await?;
    assert_eq!(r.status, 422);
    Ok(())
}
