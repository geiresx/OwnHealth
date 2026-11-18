//! The International Patient Summary: `POST /api/profiles/{p}/export {fmt: "ips", lang}` answers a
//! FHIR R4 document Bundle (`ownhealth_ops::ips`). On the Rivera demo, Alex's Bundle imports back
//! into a fresh profile with every resource projected and the same values and units, two exports
//! give the same bytes, and Lucía's empty required sections carry the IPS "no information" entry.
//!
//! With `OWNHEALTH_FHIR_OUT=<dir>` the demo test also writes Alex's and Carmen's Bundles there, for
//! the HL7 validator job (.github/workflows/fhir-validate.yml).

#[path = "auth_support.rs"]
mod support;

use std::collections::HashSet;

use axum::http::StatusCode;
use ownhealth_core::access::{ProfileIn, all_scopes, create_household, create_profile, get_profile};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{DEMO_HOUSEHOLD, seed_demo};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::testing::TestDb;
use ownhealth_ingest_pdf::textcheck::assert_text_layer;
use ownhealth_ingest_structured::fhir::{import_resources, read_entries};
use ownhealth_ops::ips::{self, Ips};
use serde_json::{Value, json};
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

/// The six IPS sections in the order the Composition lists them.
const SECTIONS: [&str; 6] = ["11450-4", "48765-2", "10160-0", "11369-6", "30954-2", "47519-4"];
const REQUIRED: [&str; 3] = ["11450-4", "48765-2", "10160-0"];

fn as_of() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap_or_default()
}

fn sections(b: &Value) -> Vec<Value> {
    b["entry"][0]["resource"]["section"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn code(v: &Value) -> &str {
    v["code"]["coding"][0]["code"].as_str().unwrap_or_default()
}

/// The resource a section entry points at.
fn target<'a>(b: &'a Value, reference: &Value) -> &'a Value {
    b["entry"]
        .as_array()
        .and_then(|e| e.iter().find(|e| e["fullUrl"] == *reference))
        .map_or(&Value::Null, |e| &e["resource"])
}

/// An IPS "no information about ..." entry (absent-unknown code system).
fn no_info(r: &Value) -> bool {
    let concept = r.get("code").or_else(|| r.get("medicationCodeableConcept"));
    concept.is_some_and(|c| c["coding"][0]["system"] == ips::ABSENT_UNKNOWN)
}

/// Every `reference` in the Bundle names an entry's fullUrl (or a contained `#id`).
fn dangling(b: &Value) -> Vec<String> {
    let urls: HashSet<&str> = b["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["fullUrl"].as_str())
        .collect();
    fn walk<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::Object(m) => {
                if let Some(r) = m.get("reference").and_then(Value::as_str) {
                    out.push(r);
                }
                m.values().for_each(|c| walk(c, out));
            }
            Value::Array(a) => a.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
    let mut refs = Vec::new();
    walk(b, &mut refs);
    refs.into_iter()
        .filter(|r| !r.starts_with('#') && !urls.contains(r))
        .map(str::to_string)
        .collect()
}

async fn demo() -> anyhow::Result<(TestDb, tempfile::TempDir, BlobStore)> {
    ownhealth_ops::register();
    ownhealth_ingest_structured::register();
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let settings = db.settings(dir.path())?;
    let blobs = BlobStore::new(settings.blobs_path())?;
    assert!(seed_demo(&db.pool, &settings, &blobs, false).await?);
    Ok((db, dir, blobs))
}

async fn pid(db: &TestDb, slug: &str) -> anyhow::Result<Uuid> {
    Ok(sqlx::query_scalar(
        "select p.id from profile p join household h on h.id = p.household_id where h.name = $1 and p.slug = $2",
    )
    .bind(DEMO_HOUSEHOLD)
    .bind(slug)
    .fetch_one(&db.pool)
    .await?)
}

async fn export(db: &TestDb, profile: Uuid, lang: &str) -> anyhow::Result<Ips> {
    let mut tx = db.pool.begin().await?;
    let out = ips::build(&mut tx, profile, &all_scopes(), lang, as_of()).await?;
    tx.commit().await?;
    Ok(out)
}

/// (loinc, value, ucum unit, day) of a profile's current observations, sorted.
async fn values(db: &TestDb, profile: Uuid, ids: Option<&[Uuid]>) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "select concat_ws(' ', coalesce(loinc, '-'), coalesce(trim_scale(value_num)::text, '-'), coalesce(unit_ucum, '-'), \
         (effective_at at time zone 'UTC')::date) from observation where profile_id = $1 \
         and status = 'current' and ($2::uuid[] is null or id = any($2)) order by loinc, effective_at",
    )
    .bind(profile)
    .bind(ids)
    .fetch_all(&db.pool)
    .await?)
}

