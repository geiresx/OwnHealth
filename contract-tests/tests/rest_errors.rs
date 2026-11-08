//! Status codes and error bodies the UI shows, exactly as 0.1.1 sends them, including FastAPI's
//! own 422 validation bodies. Python: tests/security/test_authz.py, tests/test_app.py and the
//! route tests of every lane.

use ownhealth_contract_tests::Client;
use serde_json::{Value, json};

async fn body(c: &Client, path: &str) -> anyhow::Result<(u16, Value)> {
    c.get_json(path).await
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn error_bodies_of_the_reads() -> anyhow::Result<()> {
    let c = Client::new()?;
    let cases: Vec<(&str, u16, Value)> = vec![
        (
            "/api/sources/src_nope",
            404,
            json!({"detail": {"code": "unknown", "message": "Unknown source."}}),
        ),
        (
            "/api/profiles/alex/drug-check?drug=notadrug",
            404,
            json!({"detail": {"code": "drug_unknown", "message": "This drug name is not in the drug table."}}),
        ),
        (
            "/api/profiles/alex/genome/variant",
            400,
            json!({"detail": {"code": "bad_input", "message": "Give exactly one of rsid, hgvs or region.", "fix": null}}),
        ),
        (
            "/api/profiles/alex/genome/variant?rsid=rs1",
            400,
            json!({"detail": {"code": "rsid_unknown", "message": "rs1 is not in the curated rsID index, and remote lookups are off (they leak interest).",
                              "fix": "Ask by position instead: region chrN:pos-pos (GRCh38)."}}),
        ),
        (
            "/api/profiles/alex/genome/variant?region=chr1:1-5000000",
            400,
            json!({"detail": {"code": "region_too_large", "message": "A region spans at most 1 Mb.", "fix": "Split it into smaller regions."}}),
        ),
        (
            "/api/profiles/alex/genome?min_evidence=E9",
            400,
            json!({"detail": {"code": "bad_input", "message": "min_evidence is one of ['E1', 'E2', 'E3', 'E4']"}}),
        ),
        (
            "/api/profiles/sam/observations",
            403,
            json!({"detail": {"code": "insufficient_scope", "message": "This needs read:labs, read:vitals or read:wearables.", "allowed": []}}),
        ),
        (
            "/api/profiles/sam/genome",
            403,
            json!({"detail": {"code": "insufficient_scope", "message": "This needs read:genome.", "allowed": []}}),
        ),
        (
            "/api/profiles/carmen/genome",
            403,
            json!({"detail": {"code": "insufficient_scope", "message": "This needs read:genome.", "allowed": []}}),
        ),
        (
            "/api/documents/00000000-0000-4000-8000-000000000000",
            404,
            json!({"detail": {"code": "unknown", "message": "Unknown document."}}),
        ),
        (
            "/api/profiles/alex/trend?code=ldl_c&window=bogus",
            422,
            json!({"detail": {"code": "bad_input", "message": "window is P<n>D, P<n>M, P<n>Y or all."}}),
        ),
        (
            "/api/no-such-route",
            404,
            json!({"detail": {"code": "not_found", "message": "Not found."}}),
        ),
        // FastAPI's validation bodies
        (
            "/api/profiles/alex/drug-check",
            422,
            json!({"detail": [{"type": "missing", "loc": ["query", "drug"], "msg": "Field required", "input": null}]}),
        ),
        (
            "/api/profiles/alex/wearables/summary?days=3",
            422,
            json!({"detail": [{"type": "greater_than_equal", "loc": ["query", "days"], "msg": "Input should be greater than or equal to 7", "input": "3", "ctx": {"ge": 7}}]}),
        ),
    ];
    let mut failures = Vec::new();
    for (path, status, want) in cases {
        let (s, got) = body(&c, path).await?;
        if (s, &got) != (status, &want) {
            failures.push(format!("GET {path}: {s} {got}\n    want {status} {want}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    // an ungranted profile names what the caller can use
    let (s, v) = body(&c, "/api/profiles/nobody/status").await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (404, json!("profile_not_granted"))
    );
    assert_eq!(v["detail"]["message"], "Profile 'nobody' is not available.");
    let allowed = ownhealth_contract_tests::strings(&v["detail"]["allowed"]);
    for slug in ["alex", "carmen", "lucia", "sam"] {
        assert!(allowed.contains(&slug.to_string()), "{allowed:?}");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn profile_references_slug_me_and_uuid() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (s, me) = body(&c, "/api/profiles/me/status").await?;
    assert_eq!((s, me["profile"].clone()), (200, json!("alex")));
    let profiles = c.get("/api/profiles").await?.expect(200)?.json;
    let id = profiles[0]["id"].as_str().unwrap_or_default().to_string();
    assert_eq!(profiles[0]["slug"], "alex", "the caller is listed first");
    let (s, by_id) = body(&c, &format!("/api/profiles/{id}/status")).await?;
    assert_eq!((s, by_id["profile"].clone()), (200, json!("alex")));
    assert_eq!(by_id["state_hash"], me["state_hash"]);
    // a viewer of Sam sees Sam's summary but nothing behind other scopes
    let (s, sam) = body(&c, "/api/profiles/sam/status").await?;
    assert_eq!((s, sam["profile"].clone()), (200, json!("sam")));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn filters_of_the_list_reads() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, v) = body(&c, "/api/profiles/alex/observations?category=vital-signs").await?;
    assert_eq!(v, json!({"items": []}));
    let (_, v) = body(
        &c,
        "/api/profiles/alex/observations?from=2026-09-01&to=2026-09-30",
    )
    .await?;
    let items = v["items"].as_array().cloned().unwrap_or_default();
    assert!(!items.is_empty());
    assert!(items.iter().all(|o| {
        o["date"]
            .as_str()
            .is_some_and(|d| ("2026-09-01"..="2026-09-30").contains(&d))
    }));
    let (_, all) = body(&c, "/api/profiles/alex/observations").await?;
    let dates: Vec<&str> = all["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["date"].as_str())
        .collect();
    assert!(dates.windows(2).all(|w| w[0] >= w[1]), "newest first");
    let (_, v) = body(&c, "/api/profiles/alex/documents?type=lab_report&limit=2").await?;
    assert_eq!(v["items"].as_array().map(Vec::len), Some(2));
    let (_, v) = body(
        &c,
        "/api/profiles/alex/timeline?types=lab,genome&from=2026-01-01",
    )
    .await?;
    let ev = v["events"].as_array().cloned().unwrap_or_default();
    assert!(!ev.is_empty());
    assert!(
        ev.iter()
            .all(|e| (e["type"] == "lab" || e["type"] == "genome")
                && e["date"].as_str().is_some_and(|d| d >= "2026-01-01")),
        "{v}"
    );
    let (_, then) = body(&c, "/api/profiles/alex/status?as_of=2025-01-01").await?;
    assert_eq!(then["as_of"], "2025-01-01");
    let (_, now) = body(&c, "/api/profiles/alex/status").await?;
    assert_ne!(then["state_hash"], now["state_hash"]);
    Ok(())
}
