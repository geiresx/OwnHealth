//! End-to-end lab PDF import on the database: routing, provenance, identity, pending documents, draw
//! grouping and amended reports, duplicates, injection, page access (ingestion.md 5-11).
//!
//! These run with the catalog-based stand-in normalizer, so they test lane B's pipeline alone;
//! `eval.rs` repeats the exit criteria with lane A's real normalizer.
mod support;

use std::collections::{BTreeMap, BTreeSet};

use ownhealth_core::review::accept;
use ownhealth_ingest_pdf::{ground_value, import_pdf, page_words, render_page_png};
use serde_json::{Value, json};
use sqlx::PgConnection;
use support::env::{env, fake_normalizer, items, obs, uuid};
use support::evaluate::score_slip;
use support::generate::{self, Layout};
use support::matching::inside;
use uuid::Uuid;

type R = anyhow::Result<()>;

fn slip(name: &str) -> (Vec<u8>, Value) {
    generate::all_slips()
        .into_iter()
        .chain(generate::no_text_layer())
        .chain(generate::injection_fixtures())
        .find(|(n, _, _)| n == name)
        .map(|(_, pdf, t)| (pdf, t))
        .expect("a slip")
}

fn layout(id: &str) -> Layout {
    generate::layouts().into_iter().find(|l| l.id == id).expect("a layout")
}

fn n_rows(t: &Value) -> usize {
    t["rows"].as_array().map_or(0, Vec::len)
}

async fn document(conn: &mut PgConnection, doc: Uuid) -> anyhow::Result<Value> {
    Ok(sqlx::query_scalar("select to_jsonb(d) from document d where id = $1")
        .bind(doc)
        .fetch_one(conn)
        .await?)
}

#[tokio::test]
async fn labnorte_3_page_report_meets_the_exit_criterion() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let (pdf, truth) = slip("es-labnorte-3p");
    let doc = e.store_pdf(&mut c, &pdf, "report.pdf").await?;
    let summary = import_pdf(&mut c, &e.blobs, doc, false).await?;
    let res = score_slip(&mut c, "es-labnorte-3p", &truth, doc, &summary).await?;
    assert!(res.auto as f64 / res.truth as f64 >= 0.98, "{}", summary.to_value());
    assert!(res.false_auto.is_empty() && res.dropped.is_empty(), "{res:?}");
    assert_eq!(res.auto_with_bbox, res.auto); // every committed row cites its page and a bbox around it
    for o in obs(&mut c, doc).await? {
        assert_eq!(o["src_kind"], "document_region");
        assert!([1, 2, 3].contains(&o["src_page"].as_i64().unwrap_or(0)));
        assert_eq!(o["src_bbox"].as_array().map_or(0, Vec::len), 4);
        assert_eq!(o["accepted_by"], "auto");
        assert!(
            o["report_key"]
                .as_str()
                .is_some_and(|k| k.starts_with("es/labnorte:2026-09-14:"))
        );
    }
    let d = document(&mut c, doc).await?;
    assert_eq!(
        (&d["doc_type"], &d["language"], &d["provider_name"], &d["report_status"]),
        (&json!("lab_report"), &json!("es"), &json!("LabNorte"), &json!("final"))
    );
    assert!(d["identity_match"] == "matched" && d["pages"] == 3);
    let pages: Vec<(i32, bool, bool)> = sqlx::query_as(
        "select page_no, text_layer_ok, words_blob is not null from document_page where document_id = $1 order by page_no",
    )
    .bind(doc)
    .fetch_all(&mut *c)
    .await?;
    assert_eq!(pages, vec![(1, true, true), (2, true, true), (3, true, true)]);
    let pending: Vec<(String, String)> =
        sqlx::query_as("select printed_name, reason from expected_test where report_document_id = $1")
            .bind(doc)
            .fetch_all(&mut *c)
            .await?;
    let want = truth["pending"][0]["name"].as_str().unwrap_or("").to_string();
    assert_eq!(pending, vec![(want, "Pendiente".to_string())]);
    Ok(())
}

