//! The prevention pack and the ownhealth.prevention/v1 document (data/prevention/): the loader's
//! refusals, the goldens of the four Riveras and a synthetic US profile at the docs' date, the hash,
//! the vaccine join, and one home for each lab gap.
//!
//! Re-record the golden with `OWNHEALTH_RECORD_PREVENTION=1 cargo test -p ownhealth-rules --test prevention`.

mod common;

use std::path::{Path, PathBuf};

use common::{Seeded, as_of, seeded};
use ownhealth_core::access::{ProfileIn, all_scopes, create_profile, scope_set};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use ownhealth_rules::prevention::{self, build_document, load_dir};
use ownhealth_rules::{build_state, load_pack};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

type R = anyhow::Result<()>;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/prevention");
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/prevention.json");
const SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];

async fn record(conn: &mut PgConnection, kind: &str, body: Value) -> anyhow::Result<String> {
    let src = SrcRef::New(SrcIn::manual(None));
    let done = add_record(
        conn,
        &RecordIn::from_json(kind, body)?,
        &src,
        RecordOptions::new("person"),
    )
    .await?;
    Ok(done.src_id)
}

/// A synthetic US profile in the Riveras' household: a woman of 52 with a mammogram, a Tdap and a
/// vaccine name nothing knows.
async fn us_profile(s: &Seeded, conn: &mut PgConnection) -> anyhow::Result<Uuid> {
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(s.pid("alex"))
        .fetch_one(&mut *conn)
        .await?;
    let pid = create_profile(
        &mut *conn,
        &ProfileIn {
            household_id: household,
            slug: "jordan".into(),
            display_name: "Jordan Test".into(),
            relationship: Some("other".into()),
            birth_date: chrono::NaiveDate::from_ymd_opt(1974, 3, 10),
            sex_at_birth: Some("female".into()),
            country: Some("US".into()),
            ..ProfileIn::default()
        },
    )
    .await?;
    record(
        conn,
        "procedure",
        json!({"profile_id": pid, "text": "Screening mammogram", "performed": "2025-01-15"}),
    )
    .await?;
    record(
        conn,
        "immunization",
        json!({"profile_id": pid, "vaccine": "Tdap", "occurred": "2014-09-02"}),
    )
    .await?;
    record(
        conn,
        "immunization",
        json!({"profile_id": pid, "vaccine": "Yellow fever", "occurred": "2019-04-01"}),
    )
    .await?;
    Ok(pid)
}