/// What a profile's current records say, without ids or provenance. A medicine reads as its name and
/// strength together, as FHIR carries them (one concept text).
async fn records(db: &TestDb, profile: Uuid) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "select 'condition ' || text || ' ' || coalesce(code, '-') from condition where profile_id = $1 and status = 'current' \
         union all select 'allergy ' || substance from allergy where profile_id = $1 and status = 'current' \
         union all select 'medication ' || concat_ws(' ', name, strength) || ' ' || coalesce(dose_text, '-') || ' ' || coalesce(started::text, '-') \
           from medication_statement where profile_id = $1 and status = 'current' \
         union all select 'immunization ' || vaccine || ' ' || coalesce(occurred::text, '-') from immunization \
           where profile_id = $1 and status = 'current' \
         union all select 'procedure ' || text || ' ' || coalesce(performed::text, '-') from procedure \
           where profile_id = $1 and status = 'current' order by 1",
    )
    .bind(profile)
    .fetch_all(&db.pool)
    .await?)
}

#[tokio::test]
async fn alex_exports_an_ips_that_imports_back_with_the_same_values_and_units() -> R {
    let (db, _dir, blobs) = demo().await?;
    let alex = pid(&db, "alex").await?;
    let out = export(&db, alex, "en").await?;
    let b = &out.bundle;
    assert_eq!(
        out.to_bytes()?,
        export(&db, alex, "en").await?.to_bytes()?,
        "two exports differ"
    );
    assert_eq!(
        (b["resourceType"].as_str(), b["type"].as_str()),
        (Some("Bundle"), Some("document"))
    );
    let comp = &b["entry"][0]["resource"];
    assert_eq!(
        (
            comp["resourceType"].as_str(),
            comp["type"]["coding"][0]["code"].as_str()
        ),
        (Some("Composition"), Some("60591-5"))
    );
    assert_eq!(comp["subject"]["reference"], format!("urn:uuid:{alex}"));
    // Alex has no procedure on record: IPS leaves an empty recommended section out
    let codes: Vec<String> = sections(b).iter().map(|s| code(s).to_string()).collect();
    assert_eq!(codes, SECTIONS[..5]);
    assert_eq!(dangling(b), Vec::<String>::new());
    // he has no allergy either: the required section says so with the IPS entry, never emptyReason
    let allergies = &sections(b)[1];
    assert!(allergies.get("emptyReason").is_none(), "{allergies}");
    assert!(no_info(target(b, &allergies["entry"][0]["reference"])), "{allergies}");
    // each other entry is a row: its id, its src in meta.source, and the Patient as its subject
    let entries = b["entry"].as_array().cloned().unwrap_or_default();
    let absent = entries.iter().filter(|e| no_info(&e["resource"])).count();
    assert_eq!(absent, 1);
    for e in &entries[2..] {
        let r = &e["resource"];
        assert_eq!(
            e["fullUrl"],
            format!("urn:uuid:{}", r["id"].as_str().unwrap_or_default())
        );
        assert!(Uuid::parse_str(r["id"].as_str().unwrap_or_default()).is_ok(), "{r}");
        if no_info(r) {
            continue;
        }
        let source = r["meta"]["source"].as_str().unwrap_or_default();
        assert!(source.starts_with("urn:ownhealth:src:src_"), "{r}");
        assert!(out.srcs.iter().any(|s| source.ends_with(s.as_str())), "{source}");
    }
    let results = &sections(b)[4];
    let obs: Vec<Uuid> = results["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["reference"].as_str()?.strip_prefix("urn:uuid:"))
        .filter_map(|i| Uuid::parse_str(i).ok())
        .collect();
    assert!(obs.len() >= 5, "Alex's Results section carries his lab rows: {results}");
    let records_in = entries.len() - 2 - obs.len() - absent;
    assert!(records_in >= 5, "{records_in} records");

    if let Ok(dir) = std::env::var("OWNHEALTH_FHIR_OUT") {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(format!("{dir}/ips-alex-en.json"), out.to_bytes()?)?;
        let carmen = export(&db, pid(&db, "carmen").await?, "es").await?;
        std::fs::write(format!("{dir}/ips-carmen-es.json"), carmen.to_bytes()?)?;
    }

    // import the Bundle into a fresh profile for the same person, in a new household
    let mut c = db.pool.acquire().await?;
    let row = get_profile(&mut c, alex).await?.expect("alex");
    let h = create_household(&mut c, "IPS restore").await?;
    let fresh = create_profile(
        &mut c,
        &ProfileIn {
            household_id: h,
            slug: "alex".into(),
            display_name: row.display_name.clone(),
            relationship: Some("self".into()),
            birth_date: row.birth_date,
            sex_at_birth: row.sex_at_birth.clone(),
            country: Some("ES".into()),
            user_id: None,
            unit_pref: None,
        },
    )
    .await?;
    let bytes = out.to_bytes()?;
    let sha = blobs.put_bytes(&bytes)?;
    let doc = create_document(
        &mut c,
        &NewDocument {
            filename: Some("ips-alex-en.json"),
            ..NewDocument::upload(h, Some(fresh), &sha, "application/fhir+json", bytes.len() as i64)
        },
    )
    .await?;
    let (bundle_id, entries_in) = read_entries(&bytes, false)?;
    let profile = get_profile(&mut c, fresh).await?.expect("fresh");
    let key = bundle_id.unwrap_or_default();
    let (summary, identity) = import_resources(&mut c, &entries_in, &profile, doc.id, &key, None, "FHIR").await?;
    assert_eq!(identity, "matched");
    // every resource is projected: none is "stored, not projected", none is dropped
    let held: Vec<(Option<String>, Vec<String>)> = sqlx::query_as(
        "select payload->>'projection', reasons from review_item where profile_id = $1 and status = 'open'",
    )
    .bind(fresh)
    .fetch_all(&mut *c)
    .await?;
    assert!(held.iter().all(|(p, _)| p.is_some()), "stored, not projected: {held:?}");
    // the "no information" entry says a list is empty: the importer skips it, nothing else
    assert_eq!(summary.skipped as usize, absent, "{summary:?}");
    assert_eq!(
        summary.reasons.get("skip:absent_unknown").copied().unwrap_or(0) as usize,
        absent
    );
    assert_eq!(
        (summary.accepted + summary.queued) as usize,
        obs.len() + records_in,
        "{summary:?}"
    );
    // a record typed by hand has no code, and an uncoded record waits for a person (the importer's
    // rule): those are the only ones held, and accepting them projects them as they were
    assert!(
        held.iter().all(|(_, r)| r == &vec!["unmapped".to_string()]),
        "held for another reason: {held:?}"
    );
    let user: Uuid = sqlx::query_scalar("select user_id from profile where id = $1 and user_id is not null")
        .bind(alex)
        .fetch_one(&mut *c)
        .await?;
    let items: Vec<Uuid> = sqlx::query_scalar("select id from review_item where profile_id = $1 and status = 'open'")
        .bind(fresh)
        .fetch_all(&mut *c)
        .await?;
    for item in items {
        ownhealth_core::review::accept(&mut c, item, user, None).await?;
    }
    drop(c);
    assert_eq!(values(&db, fresh, None).await?, values(&db, alex, Some(&obs)).await?);
    let back = records(&db, fresh).await?;
    let mut shown = Vec::new();
    for r in records(&db, alex).await? {
        if back.contains(&r) {
            shown.push(r);
        }
    }
    assert_eq!(back.len(), records_in, "{back:#?}");
    assert_eq!(back, shown, "each record came back as it was");
    Ok(())
}

#[tokio::test]
async fn lucia_s_empty_required_sections_carry_the_ips_no_information_entry() -> R {
    let (db, _dir, _blobs) = demo().await?;
    let lucia = pid(&db, "lucia").await?;
    for (lang, none) in [("en", "None on record."), ("es", "Nada registrado.")] {
        let b = export(&db, lucia, lang).await?.bundle;
        assert_eq!(b["entry"][0]["resource"]["language"], lang);
        let codes: Vec<String> = sections(&b).iter().map(|s| code(s).to_string()).collect();
        assert_eq!(
            codes[..3],
            REQUIRED,
            "the required sections come first and are always there"
        );
        for s in sections(&b).iter().filter(|s| REQUIRED.contains(&code(s))) {
            assert!(s.get("emptyReason").is_none(), "{s}");
            let entry = s["entry"].as_array().cloned().unwrap_or_default();
            assert_eq!(entry.len(), 1, "{s}");
            assert!(no_info(target(&b, &entry[0]["reference"])), "{s}");
            assert!(s["text"]["div"].as_str().unwrap_or_default().contains(none), "{s}");
        }
        // an empty recommended section is left out, never sent empty
        assert!(
            sections(&b)
                .iter()
                .all(|s| s["entry"].as_array().is_some_and(|e| !e.is_empty()))
        );
    }
    Ok(())
}

/// The printed twin: the summary sheet's `ips` view prints every row the Bundle carries, each with its
/// src id, procedures included.
#[tokio::test]
async fn carmen_s_ips_sheet_prints_every_row_the_bundle_carries() -> R {
    let (db, _dir, _blobs) = demo().await?;
    let carmen = pid(&db, "carmen").await?;
    let out = export(&db, carmen, "es").await?;
    let codes: Vec<String> = sections(&out.bundle).iter().map(|s| code(s).to_string()).collect();
    assert_eq!(codes, SECTIONS, "Carmen has a row in every section");
    let mut tx = db.pool.begin().await?;
    let opts = ownhealth_rules::summary::Options {
        lang: "es".into(),
        specialty: ips::SPECIALTY.into(),
        sensitive_open: false,
        as_of: as_of(),
    };
    let model = ownhealth_rules::summary::build(&mut tx, carmen, &all_scopes(), &opts).await?;
    tx.commit().await?;
    let sheet = ownhealth_ops::summary_pdf::render(&model);
    assert_eq!(sheet.cut, 0);
    let text = assert_text_layer(&sheet.pdf, |_| Ok(())).join("\n");
    assert!(text.contains("Resumen internacional del paciente (IPS)"), "{text}");
    assert!(
        text.contains("Procedimientos") && text.contains("Cataract surgery"),
        "{text}"
    );
    for src in &out.srcs {
        assert!(text.contains(src.as_str()), "{src} is in the Bundle, not on the sheet");
    }
    Ok(())
}

async fn env() -> anyhow::Result<Env> {
    Env::new(true).await
}

#[tokio::test]
async fn the_ips_export_needs_a_fresh_step_up_and_writes_an_audit_row() -> R {
    let env = env().await?;
    let body = json!({"fmt": "ips", "lang": "es"});
    let r = env.anon().post("/api/profiles/alex/export", &body).await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = env
        .stale(env.user("alex"))
        .await?
        .post("/api/profiles/alex/export", &body)
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "step_up_required"));
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post("/api/profiles/alex/export", &json!({"fmt": "ips", "lang": "fr"}))
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", r.json);
    let n = env
        .scalar_i64("select count(*) from audit_event where args_summary like 'fmt=ips%'")
        .await?;
    assert_eq!(n, 0, "a refused export writes no audit row");

    let r = alex.post("/api/profiles/alex/export", &body).await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.header("content-type"), "application/fhir+json");
    assert_eq!(r.header("cache-control"), "no-store");
    assert!(
        r.header("content-disposition")
            .starts_with("attachment; filename=\"ownhealth-ips-alex-es-"),
        "{}",
        r.header("content-disposition")
    );
    let b: Value = serde_json::from_slice(&r.bytes)?;
    assert_eq!(b["type"], "document");
    assert_eq!(b["entry"][0]["resource"]["title"], "Resumen internacional del paciente");
    let (via, kind, args): (String, Option<String>, Option<String>) = sqlx::query_as(
        "select via, resource_type, args_summary from audit_event where args_summary like 'fmt=ips%' and actor_id = $1",
    )
    .bind(env.user("alex"))
    .fetch_one(env.pool())
    .await?;
    assert_eq!((via.as_str(), kind.as_deref()), ("export", Some("ips")));
    assert_eq!(args.as_deref(), Some("fmt=ips;lang=es;entries=5"));
    Ok(())
}

#[tokio::test]
async fn a_caregiver_exports_and_a_viewer_gets_records_and_labs_withheld() -> R {
    let env = env().await?;
    // Alex is Carmen's caregiver: the IPS is the printed summary's twin, so it needs what printing does
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex.post("/api/profiles/carmen/export", &json!({"fmt": "ips"})).await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // Sam is a viewer of Alex: read:summary only, so every section is withheld and says so
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam
        .post("/api/profiles/alex/export", &json!({"fmt": "ips", "lang": "en"}))
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let b: Value = serde_json::from_slice(&r.bytes)?;
    let codes: Vec<String> = sections(&b).iter().map(|s| code(s).to_string()).collect();
    assert_eq!(
        codes, REQUIRED,
        "only the required sections, each saying it is withheld"
    );
    for s in sections(&b) {
        assert!(no_info(target(&b, &s["entry"][0]["reference"])), "{s}");
        assert!(
            s["text"]["div"].as_str().unwrap_or_default().contains("not shown"),
            "{s}"
        );
    }
    // the full zip stays the owner's
    let r = sam.post("/api/profiles/alex/export", &json!({"fmt": "zip"})).await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    Ok(())
}