#[tokio::test]
async fn reimport_is_skipped_unless_forced() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let doc = e.store_pdf(&mut c, &slip("es-01-basic").0, "report.pdf").await?;
    let first = import_pdf(&mut c, &e.blobs, doc, false).await?;
    let again = import_pdf(&mut c, &e.blobs, doc, false).await?;
    assert_eq!(first.accepted, 12);
    assert_eq!(again.reasons, BTreeMap::from([("already_imported".to_string(), 1)]));
    assert_eq!(again.accepted, 0);
    assert_eq!(obs(&mut c, doc).await?.len(), 12);
    Ok(())
}

#[tokio::test]
async fn report_for_another_family_member_never_auto_accepts() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let (pdf, truth) = slip("es-08-range-first"); // printed for Lucía
    assert_eq!(truth["person"], "lucia");
    let doc = e.store_pdf(&mut c, &pdf, "report.pdf").await?;
    let summary = import_pdf(&mut c, &e.blobs, doc, false).await?;
    assert_eq!(summary.accepted, 0);
    assert!(obs(&mut c, doc).await?.is_empty());
    let rows = items(&mut c, doc, Some("candidate_row")).await?;
    assert_eq!(rows.len(), n_rows(&truth));
    assert!(rows.iter().all(|r| {
        r["reasons"]
            .as_array()
            .is_some_and(|a| a.contains(&json!("identity_mismatch")))
    }));
    let ident = items(&mut c, doc, Some("document_identity")).await?;
    assert_eq!(ident.len(), 1);
    assert_eq!(
        ident[0]["payload"]["suggested_profile_id"],
        json!(e.fam.profile("lucia").to_string())
    );

    accept(&mut c, uuid(&ident[0]["id"]), e.fam.user("alex"), None).await?; // the person confirms the suggestion
    assert_eq!(
        uuid(&document(&mut c, doc).await?["profile_id"]),
        e.fam.profile("lucia")
    );
    assert!(
        items(&mut c, doc, Some("candidate_row"))
            .await?
            .iter()
            .all(|r| r["status"] == "dismissed")
    );
    let job: Value =
        sqlx::query_scalar("select payload from job where kind = 'import.pdf' and payload->>'document_id' = $1")
            .bind(doc.to_string())
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(job["force"], true);
    let again = import_pdf(&mut c, &e.blobs, doc, true).await?;
    assert_eq!(again.accepted as usize, n_rows(&truth));
    let owners: BTreeSet<Uuid> = obs(&mut c, doc).await?.iter().map(|o| uuid(&o["profile_id"])).collect();
    assert_eq!(owners, BTreeSet::from([e.fam.profile("lucia")]));
    Ok(())
}

#[tokio::test]
async fn scanned_pdf_and_image_wait_as_pending_documents() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let (pdf, _) = slip("es-scan");
    let scan = e.store_pdf(&mut c, &pdf, "report.pdf").await?;
    let s = import_pdf(&mut c, &e.blobs, scan, false).await?;
    let its = items(&mut c, scan, None).await?;
    assert_eq!(its.len(), 1);
    assert_eq!(
        (&its[0]["kind"], &its[0]["payload"]["why"], &its[0]["payload"]["pages"]),
        (&json!("pending_document"), &json!("no_text_layer"), &json!([1]))
    );
    assert!(s.queued == 1 && s.accepted == 0);
    let ok: Option<bool> = sqlx::query_scalar("select text_layer_ok from document_page where document_id = $1")
        .bind(scan)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(ok, Some(false));
    let runs: i64 = sqlx::query_scalar("select count(*) from extraction_run where document_id = $1")
        .bind(scan)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(runs, 0);

    // any PNG will do: a page render of the scan
    let sha = e.blobs.put_bytes(&pdf)?;
    let png = render_page_png(&e.blobs, &sha, 1, 0.5)?;
    let img = e.store(&mut c, &png, "alex", "image/png", "photo.png").await?;
    import_pdf(&mut c, &e.blobs, img, false).await?;
    let its = items(&mut c, img, None).await?;
    assert_eq!(its.len(), 1);
    assert_eq!(
        (&its[0]["kind"], &its[0]["payload"]["why"]),
        (&json!("pending_document"), &json!("image"))
    );
    import_pdf(&mut c, &e.blobs, img, true).await?;
    assert_eq!(items(&mut c, img, None).await?.len(), 1); // one open pending item per document, not one per run
    Ok(())
}

