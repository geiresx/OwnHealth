//! The genome over REST and MCP on an imported facts file: sensitive findings stay locked until
//! the profile's switches say otherwise, the minor rule, consent. Python: tests/genome/test_genome_api.py,
//! tests/genome/test_genome_findings.py, tests/mcp/test_mcp_access.py.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{ALL_SCOPES, Client, Mcp, fixture};
use serde_json::{Value, json};

async fn with_genome(c: &Client) -> anyhow::Result<String> {
    let p = c.new_relative().await?;
    let mut g: Value = serde_json::from_slice(&fixture("genome/alex-rivera.genome-facts.json")?)?;
    g["subject"]["sample_id"] = json!(format!("GN-{p}"));
    let (_, jobs) = c
        .import(
            &p,
            &[("facts.json", &serde_json::to_vec(&g)?, "application/json")],
        )
        .await?;
    anyhow::ensure!(
        jobs.first().is_some_and(|j| j["state"] == "done"),
        "{jobs:?}"
    );
    Ok(p)
}

fn genes(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["gene"].as_str().map(str::to_string))
        .collect();
    out.sort();
    out
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn sensitive_findings_follow_the_two_switches() -> anyhow::Result<()> {
    let c = Client::new()?;
    let p = with_genome(&c).await?;
    let g = c
        .get(&format!("/api/profiles/{p}/genome?min_evidence=E4"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        g["consent"],
        json!({"ui": false, "agents": false, "can_change": false}),
        "a guardian of an adult cannot consent"
    );
    assert_eq!(g["is_minor"], false);
    let locked = g["locked"].as_array().cloned().unwrap_or_default();
    assert!(
        !locked.is_empty(),
        "the fixture has sensitive findings: {}",
        g["locked"]
    );
    let locked_genes: Vec<String> = locked
        .iter()
        .filter_map(|l| l["gene"].as_str().map(str::to_string))
        .collect();
    for lg in &locked_genes {
        assert!(!genes(&g).contains(lg), "{lg} is locked but listed");
    }
    assert!(g["withheld"]["sensitive"].as_i64().unwrap_or(0) >= locked.len() as i64);

    // the consent route is for the owner (or a minor's guardian); the household switch works for a guardian
    let r = c
        .post(
            &format!("/api/profiles/{p}/genome/consent"),
            &json!({"ui": true}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("forbidden")));
    c.put(
        &format!("/api/profiles/{p}/sensitive"),
        &json!({"ui": true}),
    )
    .await?
    .expect(200)?;
    let open = c
        .get(&format!("/api/profiles/{p}/genome?min_evidence=E4"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(open["locked"], json!([]));
    for lg in &locked_genes {
        assert!(
            genes(&open).contains(lg),
            "{lg} should show once the UI switch is on"
        );
    }
    assert_shape("genome_sensitive_open", &open)?;

    // agents need their own switch and read:sensitive
    let (_, all) = c
        .token(json!({"scopes": ALL_SCOPES, "profiles": [p]}))
        .await?;
    let m = Mcp::new(&all)?;
    let env = m
        .ok(
            "get_genome_findings",
            json!({"profile": p, "detail": "full", "min_evidence": "E4"}),
        )
        .await?;
    for lg in &locked_genes {
        assert!(
            !env["data"].to_string().contains(lg.as_str()),
            "{lg} reached an agent with the agents switch off"
        );
    }
    assert!(env["withheld"]["sensitive"].as_i64().unwrap_or(0) >= 1);
    c.put(
        &format!("/api/profiles/{p}/sensitive"),
        &json!({"agents": true}),
    )
    .await?
    .expect(200)?;
    let env = m
        .ok(
            "get_genome_findings",
            json!({"profile": p, "detail": "full", "min_evidence": "E4"}),
        )
        .await?;
    for lg in &locked_genes {
        assert!(
            env["data"].to_string().contains(lg.as_str()),
            "{lg} should reach an agent with both switch and scope"
        );
    }
    let no_sens: Vec<&str> = ALL_SCOPES
        .iter()
        .copied()
        .filter(|s| *s != "read:sensitive")
        .collect();
    let (_, plain) = c.token(json!({"scopes": no_sens, "profiles": [p]})).await?;
    let env = Mcp::new(&plain)?
        .ok(
            "get_genome_findings",
            json!({"profile": p, "detail": "full", "min_evidence": "E4"}),
        )
        .await?;
    for lg in &locked_genes {
        assert!(
            !env["data"].to_string().contains(lg.as_str()),
            "{lg} without read:sensitive"
        );
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn a_minors_genome_and_consent() -> anyhow::Result<()> {
    let c = Client::new()?;
    let g = c.get("/api/profiles/lucia/genome").await?.expect(200)?.json;
    assert_eq!(g["is_minor"], true);
    assert_eq!(
        g["locked"],
        json!([]),
        "a minor's locked list stays empty: the minor rule decides"
    );
    assert_eq!(
        g["consent"]["can_change"], true,
        "a guardian of a minor may set consent"
    );
    let r = c
        .post("/api/profiles/lucia/genome/consent", &json!({"ui": false}))
        .await?
        .expect(200)?;
    assert_shape("genome_consent", &r.json)?;
    let (_, plain) = c
        .token(json!({"scopes": ALL_SCOPES, "profiles": ["lucia"]}))
        .await?;
    let m = Mcp::new(&plain)?;
    let env = m
        .ok(
            "get_genome_findings",
            json!({"profile": "lucia", "detail": "full"}),
        )
        .await?;
    let cats: Vec<&str> = env["data"]["findings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["category"].as_str())
        .collect();
    assert!(
        !cats.contains(&"carrier"),
        "the minor rule withholds carrier status: {cats:?}"
    );
    assert!(cats.contains(&"pgx"), "pgx shows for a minor: {cats:?}");
    assert_eq!(
        env["withheld"]["minor_rule"], 3,
        "the control: three findings are held back by the minor rule"
    );
    assert_eq!(g["withheld"], json!({"sensitive": 0, "minor_rule": 3}));
    let r = c
        .post("/api/profiles/sam/genome/consent", &json!({"ui": true}))
        .await?;
    assert_eq!(r.status, 403);
    let audit = c.audit(500).await?;
    assert!(
        audit.iter().any(|e| e["action"] == "consent_change"
            && e["tool"] == "POST /api/profiles/{p}/genome/consent")
    );
    let read = audit
        .iter()
        .find(|e| e["tool"] == "GET /api/profiles/{p}/genome" && e["profile"] == "lucia")
        .context("genome read audited")?;
    assert_eq!(read["resource_type"], "genome_finding");
    Ok(())
}