fn keys(doc: &Value) -> Vec<String> {
    doc["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| i["key"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn item<'a>(doc: &'a Value, key: &str) -> &'a Value {
    doc["items"]
        .as_array()
        .and_then(|a| a.iter().find(|i| i["key"] == key))
        .unwrap_or(&Value::Null)
}

#[tokio::test]
async fn goldens_for_the_riveras_and_a_us_profile() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let jordan = us_profile(&s, &mut tx).await?;
    let mut got = serde_json::Map::new();
    for slug in SLUGS {
        got.insert(slug.into(), build_document(&mut tx, s.pid(slug), as_of()).await?);
    }
    got.insert("jordan".into(), build_document(&mut tx, jordan, as_of()).await?);
    // the US profile's src ids are generated, and so its hash: the golden keeps their shape only
    let mut got = Value::Object(got);
    let jordan_src = |v: &mut Value| {
        let text = serde_json::to_string(v).unwrap_or_default();
        let re = regex::Regex::new(r#""src_[a-z0-9_]+""#).ok();
        let text = re.map_or(text.clone(), |re| re.replace_all(&text, "\"src_x\"").into_owned());
        *v = serde_json::from_str(&text).unwrap_or(Value::Null);
    };
    jordan_src(&mut got["jordan"]);
    got["jordan"]["prevention_hash"] = json!("sha256:generated");
    if std::env::var_os("OWNHEALTH_RECORD_PREVENTION").is_some() {
        std::fs::write(GOLDEN, serde_json::to_string_pretty(&got)? + "\n")?;
    }
    let want: Value = serde_json::from_str(&std::fs::read_to_string(GOLDEN)?)?;
    for slug in SLUGS.iter().chain(&["jordan"]) {
        assert_eq!(got[slug], want[slug], "{slug}");
    }
    // what each person sees, in words
    let carmen = &got["carmen"];
    for k in [
        "es.cancer.breast",
        "es.cancer.colorectal",
        "es.vac.influenza",
        "es.vac.zoster",
    ] {
        assert_eq!(item(carmen, k)["status"], "due", "carmen {k}");
    }
    assert!(!keys(carmen).contains(&"es.cancer.cervix_hpv".to_string()));
    let alex = &got["alex"];
    assert_eq!(item(alex, "es.vac.td")["last"]["date"], "2016-05-10");
    assert_eq!(item(alex, "es.vac.td")["status"], "up_to_date");
    assert_eq!(item(alex, "es.vac.influenza")["last"]["date"], "2025-10-21");
    assert!(!keys(alex).iter().any(|k| k.contains("breast") || k.contains("cervix")));
    assert_eq!(item(&got["sam"], "es.cancer.cervix_hpv")["status"], "due");
    let jordan = &got["jordan"];
    assert_eq!(item(jordan, "us.cancer.breast")["status"], "upcoming");
    assert_eq!(item(jordan, "us.cancer.breast")["due"], "2027-01-15");
    assert_eq!(item(jordan, "us.vac.td")["status"], "overdue");
    assert_eq!(jordan["unmatched"][0]["name"], "Yellow fever");
    assert_eq!(jordan["rules"], "us.yaml prevention-us 0.2.0");
    tx.rollback().await?;
    Ok(())
}

/// Each lab gap has one home, the rule pack: Alex's never-measured hepatitis C shows once, under
/// the pack's rule id, and no country row evaluates it again.
#[tokio::test]
async fn hcv_shows_exactly_once() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let doc = build_document(&mut tx, s.pid("alex"), as_of()).await?;
    let hcv: Vec<String> = keys(&doc).into_iter().filter(|k| k.contains("hcv")).collect();
    assert_eq!(hcv, vec!["infect.hcv.once_adult".to_string()]);
    let dash = ownhealth_rules::dashboard(&mut tx, s.pid("alex"), as_of(), "es", &all_scopes()).await?;
    let on_card: Vec<&Value> = dash["prevention_due"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["rule"] == "infect.hcv.once_adult")
        .collect();
    assert_eq!(on_card.len(), 1);
    assert_eq!(on_card[0]["title"], "Anticuerpos hepatitis C");
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn the_hash_is_stable_and_language_free_and_the_state_hash_does_not_move() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pack = load_pack()?;
    for slug in SLUGS {
        let before = build_state(&mut tx, s.pid(slug), as_of(), None).await?["state_hash"].clone();
        let a = build_document(&mut tx, s.pid(slug), as_of()).await?;
        let b = build_document(&mut tx, s.pid(slug), as_of()).await?;
        assert_eq!(a["prevention_hash"], b["prevention_hash"], "{slug}");
        assert_eq!(
            prevention::prevention_hash(&a)?,
            a["prevention_hash"].as_str().unwrap_or_default()
        );
        let en = prevention::render(&a, &pack, "en", &all_scopes())?;
        let es = prevention::render(&a, &pack, "es", &all_scopes())?;
        assert_eq!(en["prevention_hash"], a["prevention_hash"]);
        assert_eq!(es["prevention_hash"], a["prevention_hash"]);
        if !keys(&a).is_empty() {
            assert_ne!(en["items"], es["items"], "{slug}: the words change with the language");
        }
        let after = build_state(&mut tx, s.pid(slug), as_of(), None).await?["state_hash"].clone();
        assert_eq!(before, after, "{slug}");
        assert_eq!(before, common::EXPECTED["bodies"][slug]["state_hash"], "{slug}");
    }
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn a_caller_without_labs_or_records_sees_less() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pack = load_pack()?;
    let doc = build_document(&mut tx, s.pid("alex"), as_of()).await?;
    let mut no_labs = all_scopes();
    no_labs.remove("read:labs");
    let r = prevention::render(&doc, &pack, "en", &no_labs)?;
    let shown = r["items"].as_array().map_or(0, Vec::len);
    assert!(shown > 0);
    assert!(r["items"].as_array().into_iter().flatten().all(|i| i["area"] != "lab"));
    assert_eq!(r["withheld"].as_u64().unwrap_or(0) as usize + shown, keys(&doc).len());
    assert_eq!(r["withheld_scopes"], json!(["read:labs"]));
    let summary = prevention::render(&doc, &pack, "en", &scope_set(["read:summary"]))?;
    assert_eq!(summary["items"], json!([]));
    assert_eq!(summary["withheld_scopes"], json!(["read:labs", "read:records"]));
    assert!(summary.get("unmatched").is_none());
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn the_vaccine_join_matches_whole_words_and_lists_what_it_does_not_know() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": carmen, "vaccine": "Td (tétanos, difteria)", "occurred": "2025-02-01"}),
    )
    .await?;
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": carmen, "vaccine": "Vacuna inventada", "occurred": "2025-03-01"}),
    )
    .await?;
    let doc = build_document(&mut tx, carmen, as_of()).await?;
    assert_eq!(item(&doc, "es.vac.td")["status"], "up_to_date");
    assert_eq!(item(&doc, "es.vac.td")["last"]["date"], "2025-02-01");
    let unmatched: Vec<&Value> = doc["unmatched"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|u| &u["name"])
        .collect();
    assert_eq!(unmatched, vec![&json!("Vacuna inventada")]);
    // what the Due screen's form records for a row counts for that row, in either language
    let pack = load_pack()?;
    for (lang, key) in [("es", "es.cancer.breast"), ("en", "es.cancer.colorectal")] {
        let shown = prevention::render(&doc, &pack, lang, &all_scopes())?;
        let rec = &item(&shown, key)["record"];
        assert_eq!(rec["kind"], "procedure", "{key}");
        record(
            &mut tx,
            "procedure",
            json!({"profile_id": carmen, "text": rec["text"], "performed": "2026-10-01"}),
        )
        .await?;
        let after = build_document(&mut tx, carmen, as_of()).await?;
        assert_eq!(item(&after, key)["status"], "up_to_date", "{key} recorded as {rec}");
    }
    // a word inside another word is not a match: "Tdap" is Td, "Stdx" is not
    assert!(prevention::words("Stdx booster").iter().all(|w| w != "td"));
    tx.rollback().await?;
    Ok(())
}