#[tokio::test]
async fn amended_edition_links_confirms_and_queues_changed_values() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let (pdf_a, truth_a) = slip("es-amend-a");
    let (pdf_b, truth_b) = slip("es-amend-b");
    let a = e.store_pdf(&mut c, &pdf_a, "a.pdf").await?;
    let sa = import_pdf(&mut c, &e.blobs, a, false).await?;
    assert_eq!(sa.accepted, 8);
    let statuses: BTreeSet<String> = obs(&mut c, a)
        .await?
        .iter()
        .map(|o| o["status"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(statuses, BTreeSet::from(["provisional".to_string()])); // the first edition said "provisional"

    let b = e.store_pdf(&mut c, &pdf_b, "b.pdf").await?;
    let sb = import_pdf(&mut c, &e.blobs, b, false).await?;
    assert_eq!(uuid(&document(&mut c, b).await?["supersedes_document"]), a);
    assert_eq!(
        sb.reasons,
        BTreeMap::from([
            ("identical_in_earlier_edition".to_string(), 7),
            ("amended_value".to_string(), 1)
        ])
    );
    assert_eq!(sb.accepted, 1); // AST is new in the amended edition
    let old: BTreeMap<String, Value> = obs(&mut c, a)
        .await?
        .into_iter()
        .map(|o| (o["alias_group"].as_str().unwrap_or("").to_string(), o))
        .collect();
    assert_eq!(old["glucose"]["status"], "provisional");
    assert!(
        old.iter()
            .filter(|(g, _)| *g != "glucose")
            .all(|(_, o)| o["status"] == "current")
    );
    let amended = items(&mut c, b, Some("amended_report")).await?;
    assert_eq!(amended.len(), 1);
    assert_ne!(amended[0]["payload"]["old_printed_value"], "142");
    assert_eq!(amended[0]["observation_id"], old["glucose"]["id"]);
    assert!(
        amended[0]["reasons"]
            .as_array()
            .is_some_and(|r| r.contains(&json!("amended_value")))
    );

    let ids = accept(&mut c, uuid(&amended[0]["id"]), e.fam.user("alex"), None).await?;
    assert_eq!(ids.len(), 1);
    let old_id = uuid(&old["glucose"]["id"]);
    let old_g: String = sqlx::query_scalar("select status from observation where id = $1")
        .bind(old_id)
        .fetch_one(&mut *c)
        .await?;
    let new: Value = sqlx::query_scalar("select to_jsonb(o) from observation o where id = $1")
        .bind(ids[0])
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(old_g, "superseded");
    assert_eq!(uuid(&new["supersedes"]), old_id);
    assert_eq!(new["printed_value"], "142");
    assert_eq!(uuid(&new["document_id"]), b);
    assert_eq!(new["accepted_by"], "person");
    // every row of edition b is accounted for: linked, queued or committed
    let runs: Vec<(String, i64)> = sqlx::query_as(
        "select c.state, count(*) from candidate_row c join extraction_run r on r.id = c.extraction_run_id \
         where r.document_id = $1 group by 1 order by 1",
    )
    .bind(b)
    .fetch_all(&mut *c)
    .await?;
    assert_eq!(
        runs,
        vec![("auto_accepted".to_string(), 1), ("confirmed".to_string(), 8)]
    );
    assert!(n_rows(&truth_b) == 9 && n_rows(&truth_a) == 8);
    Ok(())
}

#[tokio::test]
async fn same_report_in_another_file_is_linked_not_duplicated() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let (pdf, _) = slip("es-01-basic");
    let reprint = generate::render(
        &Layout {
            meta: vec![("Subject", "reimpresión")],
            ..layout("es-01-basic")
        },
        None,
    )
    .0;
    assert_ne!(reprint, pdf);
    let first = e.store_pdf(&mut c, &pdf, "a.pdf").await?;
    import_pdf(&mut c, &e.blobs, first, false).await?;
    let second = e.store_pdf(&mut c, &reprint, "b.pdf").await?;
    let s = import_pdf(&mut c, &e.blobs, second, false).await?;
    // same sample id and day: the reprint is read as an edition of the first, with nothing changed
    assert!(s.accepted == 0 && s.queued == 0 && s.skipped == 12, "{}", s.to_value());
    assert!(obs(&mut c, second).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn conflicting_value_from_another_lab_goes_to_review() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let lay = layout("en-01-basic");
    let a = generate::render(&lay, None).0;
    let b = generate::render(
        &Layout {
            lab: "Demo Diagnostics".into(),
            sample: "OTHER-1".into(),
            analytes: vec!["glucose"],
            changes: vec![("glucose", "97")],
            ..lay
        },
        None,
    )
    .0;
    let doc_a = e.store_pdf(&mut c, &a, "a.pdf").await?;
    import_pdf(&mut c, &e.blobs, doc_a, false).await?;
    let doc_b = e.store_pdf(&mut c, &b, "b.pdf").await?;
    let s = import_pdf(&mut c, &e.blobs, doc_b, false).await?;
    assert_eq!(s.accepted, 0);
    assert_eq!(s.reasons.get("conflict"), Some(&1));
    Ok(())
}

fn strs(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(String::from))
        .collect()
}

