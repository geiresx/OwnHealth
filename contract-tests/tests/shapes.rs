//! The JSON shapes of every REST read the UI makes, on the demo family as the seed left it
//! (src/shape.rs: keys, number kinds, datetime formats, nulls). Golden files in `golden/`.
//!
//! These tests only read. Tests that write work on profiles they create, so the demo profiles
//! keep the shapes recorded here.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{Client, Res};
use serde_json::Value;

async fn ok(c: &Client, path: &str) -> anyhow::Result<Value> {
    let r: Res = c.get(path).await?;
    anyhow::ensure!(
        r.status == 200,
        "GET {path}: HTTP {} {}",
        r.status,
        String::from_utf8_lossy(&r.body)
    );
    Ok(r.json)
}

async fn check(c: &Client, name: &str, path: &str) -> anyhow::Result<Value> {
    let v = ok(c, path).await?;
    assert_shape(name, &v).with_context(|| format!("GET {path}"))?;
    Ok(v)
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn me_profiles_household_and_auth_state() -> anyhow::Result<()> {
    let c = Client::new()?;
    // other tests add relatives and members; the shapes are of the demo family
    let mut me = ok(&c, "/api/me").await?;
    demo_only(&mut me["profiles"], "slug");
    assert_shape("me", &me)?;
    let mut profiles = ok(&c, "/api/profiles").await?;
    demo_only(&mut profiles, "slug");
    assert_shape("profiles", &profiles)?;
    let mut hh = ok(&c, "/api/household").await?;
    demo_only(&mut hh["profiles"], "slug");
    demo_only(&mut hh["grants"], "profile");
    let demo_users: Vec<Value> = hh["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["display_name"] == "Alex Rivera" || m["display_name"] == "Sam Rivera")
        .map(|m| m["user_id"].clone())
        .collect();
    if let Some(a) = hh["members"].as_array_mut() {
        a.retain(|m| demo_users.contains(&m["user_id"]));
    }
    if let Some(a) = hh["grants"].as_array_mut() {
        a.retain(|g| demo_users.contains(&g["user_id"]));
    }
    assert_shape("household", &hh)?;
    check(&c, "auth_state_dev", "/api/auth/state").await?;
    check(&c, "rules", "/api/rules").await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn status_and_dashboard_of_every_demo_profile() -> anyhow::Result<()> {
    let c = Client::new()?;
    for p in ["alex", "sam", "lucia", "carmen"] {
        check(
            &c,
            &format!("status_{p}"),
            &format!("/api/profiles/{p}/status"),
        )
        .await?;
        check(
            &c,
            &format!("dashboard_{p}"),
            &format!("/api/profiles/{p}/dashboard"),
        )
        .await?;
    }
    check(
        &c,
        "status_alex_en_conventional",
        "/api/profiles/alex/status?lang=en&units=conventional",
    )
    .await?;
    check(
        &c,
        "dashboard_alex_en",
        "/api/profiles/alex/dashboard?lang=en",
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn observations_and_trends() -> anyhow::Result<()> {
    let c = Client::new()?;
    check(&c, "observations_alex", "/api/profiles/alex/observations").await?;
    check(
        &c,
        "observations_alex_si_en",
        "/api/profiles/alex/observations?units=si&lang=en",
    )
    .await?;
    check(
        &c,
        "observations_alex_q_ldl",
        "/api/profiles/alex/observations?q=ldl",
    )
    .await?;
    check(&c, "observations_lucia", "/api/profiles/lucia/observations").await?;
    check(&c, "trend_alex_ldl", "/api/profiles/alex/trend?code=ldl_c").await?;
    check(
        &c,
        "trend_alex_loinc_conventional",
        "/api/profiles/alex/trend?code=LOINC:13457-7&units=conventional&window=P10Y",
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn records_wearables_and_timeline() -> anyhow::Result<()> {
    let c = Client::new()?;
    for kind in [
        "condition",
        "medication",
        "allergy",
        "immunization",
        "procedure",
    ] {
        check(
            &c,
            &format!("records_alex_{kind}"),
            &format!("/api/profiles/alex/records/{kind}"),
        )
        .await?;
    }
    check(
        &c,
        "wearables_summary_alex",
        "/api/profiles/alex/wearables/summary",
    )
    .await?;
    let summary = ok(&c, "/api/profiles/alex/wearables/summary").await?;
    let metric = first_metric(&summary).context("no wearable metric in the summary")?;
    check(
        &c,
        "wearables_metric_alex",
        &format!("/api/profiles/alex/wearables/{metric}"),
    )
    .await?;
    check(
        &c,
        "wearables_metric_alex_hour",
        &format!("/api/profiles/alex/wearables/{metric}?resolution=hour&window=P7D"),
    )
    .await?;
    check(&c, "timeline_alex", "/api/profiles/alex/timeline").await?;
    check(
        &c,
        "timeline_alex_es_labs",
        "/api/profiles/alex/timeline?lang=es&types=lab",
    )
    .await?;
    Ok(())
}

const DEMO_SLUGS: [&str; 4] = ["alex", "sam", "lucia", "carmen"];

fn demo_only(list: &mut Value, key: &str) {
    if let Some(a) = list.as_array_mut() {
        a.retain(|p| p[key].as_str().is_some_and(|s| DEMO_SLUGS.contains(&s)));
    }
}

fn first_metric(summary: &Value) -> Option<String> {
    for key in ["metrics", "items", "cards"] {
        if let Some(a) = summary[key].as_array() {
            for m in a {
                for k in ["metric", "key"] {
                    if let Some(s) = m[k].as_str() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn documents_pages_and_sources() -> anyhow::Result<()> {
    let c = Client::new()?;
    let docs = check(&c, "documents_alex", "/api/profiles/alex/documents").await?;
    let lab = docs["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["doc_type"] == "lab_report" && d["pages"].as_i64() == Some(2))
        .context("no two-page lab report in the demo")?;
    let id = lab["id"].as_str().context("doc id")?;
    check(&c, "document_lab", &format!("/api/documents/{id}")).await?;
    check(
        &c,
        "document_page_words",
        &format!("/api/documents/{id}/pages/1/words"),
    )
    .await?;
    let obs = ok(&c, "/api/profiles/alex/observations?q=ldl").await?;
    let src = obs["items"][0]["src"]
        .as_str()
        .context("no src on the LDL row")?;
    check(&c, "source_lab", &format!("/api/sources/{src}")).await?;
    check(
        &c,
        "source_lab_text",
        &format!("/api/sources/{src}?include_text=true"),
    )
    .await?;
    check(
        &c,
        "documents_alex_q",
        "/api/profiles/alex/documents?q=LabNorte",
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn genome_and_drug_check() -> anyhow::Result<()> {
    let c = Client::new()?;
    check(&c, "genome_alex", "/api/profiles/alex/genome").await?;
    check(
        &c,
        "genome_alex_e1_en",
        "/api/profiles/alex/genome?min_evidence=E1&lang=en",
    )
    .await?;
    check(&c, "genome_lucia", "/api/profiles/lucia/genome").await?;
    check(
        &c,
        "genome_variant_rsid",
        "/api/profiles/alex/genome/variant?rsid=rs4244285",
    )
    .await?;
    check(
        &c,
        "genome_variant_region",
        "/api/profiles/alex/genome/variant?region=chr10:94781000-94782000",
    )
    .await?;
    for drug in ["clopidogrel", "simvastatin", "ibuprofen", "warfarin"] {
        check(
            &c,
            &format!("drug_check_alex_{drug}"),
            &format!("/api/profiles/alex/drug-check?drug={drug}"),
        )
        .await?;
    }
    check(
        &c,
        "drug_check_alex_en",
        "/api/profiles/alex/drug-check?drug=clopidogrel&lang=en",
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn review_queue_of_the_demo() -> anyhow::Result<()> {
    let c = Client::new()?;
    let list = check(&c, "review_alex", "/api/review?profile=alex").await?;
    // every open item of each kind, so the shape does not depend on which one comes first
    for kind in ["candidate_row", "agent_proposal"] {
        let ids: Vec<String> = list["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["kind"] == kind)
            .filter_map(|i| i["id"].as_str().map(str::to_string))
            .collect();
        anyhow::ensure!(!ids.is_empty(), "no {kind} item in the demo");
        let mut items = Vec::new();
        for id in ids {
            items.push(ok(&c, &format!("/api/review/{id}")).await?);
        }
        assert_shape(&format!("review_items_{kind}"), &Value::Array(items))?;
    }
    check(&c, "review_alex_all", "/api/review?profile=alex&status=").await?;
    Ok(())
}