/// A copy of data/prevention in a temporary directory, removed on drop.
struct Copy(PathBuf);

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_data() -> anyhow::Result<Copy> {
    let dir = std::env::temp_dir().join(format!("oh-prevention-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    for entry in std::fs::read_dir(DATA)? {
        let p = entry?.path();
        std::fs::copy(&p, dir.join(p.file_name().unwrap_or_default()))?;
    }
    Ok(Copy(dir))
}

fn mutate(dir: &Path, file: &str, from: &str, to: &str) -> anyhow::Result<()> {
    let p = dir.join(file);
    let body = std::fs::read_to_string(&p)?;
    assert!(body.contains(from), "{file}: {from:?} is not in the file");
    std::fs::write(&p, body.replacen(from, to, 1))?;
    Ok(())
}

#[test]
fn the_shipped_files_load_and_every_row_is_cited() -> R {
    let pack = load_pack()?;
    let p = load_dir(Path::new(DATA), &pack)?;
    assert_eq!(p.countries.keys().cloned().collect::<Vec<_>>(), ["ES", "US"]);
    for c in p.countries.values() {
        assert!(!c.rows.is_empty());
        for r in &c.rows {
            assert!(r.cite.starts_with("https://"), "{}", r.id);
        }
    }
    assert_eq!(
        p.countries["US"].pack_rules,
        vec![("us.infect.hcv".into(), "infect.hcv.once_adult".into())]
    );
    // the compiled-in copy is the same files
    assert_eq!(
        prevention::load(&pack)?.countries["ES"].version,
        p.countries["ES"].version
    );
    Ok(())
}

#[test]
fn the_loader_refuses_each_broken_file() -> R {
    let pack = load_pack()?;
    let cite = "https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/breast-cancer-screening";
    let cases: [(&str, &str, &str, String, &str); 16] = [
        (
            "http cite",
            "us.yaml",
            cite,
            cite.replace("https://", "http://"),
            "no https `cite`",
        ),
        (
            "no cite",
            "us.yaml",
            &format!("    cite: {cite}\n"),
            String::new(),
            "no https `cite`",
        ),
        (
            "no reviewed date",
            "es.yaml",
            "reviewed: 2026-11-11\n",
            String::new(),
            "no `reviewed` date",
        ),
        (
            "missing es template",
            "templates.yaml",
            "    es: \"La guía incluye {doses} dosis a partir de los {from} años.\"\n",
            String::new(),
            "no `es` text",
        ),
        (
            "missing reviewer",
            "us.yaml",
            "reviewer: \"OwnHealth maintainers (not clinicians)\"\n",
            String::new(),
            "no `reviewer`",
        ),
        (
            "a row overlapping a pack screening group",
            "evidence.yaml",
            "    loinc: [\"10524-7\"]\n",
            "    loinc: [\"10524-7\"]\n    alias_groups: [hcv_ab]\n".into(),
            "the rule pack already checks",
        ),
        (
            "an unknown pack rule",
            "us.yaml",
            "pack_rule: infect.hcv.once_adult",
            "pack_rule: infect.hcv.twice".into(),
            "not a never_measured or recency rule",
        ),
        (
            "an unreadable end age",
            "es.yaml",
            "ages: [35, 65]",
            "ages: [35, sixty]".into(),
            "end age",
        ),
        (
            "an end age below the start",
            "es.yaml",
            "ages: [25, 34]",
            "ages: [25, 20]".into(),
            "end age",
        ),
        ("a zero interval", "es.yaml", "months: 36", "months: 0".into(), "months"),
        (
            "an unreadable group interval",
            "es.yaml",
            "interval_for: {colonoscopy: 180}",
            "interval_for: {colonoscopy: soon}".into(),
            "interval_for",
        ),
        (
            "a yes-or-no group interval",
            "es.yaml",
            "interval_for: {colonoscopy: 180}",
            "interval_for: {colonoscopy: true}".into(),
            "interval_for",
        ),
        (
            "a group interval outside the evidence",
            "es.yaml",
            "interval_for: {colonoscopy: 180}",
            "interval_for: {hpv_test: 180}".into(),
            "interval_for",
        ),
        (
            "a row that needs an answer nobody can give",
            "us.yaml",
            "needs: {smoking_history: \"yes\"}",
            "needs: {pack_years: \"yes\"}".into(),
            "not one of the answer_keys",
        ),
        (
            "an answer that is not yes or no",
            "es.yaml",
            "unless: {hysterectomy: \"yes\"}",
            "unless: {hysterectomy: \"maybe\"}".into(),
            "is \"yes\" or \"no\"",
        ),
        (
            "an answer key with no question",
            "templates.yaml",
            "  hysterectomy:\n    question:",
            "  hysterectomy_old:\n    question:".into(),
            "answer hysterectomy has no question",
        ),
    ];
    for (what, file, from, to, reason) in cases {
        let copy = copy_data()?;
        mutate(&copy.0, file, from, &to)?;
        match load_dir(&copy.0, &pack) {
            Ok(_) => panic!("{what}: loaded"),
            Err(e) => assert!(e.to_string().contains(reason), "{what}: {e}"),
        }
    }
    Ok(())
}

fn day(s: &str) -> chrono::NaiveDate {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}

/// A record tagged sensitive is left out for every caller, as the state engine leaves it out: it
/// never counts as done and its name is never listed as unmatched.
#[tokio::test]
async fn sensitive_records_never_count_and_are_never_listed() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let sam = s.pid("sam");
    record(
        &mut tx,
        "procedure",
        json!({"profile_id": sam, "text": "Citología cervical", "performed": "2026-01-10",
               "sensitivity": ["reproductive"]}),
    )
    .await?;
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": sam, "vaccine": "Vacuna inventada", "occurred": "2026-01-11",
               "sensitivity": ["sexual_health"]}),
    )
    .await?;
    let doc = build_document(&mut tx, sam, as_of()).await?;
    let hpv = item(&doc, "es.cancer.cervix_hpv");
    assert_eq!(hpv["status"], "due", "{hpv}");
    assert!(hpv.get("last").is_none(), "{hpv}");
    assert!(!doc.to_string().contains("Vacuna inventada"), "{doc}");
    tx.rollback().await?;
    Ok(())
}