#[tokio::test]
async fn injection_fixtures_are_flagged_and_hidden_values_never_read() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    for name in [
        "inj-white-on-white",
        "inj-hidden-ai-instruction",
        "inj-url-in-table",
        "inj-metadata",
    ] {
        let (pdf, truth) = slip(name);
        let doc = e.store_pdf(&mut c, &pdf, &format!("{name}.pdf")).await?;
        import_pdf(&mut c, &e.blobs, doc, false).await?;
        let flags = document(&mut c, doc).await?["flags"].clone();
        let exp = &truth["expect"];
        if exp.get("possible_injection").is_some() {
            assert!(
                strs(&exp["possible_injection"]).is_subset(&strs(&flags["possible_injection"]["pages"])),
                "{name}"
            );
        }
        if exp.get("possible_injection_metadata").is_some() {
            assert!(
                strs(&exp["possible_injection_metadata"]).is_subset(&strs(&flags["possible_injection"]["metadata"]))
            );
            assert!(
                flags["metadata_text"]["Subject"]
                    .as_str()
                    .is_some_and(|s| s.contains("System prompt"))
            );
        }
        assert_eq!(flags.get("hidden_text").is_some(), exp["hidden_text"] == true, "{name}");
        let values: Vec<String> = obs(&mut c, doc)
            .await?
            .iter()
            .map(|o| o["printed_value"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(!values.iter().any(|v| v == "45" || v.contains("http")), "{name}");
        if name == "inj-url-in-table" {
            let url: Vec<Value> = items(&mut c, doc, Some("candidate_row"))
                .await?
                .into_iter()
                .filter(|i| {
                    i["payload"]["cells"]["value"]["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("http"))
                })
                .collect();
            assert_eq!(url.len(), 1);
            assert!(
                url[0]["reasons"]
                    .as_array()
                    .is_some_and(|r| r.contains(&json!("possible_injection")))
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn page_words_ground_value_and_render() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let (pdf, truth) = slip("es-labnorte-3p");
    let doc = e.store_pdf(&mut c, &pdf, "report.pdf").await?;
    import_pdf(&mut c, &e.blobs, doc, false).await?;
    let row = &truth["rows"][2]; // a value with a decimal comma
    let (page, value, name) = (
        row["page"].as_i64().unwrap_or(0) as i32,
        row["value"].as_str().unwrap_or(""),
        row["name"].as_str().unwrap_or(""),
    );
    assert!(value.contains(','));
    let words = page_words(&mut c, &e.blobs, doc, page).await?;
    assert!(words.iter().any(|w| w.text == value));
    let dotted = value.replace(',', ".");
    let tb: Vec<f64> = row["bbox"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .collect();
    let b = ground_value(&mut c, &e.blobs, doc, page, name, &dotted)
        .await?
        .expect("grounded");
    assert!(inside(&[tb[0], tb[1], tb[0] + 10.0, tb[3]], &b));
    assert!(
        ground_value(&mut c, &e.blobs, doc, 1, "Glucosa", "9999")
            .await?
            .is_none()
    );
    assert!(page_words(&mut c, &e.blobs, doc, 9).await?.is_empty());

    let inj = e.store_pdf(&mut c, &slip("inj-white-on-white").0, "inj.pdf").await?;
    import_pdf(&mut c, &e.blobs, inj, false).await?;
    let hidden: BTreeSet<String> = page_words(&mut c, &e.blobs, inj, 1)
        .await?
        .into_iter()
        .filter_map(|w| w.hidden)
        .collect();
    assert_eq!(hidden, BTreeSet::from(["white_on_white".to_string()]));
    assert!(ground_value(&mut c, &e.blobs, inj, 1, "Glucosa", "45").await?.is_none());

    let sha: String = sqlx::query_scalar("select encode(blob_sha256, 'hex') from document where id = $1")
        .bind(doc)
        .fetch_one(&mut *c)
        .await?;
    let png = render_page_png(&e.blobs, &sha, 3, 1.0)?;
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    assert!(w.abs_diff(595) <= 1 && h.abs_diff(842) <= 1, "{w}x{h}");
    assert!(matches!(
        render_page_png(&e.blobs, &sha, 4, 1.0),
        Err(ownhealth_core::Error::NotFound(_))
    ));
    Ok(())
}

#[tokio::test]
async fn import_pdf_job_runs_through_the_queue() -> R {
    fake_normalizer();
    let e = env().await?;
    let (pdf, truth) = slip("de-01-basic");
    let sha = e.blobs.put_bytes(&pdf)?;
    let mut c = e.conn().await?;
    let out = ownhealth_core::documents::dispatch_upload(
        &mut c,
        &e.blobs,
        e.fam.household_id,
        e.fam.profile("alex"),
        &[ownhealth_core::documents::StoredFile {
            filename: "befund.pdf".into(),
            sha256: sha,
            size: pdf.len() as i64,
        }],
        None,
    )
    .await?;
    drop(c);
    let its = out["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(its.len(), 1);
    e.drain().await?;
    let mut c = e.conn().await?;
    let (state, result): (String, Value) = sqlx::query_as("select state, result from job where id = $1")
        .bind(its[0]["job_id"].as_i64().unwrap_or(0))
        .fetch_one(&mut *c)
        .await?;
    let n: i64 = sqlx::query_scalar("select count(*) from observation where document_id = $1")
        .bind(uuid(&its[0]["document_id"]))
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(state, "done");
    let (acc, queued) = (
        result["accepted"].as_i64().unwrap_or(-1),
        result["queued"].as_i64().unwrap_or(-1),
    );
    assert_eq!(acc + queued, n_rows(&truth) as i64);
    assert_eq!(n + queued, n_rows(&truth) as i64);
    Ok(())
}

#[tokio::test]
async fn bad_payload_is_a_permanent_job_failure() -> R {
    let e = env().await?;
    let mut c = e.conn().await?;
    let jid = ownhealth_core::jobs::enqueue(
        &mut c,
        "import.pdf",
        &json!({"filename": "x.pdf"}),
        ownhealth_core::jobs::EnqueueOptions::dedupe("import.pdf:bad-payload-test"),
    )
    .await?
    .expect("a job id");
    drop(c);
    e.drain().await?;
    let mut c = e.conn().await?;
    let got: (String, i32) = sqlx::query_as("select state, attempts from job where id = $1")
        .bind(jid)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(got, ("dead".to_string(), 1));
    Ok(())
}

#[tokio::test]
async fn lab_values_carry_a_provenance_label() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let doc = e
        .store_pdf(&mut c, &slip("es-labnorte-3p").0, "LabNorte_2026-09-14.pdf")
        .await?;
    import_pdf(&mut c, &e.blobs, doc, false).await?;
    let labels: Vec<(Option<i32>, Option<String>)> = sqlx::query_as(
        "select s.page_no, s.label from observation o join src s on s.id = o.src_id where o.document_id = $1 \
         order by s.page_no",
    )
    .bind(doc)
    .fetch_all(&mut *c)
    .await?;
    assert!(!labels.is_empty());
    assert_eq!(labels[0], (Some(1), Some("LabNorte · 2026-09-14 · p1".to_string())));
    for (page, label) in labels {
        assert_eq!(label, Some(format!("LabNorte · 2026-09-14 · p{}", page.unwrap_or(0))));
    }
    Ok(())
}

#[tokio::test]
async fn with_ocr_on_scans_and_images_go_to_the_ocr_queue_not_to_review() -> R {
    use ownhealth_core::acceptance::DEFAULT_THRESHOLD;
    use ownhealth_ingest_pdf::{ImportOptions, import_pdf_opts};
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let on = ImportOptions {
        threshold: DEFAULT_THRESHOLD,
        ocr: true,
    };
    let (pdf, _) = slip("es-scan");
    let scan = e.store_pdf(&mut c, &pdf, "report.pdf").await?;
    import_pdf_opts(&mut c, &e.blobs, scan, false, on).await?;
    let sha = e.blobs.put_bytes(&pdf)?;
    let png = render_page_png(&e.blobs, &sha, 1, 0.5)?;
    let img = e.store(&mut c, &png, "alex", "image/png", "photo.png").await?;
    import_pdf_opts(&mut c, &e.blobs, img, false, on).await?;
    for doc in [scan, img] {
        assert!(
            items(&mut c, doc, None).await?.is_empty(),
            "a scan waits for OCR, not for a person"
        );
        let (queue, pages, state): (String, Value, String) = sqlx::query_as(
            "select j.queue, j.payload->'pages', d.ocr_state from job j join document d on d.id::text = j.payload->>'document_id' \
             where j.kind = 'ocr.document' and d.id = $1",
        )
        .bind(doc)
        .fetch_one(&mut *c)
        .await?;
        assert_eq!((queue.as_str(), pages, state.as_str()), ("ocr", json!([1]), "queued"));
    }

    // off (the default): the pending item as before, and the document says why it waits
    let (pdf2, _) = slip("es-scan");
    let other = e
        .store_pdf(&mut c, &[pdf2.as_slice(), b"\n%second copy"].concat(), "again.pdf")
        .await?;
    import_pdf(&mut c, &e.blobs, other, false).await?;
    let state: String = sqlx::query_scalar("select ocr_state from document where id = $1")
        .bind(other)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(state, "off");
    assert_eq!(items(&mut c, other, Some("pending_document")).await?.len(), 1);
    // the same scan with other bytes is also "probably the same as" the first (dedup level 2)
    let near = items(&mut c, other, Some("near_duplicate")).await?;
    assert_eq!(near.len(), 1);
    assert_eq!(near[0]["payload"]["same_as"], json!(scan.to_string()));
    assert_eq!(items(&mut c, other, None).await?.len(), 2);
    Ok(())
}

/// The locked original of an email PDF is kept as received and never read: reprocessing it, or moving
/// it to another profile, files nothing (its unlocked copy is the document that is imported).
#[tokio::test]
async fn a_locked_original_is_never_imported() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let locked = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../fixtures/A-email/labnorte-locked-12345678Z.pdf"
    ))?;
    let doc = e.store_pdf(&mut c, &locked, "locked.pdf").await?;
    sqlx::query("update document set flags = flags || '{\"encrypted\": true}'::jsonb where id = $1")
        .bind(doc)
        .execute(&mut *c)
        .await?;
    let summary = import_pdf(&mut c, &e.blobs, doc, true).await?.to_value();
    assert_eq!(summary["reasons"]["encrypted_original"], 1, "{summary}");
    let filed: i64 = sqlx::query_scalar(
        "select (select count(*) from review_item where document_id = $1) \
         + (select count(*) from extraction_run where document_id = $1) \
         + (select count(*) from document_page where document_id = $1)",
    )
    .bind(doc)
    .fetch_one(&mut *c)
    .await?;
    assert_eq!(filed, 0, "a locked original was read");
    Ok(())
}

/// A one-page LabNorte report for alex (synthetic), with the collection date as printed.
fn labnorte(report_id: &str, collected: &str, rows: &[(&str, &str, &str, &str)]) -> Vec<u8> {
    use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
    lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe de laboratorio",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected,
        report_id,
        pages: vec![rows.iter().map(|(n, v, u, r)| LabRow::new(n, v, u, r)).collect()],
    })
    .0
}

const GLUCOSE: (&str, &str, &str, &str) = ("Glucosa", "102", "mg/dL", "70 - 110");
const OTHERS: [(&str, &str, &str, &str); 2] = [
    ("Creatinina", "0,9", "mg/dL", "0,7 - 1,3"),
    ("Urea", "32", "mg/dL", "15 - 45"),
];
/// Other analytes again, so the date-only report is not read as an edition of an earlier one.
const MORE: [(&str, &str, &str, &str); 2] = [
    ("Hemoglobina", "14,8", "g/dL", "13,0 - 17,0"),
    ("Hematocrito", "44", "%", "40 - 52"),
];

async fn glucose_rows(conn: &mut PgConnection) -> anyhow::Result<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from observation where alias_group = 'glucose' and status = 'current'")
            .fetch_one(conn)
            .await?,
    )
}