/// Off season, a seasonal row with no dose this season claims nothing: it shows as upcoming once the
/// next season is near, and before that not at all. A dose this season reads up to date.
#[tokio::test]
async fn a_seasonal_row_off_season_never_claims_up_to_date_without_a_dose() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    let may = build_document(&mut tx, carmen, day("2027-05-01")).await?;
    assert!(!keys(&may).contains(&"es.vac.influenza".to_string()), "{may}");
    let aug = build_document(&mut tx, carmen, day("2027-08-01")).await?;
    let flu = item(&aug, "es.vac.influenza");
    assert_eq!(
        (&flu["status"], &flu["due"]),
        (&json!("upcoming"), &json!("2027-10-01"))
    );
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": carmen, "vaccine": "Gripe", "occurred": "2026-11-20"}),
    )
    .await?;
    let may = build_document(&mut tx, carmen, day("2027-05-01")).await?;
    assert_eq!(item(&may, "es.vac.influenza")["status"], "up_to_date");
    tx.rollback().await?;
    Ok(())
}

/// An item any of whose counted facts is a lab value needs read:labs, even when its latest fact is
/// a procedure.
#[tokio::test]
async fn an_item_with_any_lab_fact_needs_read_labs() -> R {
    use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let sam = s.pid("sam");
    let src = ownhealth_core::provenance::create_src(
        &mut tx,
        &SrcIn {
            profile_id: Some(sam),
            label: Some("LabNorte · 2024-01-10".into()),
            ..SrcIn::new("manual")
        },
    )
    .await?;
    let obs = ObservationIn::from_json(json!({
        "profile_id": sam, "printed_name": "VPH alto riesgo", "printed_value": "Negativo",
        "loinc": "21440-3", "alias_group": "hpv_hr", "derivation_version": "test",
        "effective_at": "2024-01-10T08:00:00+00:00",
    }))?;
    commit_observation(&mut tx, &obs, &SrcRef::Id(src), CommitOptions::new("auto")).await?;
    record(
        &mut tx,
        "procedure",
        json!({"profile_id": sam, "text": "Citología cervical", "performed": "2025-01-10"}),
    )
    .await?;
    let doc = build_document(&mut tx, sam, as_of()).await?;
    let hpv = item(&doc, "es.cancer.cervix_hpv");
    assert_eq!(hpv["last"]["date"], "2025-01-10", "{hpv}");
    assert_eq!(hpv["labs"], true, "{hpv}");
    let pack = load_pack()?;
    let mut no_labs = all_scopes();
    no_labs.remove("read:labs");
    let shown = prevention::render(&doc, &pack, "en", &no_labs)?;
    assert!(item(&shown, "es.cancer.cervix_hpv").is_null());
    assert_eq!(shown["withheld_scopes"], json!(["read:labs"]));
    tx.rollback().await?;
    Ok(())
}

/// A cytology of another organ is not cervical screening.
#[tokio::test]
async fn a_urine_cytology_is_not_cervical_screening() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let sam = s.pid("sam");
    record(
        &mut tx,
        "procedure",
        json!({"profile_id": sam, "text": "Citología de orina", "performed": "2026-01-10"}),
    )
    .await?;
    let doc = build_document(&mut tx, sam, as_of()).await?;
    assert_eq!(item(&doc, "es.cancer.cervix_hpv")["status"], "due");
    tx.rollback().await?;
    Ok(())
}

/// A next date after the row's age window is not sent: the guideline lists nothing then.
#[tokio::test]
async fn no_next_date_after_the_age_window() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    record(
        &mut tx,
        "procedure",
        json!({"profile_id": carmen, "text": "Colonoscopia", "performed": "2020-03-01"}),
    )
    .await?;
    let doc = build_document(&mut tx, carmen, as_of()).await?;
    let crc = item(&doc, "es.cancer.colorectal");
    assert_eq!(crc["status"], "up_to_date", "{crc}");
    assert!(crc.get("due").is_none(), "{crc}");
    tx.rollback().await?;
    Ok(())
}

/// A series counts a dose only when it comes at least gap_days after the previous counted one.
#[tokio::test]
async fn a_series_counts_doses_the_gap_apart() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    for (name, date) in [("Herpes zóster", "2026-11-01"), ("Shingrix", "2026-11-02")] {
        record(
            &mut tx,
            "immunization",
            json!({"profile_id": carmen, "vaccine": name, "occurred": date}),
        )
        .await?;
    }
    let doc = build_document(&mut tx, carmen, as_of()).await?;
    let z = item(&doc, "es.vac.zoster");
    assert_eq!(
        (&z["status"], &z["due"]),
        (&json!("upcoming"), &json!("2026-12-27")),
        "{z}"
    );
    tx.rollback().await?;
    Ok(())
}

/// The status section names its own fields (`due_total`), and leaves out the date of an open item
/// never recorded: that date is where the age window opened, not a deadline.
#[tokio::test]
async fn the_status_section_sends_no_window_start_as_a_due_date() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pack = load_pack()?;
    let carmen = s.pid("carmen");
    let body = build_state(&mut tx, carmen, as_of(), None).await?;
    let sec = prevention::status_section(&mut tx, carmen, as_of(), &body, &pack, &all_scopes())
        .await?
        .unwrap_or_default();
    assert!(sec["due_total"].as_u64().unwrap_or(0) >= 6, "{sec}");
    assert!(sec.get("total").is_none(), "{sec}");
    let items = sec["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 3, "{sec}");
    assert!(items.iter().all(|i| i.get("due").is_none()), "{sec}");
    tx.rollback().await?;
    Ok(())
}

/// The dashboard says which scope its Due card lacks, so an empty card is not read as nothing due.
#[tokio::test]
async fn the_dashboard_names_the_scope_the_due_card_lacks() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let viewer = ownhealth_rules::dashboard(&mut tx, s.pid("sam"), as_of(), "es", &scope_set(["read:summary"])).await?;
    assert_eq!(viewer["prevention_due"], json!([]));
    assert_eq!(viewer["prevention_withheld"], json!(["read:records"]));
    let full = ownhealth_rules::dashboard(&mut tx, s.pid("sam"), as_of(), "es", &all_scopes()).await?;
    assert_eq!(full["prevention_withheld"], json!([]));
    tx.rollback().await?;
    Ok(())
}

/// The cited schedules list a conjugate pneumococcal vaccine and the recombinant zoster vaccine: a
/// polysaccharide dose (CVX 33) or live zoster doses (CVX 121) do not count, and are listed instead,
/// even when their names read like the group's.
#[tokio::test]
async fn ppsv23_and_live_zoster_do_not_count() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    for (name, code, date) in [
        ("PPSV23", "33", "2026-01-05"),
        ("Zostavax", "121", "2025-01-05"),
        ("Zostavax", "121", "2025-06-05"),
        ("Pneumococcal polysaccharide vaccine", "33", "2026-02-05"),
        ("Zoster vaccine live", "121", "2025-02-05"),
        ("Zoster vaccine live", "121", "2025-07-05"),
    ] {
        record(
            &mut tx,
            "immunization",
            json!({"profile_id": carmen, "vaccine": name, "code_system": "CVX", "code": code, "occurred": date}),
        )
        .await?;
    }
    let doc = build_document(&mut tx, carmen, as_of()).await?;
    assert_eq!(item(&doc, "es.vac.pneumococcal")["status"], "due");
    assert_eq!(item(&doc, "es.vac.zoster")["status"], "due");
    let unmatched: Vec<&str> = doc["unmatched"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["name"].as_str())
        .collect();
    assert_eq!(
        unmatched,
        [
            "Zostavax",
            "Zoster vaccine live",
            "Zostavax",
            "Zoster vaccine live",
            "PPSV23",
            "Pneumococcal polysaccharide vaccine"
        ],
        "{doc}"
    );
    tx.rollback().await?;
    Ok(())
}