#[tokio::test]
async fn the_same_value_from_another_lab_on_the_same_day_is_kept() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let a = e
        .store_pdf(&mut c, &labnorte("LN-D1", "14/09/2026 08:15", &[GLUCOSE]), "a.pdf")
        .await?;
    let s = import_pdf(&mut c, &e.blobs, a, false).await?;
    assert_eq!(s.accepted, 1, "{}", s.to_value());
    // the first result came from another lab (fictional)
    sqlx::query("update document set provider_name = 'Laboratorio Centro' where id = $1")
        .bind(a)
        .execute(&mut *c)
        .await?;
    let mut rows = vec![GLUCOSE];
    rows.extend(OTHERS);
    let b = e
        .store_pdf(&mut c, &labnorte("LN-D2", "14/09/2026 08:15", &rows), "b.pdf")
        .await?;
    let s = import_pdf(&mut c, &e.blobs, b, false).await?;
    assert_eq!(s.reasons.get("duplicate"), None, "{}", s.to_value());
    assert_eq!(s.accepted, 3, "{}", s.to_value());
    assert_eq!(glucose_rows(&mut c).await?, 2, "two labs, two facts");
    Ok(())
}

#[tokio::test]
async fn the_same_fact_again_is_linked_and_a_date_only_one_goes_to_review() -> R {
    fake_normalizer();
    let e = env().await?;
    let mut c = e.conn().await?;
    let a = e
        .store_pdf(&mut c, &labnorte("LN-D3", "14/09/2026 08:15", &[GLUCOSE]), "a.pdf")
        .await?;
    import_pdf(&mut c, &e.blobs, a, false).await?;
    let mut rows = vec![GLUCOSE];
    rows.extend(OTHERS);

    // same lab, same time, same value: one fact, with a second provenance link
    let b = e
        .store_pdf(&mut c, &labnorte("LN-D4", "14/09/2026 08:15", &rows), "b.pdf")
        .await?;
    let s = import_pdf(&mut c, &e.blobs, b, false).await?;
    assert_eq!(s.reasons.get("duplicate"), Some(&1), "{}", s.to_value());
    assert_eq!(glucose_rows(&mut c).await?, 1);
    let links: i64 = sqlx::query_scalar("select count(*) from observation_link where document_id = $1")
        .bind(b)
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(links, 1);

    // a date with no time: maybe the same draw, maybe another one; a person says
    let mut rows = vec![GLUCOSE];
    rows.extend(MORE);
    let d = e
        .store_pdf(&mut c, &labnorte("LN-D5", "14/09/2026", &rows), "d.pdf")
        .await?;
    let s = import_pdf(&mut c, &e.blobs, d, false).await?;
    assert_eq!(s.reasons.get("collection_time_unknown"), Some(&1), "{}", s.to_value());
    assert_eq!(glucose_rows(&mut c).await?, 1);
    Ok(())
}