/// A recognised CVX code decides the group on its own, so a name that reads like another vaccine
/// does not make one dose count twice. A record with no code that names the polysaccharide or the
/// live product does not count either.
#[tokio::test]
async fn a_known_cvx_code_wins_over_the_name() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": carmen, "vaccine": "Pneumococcal vaccine", "code_system": "CVX", "code": "187", "occurred": "2026-01-05"}),
    )
    .await?;
    for (name, date) in [
        ("Pneumococcal polysaccharide vaccine (PPSV23)", "2026-02-05"),
        ("Vacuna neumocócica polisacárida 23-valente", "2026-02-06"),
        ("Zoster vaccine live", "2025-02-05"),
        ("Vacuna de herpes zóster atenuada", "2025-03-05"),
    ] {
        record(
            &mut tx,
            "immunization",
            json!({"profile_id": carmen, "vaccine": name, "occurred": date}),
        )
        .await?;
    }
    let doc = build_document(&mut tx, carmen, as_of()).await?;
    assert_eq!(item(&doc, "es.vac.zoster")["last"]["date"], "2026-01-05", "{doc}");
    let pneumo = item(&doc, "es.vac.pneumococcal");
    assert_eq!(pneumo["status"], "due", "{pneumo}");
    assert!(pneumo.get("last").is_none(), "{pneumo}");
    let unmatched: Vec<&str> = doc["unmatched"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u["name"].as_str())
        .collect();
    assert_eq!(
        unmatched,
        [
            "Zoster vaccine live",
            "Vacuna de herpes zóster atenuada",
            "Pneumococcal polysaccharide vaccine (PPSV23)",
            "Vacuna neumocócica polisacárida 23-valente"
        ],
        "{doc}"
    );
    tx.rollback().await?;
    Ok(())
}

/// A country row that names a pack rule gives that rule's item the country's source and cite; the
/// pack stays the one that evaluates it.
#[tokio::test]
async fn a_pack_rule_named_by_a_country_row_carries_its_cite() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let jordan = us_profile(&s, &mut tx).await?;
    let doc = build_document(&mut tx, jordan, as_of()).await?;
    let hcv = item(&doc, "infect.hcv.once_adult");
    assert_eq!(hcv["source"], "USPSTF 2020 (B)", "{hcv}");
    assert_eq!(
        hcv["cite"],
        "https://www.uspreventiveservicestaskforce.org/uspstf/recommendation/hepatitis-c-screening"
    );
    // a Spanish profile keeps the pack's own cite
    let alex = build_document(&mut tx, s.pid("alex"), as_of()).await?;
    assert!(item(&alex, "infect.hcv.once_adult").get("source").is_none());
    tx.rollback().await?;
    Ok(())
}

/// The hash covers immunizations, procedures, encounters and the person's decisions and answers,
/// which the state body does not hold. A caller without read:records sees none of them, so it gets
/// no hash either: the same hash before and after would tell it whether such a record changed.
#[tokio::test]
async fn a_caller_without_read_records_gets_no_prevention_hash() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let pack = load_pack()?;
    let doc = build_document(&mut tx, s.pid("alex"), as_of()).await?;
    let summary = prevention::render(&doc, &pack, "en", &scope_set(["read:summary"]))?;
    assert!(summary.get("prevention_hash").is_none(), "{summary}");
    let mut no_labs = all_scopes();
    no_labs.remove("read:labs");
    let r = prevention::render(&doc, &pack, "en", &no_labs)?;
    assert_eq!(r["prevention_hash"], doc["prevention_hash"]);
    tx.rollback().await?;
    Ok(())
}

/// A seasonal row whose start age is a few weeks away, off season and with no dose on record, claims
/// nothing: it is not up to date, because up to date needs a dose.
#[tokio::test]
async fn an_off_season_row_before_its_start_age_claims_nothing_without_a_dose() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let household: Uuid = sqlx::query_scalar("select household_id from profile where id = $1")
        .bind(s.pid("alex"))
        .fetch_one(&mut *tx)
        .await?;
    let pid = create_profile(
        &mut tx,
        &ProfileIn {
            household_id: household,
            slug: "riley".into(),
            display_name: "Riley Test".into(),
            relationship: Some("other".into()),
            birth_date: chrono::NaiveDate::from_ymd_opt(2008, 6, 1),
            sex_at_birth: Some("male".into()),
            country: Some("US".into()),
            ..ProfileIn::default()
        },
    )
    .await?;
    // 19 on 2027-06-01, 47 days after this date; the season opens on 09-01
    let doc = build_document(&mut tx, pid, day("2027-04-15")).await?;
    let flu = item(&doc, "us.vac.influenza");
    assert!(flu.is_null(), "{flu}");
    tx.rollback().await?;
    Ok(())
}

/// A procedure on `on` and a current `done` decision about `key` that points at it.
async fn done_with(tx: &mut PgConnection, pid: Uuid, key: &str, on: chrono::NaiveDate) -> R {
    done_with_named(tx, pid, key, "Blood draw, private clinic", on).await
}

/// A procedure named `text` on `on` and a current `done` decision about `key` that points at it.
async fn done_with_named(tx: &mut PgConnection, pid: Uuid, key: &str, text: &str, on: chrono::NaiveDate) -> R {
    let src = record(
        tx,
        "procedure",
        json!({"profile_id": pid, "text": text, "performed": on.to_string()}),
    )
    .await?;
    let id: Uuid = sqlx::query_scalar("select id from procedure where src_id = $1")
        .bind(&src)
        .fetch_one(&mut *tx)
        .await?;
    decide_done(tx, pid, key, Some(id), None).await
}

/// A current `done` decision about `key` that points at a record (a procedure) or a document.
async fn decide_done(tx: &mut PgConnection, pid: Uuid, key: &str, record: Option<Uuid>, doc: Option<Uuid>) -> R {
    sqlx::query(
        "update prevention_decision set status = 'superseded', superseded_at = clock_timestamp() \
         where profile_id = $1 and item_key = $2 and status = 'current'",
    )
    .bind(pid)
    .bind(key)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "insert into prevention_decision (profile_id, item_key, kind, evidence_table, evidence_id, document_id) \
         values ($1, $2, 'done', $3, $4, $5)",
    )
    .bind(pid)
    .bind(key)
    .bind(record.map(|_| "procedure"))
    .bind(record)
    .bind(doc)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// A done decision on the cervical HPV row whose evidence is a cytology the record already counts
/// keeps the cytology's 36-month interval; a document, which names no group, gets the row's
/// shortest interval, never the HPV test's 60 months.
#[tokio::test]
async fn done_on_a_cytology_keeps_its_own_interval() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let jordan = us_profile(&s, &mut tx).await?;
    let key = "us.cancer.cervix_hpv";
    done_with_named(&mut tx, jordan, key, "Pap test", day("2026-01-01")).await?;
    let doc = build_document(&mut tx, jordan, as_of()).await?;
    assert_eq!(item(&doc, key)["due"], "2029-01-01", "{doc}");

    let file: Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, mime, size_bytes, collected_at) \
         select household_id, id, decode(repeat('cd', 32), 'hex'), 'application/pdf', 1, '2026-02-01' \
         from profile where id = $1 returning id",
    )
    .bind(jordan)
    .fetch_one(&mut *tx)
    .await?;
    decide_done(&mut tx, jordan, key, None, Some(file)).await?;
    let doc = build_document(&mut tx, jordan, as_of()).await?;
    assert_eq!(item(&doc, key)["due"], "2029-02-01", "{doc}");
    tx.rollback().await?;
    Ok(())
}

/// A series counts its doses in date order, so a done decision that proves an earlier dose than
/// the one on record completes it when the two are the gap apart.
#[tokio::test]
async fn done_with_an_earlier_dose_completes_a_series() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    let key = "es.vac.zoster";
    record(
        &mut tx,
        "immunization",
        json!({"profile_id": carmen, "vaccine": "Shingrix", "occurred": "2026-11-01"}),
    )
    .await?;
    let file: Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, mime, size_bytes, collected_at) \
         select household_id, id, decode(repeat('ef', 32), 'hex'), 'application/pdf', 1, '2026-09-01' \
         from profile where id = $1 returning id",
    )
    .bind(carmen)
    .fetch_one(&mut *tx)
    .await?;
    decide_done(&mut tx, carmen, key, None, Some(file)).await?;
    let doc = build_document(&mut tx, carmen, as_of()).await?;
    assert_eq!(item(&doc, key)["status"], "up_to_date", "{doc}");
    tx.rollback().await?;
    Ok(())
}

/// One source may hold several records. A done decision that points at a record the rules do not
/// match still counts on its own day when another record of the same source counts on another day.
#[tokio::test]
async fn done_on_a_record_that_shares_its_source_still_counts() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let carmen = s.pid("carmen");
    let key = "es.vac.zoster";
    let src = record(
        &mut tx,
        "immunization",
        json!({"profile_id": carmen, "vaccine": "Shingrix", "occurred": "2026-09-01"}),
    )
    .await?;
    let body = json!({"profile_id": carmen, "text": "Vaccination visit", "performed": "2026-11-01"});
    let done = add_record(
        &mut tx,
        &RecordIn::from_json("procedure", body)?,
        &SrcRef::Id(src),
        RecordOptions::new("person"),
    )
    .await?;
    let id: Uuid = sqlx::query_scalar("select id from procedure where src_id = $1 and performed = '2026-11-01'")
        .bind(&done.src_id)
        .fetch_one(&mut *tx)
        .await?;
    decide_done(&mut tx, carmen, key, Some(id), None).await?;
    let doc = build_document(&mut tx, carmen, as_of()).await?;
    assert_eq!(item(&doc, key)["status"], "up_to_date", "{doc}");
    tx.rollback().await?;
    Ok(())
}

/// The name the Due screen fills in to record a group is one its own aliases match, in every
/// language, so the record it saves counts.
#[test]
fn every_group_name_matches_its_own_aliases() -> R {
    let pack = load_pack()?;
    let p = load_dir(Path::new(DATA), &pack)?;
    let mut misses = Vec::new();
    for g in p.groups.values() {
        for (lang, name) in g.name.as_object().into_iter().flatten() {
            let w = prevention::words(name.as_str().unwrap_or_default());
            let hit =
                |list: &Vec<Vec<String>>| list.iter().any(|a| !a.is_empty() && w.windows(a.len()).any(|x| x == a));
            if !hit(&g.aliases) || hit(&g.aliases_not) {
                misses.push(format!("{} {lang}: {name}", g.id));
            }
        }
    }
    assert!(misses.is_empty(), "{misses:?}");
    Ok(())
}

/// Lung screening counts only a low-dose CT of the chest: a low-dose CT of another part of the body,
/// such as one for kidney stones, says nothing about the lungs.
#[test]
fn only_a_chest_low_dose_ct_counts_for_lung_screening() -> R {
    let pack = load_pack()?;
    let p = load_dir(Path::new(DATA), &pack)?;
    let g = &p.groups["ldct_chest"];
    let hit = |name: &str| {
        let w = prevention::words(name);
        let any = |list: &Vec<Vec<String>>| list.iter().any(|a| !a.is_empty() && w.windows(a.len()).any(|x| x == a));
        any(&g.aliases) && !any(&g.aliases_not)
    };
    for yes in [
        "Low-dose CT chest",
        "LDCT lung screening",
        "Low dose chest CT",
        "TAC de tórax de baja dosis",
        "CT chest low dose",
        "Chest CT low dose",
    ] {
        assert!(hit(yes), "{yes}");
    }
    for no in [
        "Low-dose CT abdomen",
        "LDCT abdomen",
        "Low dose CT kidney stones",
        "TC de baja dosis abdominal",
        "Low dose computed tomography of the sinuses",
    ] {
        assert!(!hit(no), "{no}");
    }
    Ok(())
}

/// A retest marked done is hidden while its evidence covers it: from a day no older than the last
/// value the item reports, for the rule's window. Then the next retest is due again, shown as if
/// nothing had been decided; evidence older than the last value covers nothing.
#[tokio::test]
async fn done_on_a_retest_lasts_one_window_from_its_evidence() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let alex = s.pid("alex");
    let key = "stale:recency.thyroid:tsh";
    let before = build_document(&mut tx, alex, as_of()).await?;
    let stale = item(&before, key).clone();
    let (last, due) = (
        day(stale["last"]["date"].as_str().unwrap_or_default()),
        day(stale["due"].as_str().unwrap_or_default()),
    );
    let window = due - last;
    let listed = |doc: &Value, list: &str| {
        doc[list]
            .as_array()
            .and_then(|a| a.iter().find(|i| i["key"] == key))
            .cloned()
    };

    done_with(&mut tx, alex, key, last - chrono::Duration::days(1)).await?;
    let doc = build_document(&mut tx, alex, as_of()).await?;
    let shown = listed(&doc, "items").expect("older evidence covers nothing");
    assert!(shown.get("decision").is_none(), "{shown}");

    let on = as_of() - chrono::Duration::days(3);
    done_with(&mut tx, alex, key, on).await?;
    for d in [as_of(), on + window] {
        let doc = build_document(&mut tx, alex, d).await?;
        assert!(listed(&doc, "items").is_none(), "{d}: still listed");
        assert_eq!(
            listed(&doc, "hidden").map(|h| h["decision"]["kind"].clone()),
            Some(json!("done")),
            "{d}"
        );
    }
    let later = on + window + chrono::Duration::days(1);
    let doc = build_document(&mut tx, alex, later).await?;
    let back = listed(&doc, "items").expect("due again once the window has run");
    assert!(back.get("decision").is_none(), "{back}");
    assert!(listed(&doc, "hidden").is_none(), "{doc}");
    tx.rollback().await?;
    Ok(())
}
