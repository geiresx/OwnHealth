//! The eight MCP tools on the demo family: the envelope, the shapes, the genome and scope gates,
//! state_hash stability, identity modes, cursors and budgets. Python: tests/mcp/test_mcp_tools.py,
//! test_mcp_real_services.py, test_mcp_access.py, test_mcp_identity.py, test_mcp_budget.py.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{ALL_SCOPES, Client, ENVELOPE_KEYS, Mcp, assert_envelope, src_ids};
use serde_json::{Value, json};

const NO_GENOME: [&str; 8] = [
    "read:documents_text",
    "read:labs",
    "read:records",
    "read:sensitive",
    "read:summary",
    "read:vitals",
    "read:wearables",
    "write:propose",
];

fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("get_health_status", json!({"profile": "alex"})),
        (
            "search_observations",
            json!({"profile": "alex", "query": "LDL", "detail": "full"}),
        ),
        (
            "search_observations",
            json!({"profile": "alex", "query": "LDL"}),
        ),
        (
            "get_trend",
            json!({"profile": "alex", "code": "13457-7", "window": "all"}),
        ),
        (
            "check_drug",
            json!({"profile": "alex", "drug": "simvastatin"}),
        ),
        (
            "get_genome_findings",
            json!({"profile": "alex", "detail": "full"}),
        ),
        ("get_genome_findings", json!({"profile": "alex"})),
        (
            "lookup_variant",
            json!({"profile": "alex", "rsid": "rs4244285"}),
        ),
    ]
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn every_tool_answers_in_the_envelope_with_its_shape() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    for (i, (name, args)) in calls().into_iter().enumerate() {
        let r = m.call(name, args.clone()).await?;
        anyhow::ensure!(!r.is_error, "{name} {args}: {}", r.sc);
        assert_envelope(&r.sc)?;
        let keys: Vec<&str> =
            r.sc.as_object()
                .context("env")?
                .keys()
                .map(String::as_str)
                .collect();
        assert_eq!(keys, ENVELOPE_KEYS, "envelope key order");
        assert_eq!(r.sc["profile"], "alex");
        assert_eq!(r.sc["identity_mode"], "full");
        let notice = if name == "check_drug" {
            "Information from your records, not medical advice. Discuss with your prescriber."
        } else {
            "Information from your records, not a diagnosis."
        };
        assert_eq!(r.sc["notice"], notice, "{name}");
        assert!(
            r.text
                .starts_with(&format!("OwnHealth {name} · profile alex · as of ")),
            "{name}: {}",
            &r.text[..80.min(r.text.len())]
        );
        assert_shape(&format!("mcp_{i}_{name}"), &r.sc)?;
    }
    // get_source on a fact the status cites
    let st = m
        .ok("get_health_status", json!({"profile": "alex"}))
        .await?;
    let src = src_ids(&st["data"])
        .into_iter()
        .next()
        .context("a src in the status")?;
    let s = m.ok("get_source", json!({"src_id": src})).await?;
    assert_envelope(&s)?;
    assert_eq!(s["data"]["src_id"], json!(src));
    assert_shape("mcp_get_source", &s)?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn the_status_pages_stay_under_2kb_whole() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, plain) = c.token(json!({"scopes": ALL_SCOPES, "lang": "en"})).await?;
    let m = Mcp::new(&plain)?;
    let (mut flags, mut cursor, mut pages) = (Vec::new(), None::<String>, 0);
    loop {
        let mut args = json!({"profile": "alex"});
        if let Some(cur) = &cursor {
            args["cursor"] = json!(cur);
        }
        let env = m.ok("get_health_status", args).await?;
        let size = serde_json::to_vec(&env)?.len();
        assert!(size < 2048, "page {pages}: {size} bytes");
        flags.extend(env["data"]["flags"].as_array().cloned().unwrap_or_default());
        pages += 1;
        cursor = env["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() || pages > 20 {
            break;
        }
    }
    let ranks: Vec<i64> = flags.iter().filter_map(|f| f["rank"].as_i64()).collect();
    assert_eq!(ranks, (1..=flags.len() as i64).collect::<Vec<_>>());
    for f in &flags {
        let keys: Vec<&String> = f
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        assert!(
            keys.iter()
                .all(|k| ["rank", "kind", "rule", "code", "text", "src"].contains(&k.as_str())),
            "{f}"
        );
    }
    assert!(pages >= 1);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn state_hash_is_the_same_for_every_token_language_unit_and_mode() -> anyhow::Result<()> {
    let c = Client::new()?;
    let today = c.get("/api/profiles/alex/status").await?.expect(200)?.json;
    let rest_hash = today["state_hash"]
        .as_str()
        .context("REST state_hash")?
        .to_string();
    let as_of = today["as_of"].as_str().context("as_of")?.to_string();
    let mut texts = std::collections::BTreeSet::new();
    for (lang, units, mode, scopes) in [
        ("en", "si", "full", ALL_SCOPES.to_vec()),
        ("es", "as_reported", "pseudonymous", ALL_SCOPES.to_vec()),
        ("es", "conventional", "minimal", ALL_SCOPES.to_vec()),
        ("en", "si", "full", vec!["read:summary"]),
        ("es", "si", "full", NO_GENOME.to_vec()),
    ] {
        let (_, plain) = c
            .token(json!({"scopes": scopes, "lang": lang, "units": units, "identity_mode": mode}))
            .await?;
        let env = Mcp::new(&plain)?
            .ok(
                "get_health_status",
                json!({"profile": "alex", "as_of": as_of}),
            )
            .await?;
        assert_eq!(
            (env["lang"].clone(), env["identity_mode"].clone()),
            (json!(lang), json!(mode))
        );
        assert_eq!(
            env["data"]["state_hash"],
            json!(rest_hash),
            "{lang} {units} {mode} {scopes:?}"
        );
        if let Some(t) = env["data"]["flags"][0]["text"].as_str() {
            texts.insert(t.to_string());
        }
    }
    assert!(texts.len() >= 2, "en and es render differently: {texts:?}");
    // REST in another language and unit system: same hash
    for q in ["lang=en", "units=conventional", "lang=es&units=si"] {
        let other = c
            .get(&format!("/api/profiles/alex/status?{q}"))
            .await?
            .expect(200)?
            .json;
        assert_eq!(other["state_hash"], json!(rest_hash), "{q}");
    }
    let re = regex::Regex::new(r"^sha256:[0-9a-f]{64}$")?;
    assert!(re.is_match(&rest_hash));
    // the read is audited with the hash it showed
    let audit = c.audit(500).await?;
    assert!(
        audit
            .iter()
            .any(|e| e["resource_ids"] == json!([rest_hash])),
        "no audit row names the state_hash"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn without_read_genome_no_genome_fact_appears_anywhere() -> anyhow::Result<()> {
    let c = Client::new()?;
    let all = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let g = all
        .ok(
            "get_genome_findings",
            json!({"profile": "alex", "detail": "full"}),
        )
        .await?;
    let genome_srcs: Vec<String> = src_ids(&g["data"]);
    assert!(
        !genome_srcs.is_empty(),
        "the positive control: Alex has genome findings"
    );
    // the control: with read:genome, check_drug shows the SLCO1B1 phenotype
    let drug = all
        .ok(
            "check_drug",
            json!({"profile": "alex", "drug": "simvastatin"}),
        )
        .await?;
    assert!(drug.to_string().contains("Decreased Function"), "{drug}");

    let none = c.mcp(&NO_GENOME, &["alex"]).await?;
    let tools = none.tool_names().await?;
    assert!(
        !tools
            .iter()
            .any(|t| t == "get_genome_findings" || t == "lookup_variant"),
        "{tools:?}"
    );
    for (name, args) in calls().into_iter().take(5) {
        let env = none.ok(name, args).await?;
        let s = env.to_string();
        assert!(
            !s.to_lowercase().contains("decreased function"),
            "{name} leaks a genotype result"
        );
        for g in &genome_srcs {
            assert!(!s.contains(g.as_str()), "{name} cites genome src {g}");
        }
    }
    let r = none
        .call("get_genome_findings", json!({"profile": "alex"}))
        .await?;
    assert_eq!(r.code(), Some("scope_missing"));
    // a genome src is unknown to a token without read:genome
    let r = none
        .call("get_source", json!({"src_id": genome_srcs[0]}))
        .await?;
    assert_eq!(r.code(), Some("unknown"), "{}", r.sc);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn lookups_hold_the_sensitive_gate_and_count_against_the_quota() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let env = m
        .ok(
            "lookup_variant",
            json!({"profile": "alex", "rsid": "rs4244285"}),
        )
        .await?;
    let d = &env["data"];
    assert_eq!(d["variant"]["gene"], "CYP2C19");
    assert_eq!(
        (
            d["calls"][0]["gt"].clone(),
            d["calls"][0]["zygosity"].clone()
        ),
        (json!("0/1"), json!("het"))
    );
    assert_eq!(d["quota"], json!({"used_today": 1, "daily_cap": 200}));
    // a sensitive site with the agents switch off: withheld, counted, not crashed
    let env = m
        .ok(
            "lookup_variant",
            json!({"profile": "alex", "hgvs": "NC_000019.10:g.44908684T>C"}),
        )
        .await?;
    assert_eq!(env["data"]["withheld"], "withheld_sensitive");
    assert_eq!(env["data"]["calls"], json!([]));
    assert_eq!(
        env["data"]["variant"]["rsid"],
        Value::Null,
        "a withheld finding never resolves an rsID"
    );
    assert!(env["withheld"]["sensitive"].as_i64().unwrap_or(0) >= 1);
    assert_eq!(env["data"]["quota"]["used_today"], 2);
    let r = m
        .call("lookup_variant", json!({"profile": "alex", "rsid": "rs1"}))
        .await?;
    assert_eq!(r.code(), Some("rsid_unknown"));
    assert_eq!(
        r.sc["fix"],
        "Ask by position instead: region chrN:pos-pos (GRCh38)."
    );
    let r = m.call("lookup_variant", json!({"profile": "alex"})).await?;
    assert_eq!(r.code(), Some("bad_input"), "oneOf rsid | hgvs | region");
    let r = m
        .call(
            "lookup_variant",
            json!({"profile": "alex", "region": "chr1:1-2000000"}),
        )
        .await?;
    assert!(r.is_error, "a region over 1 Mb is refused: {}", r.sc);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn tool_errors_have_one_shape() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    for (name, args, code) in [
        (
            "get_health_status",
            json!({"profile": "sam"}),
            "profile_not_granted",
        ),
        (
            "get_health_status",
            json!({"profile": "alex", "as_of": "yesterday"}),
            "bad_input",
        ),
        (
            "search_observations",
            json!({"profile": "alex", "query": ""}),
            "bad_input",
        ),
        (
            "search_observations",
            json!({"profile": "alex", "query": "ldl", "extra": 1}),
            "bad_input",
        ),
        ("get_source", json!({"src_id": "not-a-src"}), "bad_input"),
        (
            "get_source",
            json!({"src_id": "src_doesnotexist"}),
            "unknown",
        ),
        (
            "get_trend",
            json!({"profile": "alex", "code": "13457-7", "window": "forever"}),
            "bad_input",
        ),
        (
            "get_health_status",
            json!({"profile": "alex", "cursor": "garbage"}),
            "bad_input",
        ),
    ] {
        let r = m.call(name, args.clone()).await?;
        assert!(r.is_error, "{name} {args}");
        assert_eq!(r.code(), Some(code), "{name} {args}: {}", r.sc);
        let keys: Vec<&String> = r.sc.as_object().context("error body")?.keys().collect();
        assert!(
            keys.len() >= 3 && keys[..3] == ["code", "message", "fix"],
            "{keys:?}"
        );
        let text: Value = serde_json::from_str(&r.text)?;
        assert_eq!(text["isError"], true);
        assert_eq!(text["code"], json!(code));
    }
    let r = m
        .call("get_health_status", json!({"profile": "sam"}))
        .await?;
    assert_eq!(r.sc["allowed"], json!(["alex"]));
    assert_eq!(r.sc["fix"], "Use one of: alex.");
    // an unknown drug: 0.1.1 answers internal_error here (REST answers 404 drug_unknown)
    let r = m
        .call(
            "check_drug",
            json!({"profile": "alex", "drug": "notadrugatall"}),
        )
        .await?;
    assert!(r.is_error);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn get_source_text_needs_documents_text_and_other_profiles_are_unknown() -> anyhow::Result<()>
{
    let c = Client::new()?;
    let obs = c
        .get("/api/profiles/alex/observations?q=ldl")
        .await?
        .expect(200)?
        .json;
    let src = obs["items"][0]["src"].as_str().context("ldl src")?;
    let full = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let env = full
        .ok("get_source", json!({"src_id": src, "include_text": true}))
        .await?;
    let wrapped = ownhealth_contract_tests::untrusted(&env["data"]);
    assert!(
        wrapped
            .iter()
            .any(|(o, t)| o == "pdf_text_layer" && t.contains("Colesterol LDL")),
        "{wrapped:?}"
    );
    assert_shape("mcp_get_source_text", &env)?;
    let no_text: Vec<&str> = ALL_SCOPES
        .iter()
        .copied()
        .filter(|s| *s != "read:documents_text")
        .collect();
    let m = c.mcp(&no_text, &["alex"]).await?;
    let env = m
        .ok("get_source", json!({"src_id": src, "include_text": true}))
        .await?;
    assert!(
        env["data"].get("page_text").is_none_or(Value::is_null),
        "{}",
        env["data"]
    );
    // a src of another profile is unknown to a token without it
    let lucia_obs = c
        .get("/api/profiles/lucia/observations")
        .await?
        .expect(200)?
        .json;
    if let Some(ls) = lucia_obs["items"][0]["src"].as_str() {
        let r = full.call("get_source", json!({"src_id": ls})).await?;
        assert_eq!(r.code(), Some("unknown"));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn pseudonymous_and_minimal_modes_hide_names() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, plain) = c
        .token(json!({"scopes": ALL_SCOPES, "identity_mode": "pseudonymous"}))
        .await?;
    let m = Mcp::new(&plain)?;
    for (name, args) in calls().into_iter().take(5) {
        let env = m.ok(name, args).await?;
        let s = env.to_string();
        assert!(
            !s.contains("Alex Rivera") && !s.contains("Rivera"),
            "{name} shows a name"
        );
        assert_eq!(env["identity_mode"], "pseudonymous");
    }
    let rows = m
        .ok(
            "search_observations",
            json!({"profile": "alex", "query": "LDL"}),
        )
        .await?;
    let r0 = &rows["data"]["results"][0];
    assert_eq!(r0["lab"], "lab", "the lab name is redacted");
    assert_eq!(r0["date"], "2026-09-14", "dates stay in pseudonymous mode");

    let (_, plain) = c
        .token(json!({"scopes": ALL_SCOPES, "identity_mode": "minimal"}))
        .await?;
    let m = Mcp::new(&plain)?;
    let rows = m
        .ok(
            "search_observations",
            json!({"profile": "alex", "query": "LDL"}),
        )
        .await?;
    let r0 = &rows["data"]["results"][0];
    assert!(
        r0["date"]
            .as_str()
            .is_some_and(|d| d.ends_with(" days before as_of")),
        "{r0}"
    );
    assert_eq!(
        (r0["name"].clone(), r0["lab"].clone()),
        (Value::Null, json!("lab"))
    );
    assert_eq!(
        (r0["value"].clone(), r0["unit"].clone()),
        (json!(97), json!("mg/dL")),
        "structured values survive"
    );
    assert_shape("mcp_search_minimal", &rows)?;
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn cursors_are_bound_to_the_query_and_the_token() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m1 = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let m2 = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let first = m1
        .ok("get_health_status", json!({"profile": "alex"}))
        .await?;
    let Some(cursor) = first["next_cursor"].as_str() else {
        anyhow::bail!("the demo status of Alex has more than one page: {first}");
    };
    assert!(first["budget"]["truncated"].as_bool().unwrap_or(false));
    let second = m1
        .ok(
            "get_health_status",
            json!({"profile": "alex", "cursor": cursor}),
        )
        .await?;
    assert_eq!(second["data"]["state_hash"], first["data"]["state_hash"]);
    assert_ne!(second["data"]["flags"], first["data"]["flags"]);
    let r = m2
        .call(
            "get_health_status",
            json!({"profile": "alex", "cursor": cursor}),
        )
        .await?;
    assert_eq!(r.code(), Some("bad_input"), "another token's cursor");
    let r = m1
        .call(
            "get_health_status",
            json!({"profile": "alex", "as_of": "2026-01-01", "cursor": cursor}),
        )
        .await?;
    assert_eq!(r.code(), Some("bad_input"), "another query's cursor");
    let (body, mac) = cursor.split_once('.').context("cursor has a mac")?;
    let tail = if body.ends_with("AA") { "BB" } else { "AA" };
    let forged = format!("{}{tail}.{mac}", &body[..body.len() - 2]);
    let r = m1
        .call(
            "get_health_status",
            json!({"profile": "alex", "cursor": forged}),
        )
        .await?;
    assert_eq!(r.code(), Some("bad_input"), "a forged cursor");
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn as_of_answers_what_the_store_knew() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.mcp(&ALL_SCOPES, &["alex"]).await?;
    let args = json!({"profile": "alex", "loinc": "13457-7", "detail": "full"});
    let mut then = args.clone();
    then["as_of"] = json!("2020-01-01");
    let env = m.ok("search_observations", then).await?;
    assert_eq!(env["as_of"], "2020-01-01");
    assert_eq!(env["data"]["results"], json!([]));
    assert_eq!(env["data"]["not_measured"], true);
    let mut later = args.clone();
    later["as_of"] = json!("2099-01-01");
    let env = m.ok("search_observations", later).await?;
    let n = env["data"]["results"].as_array().map(Vec::len).unwrap_or(0);
    let now = m.ok("search_observations", args).await?;
    assert_eq!(
        now["data"]["results"].as_array().map(Vec::len).unwrap_or(0),
        n
    );
    assert!(n > 0);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn category_scopes_split_one_search() -> anyhow::Result<()> {
    let c = Client::new()?;
    let labs = c.mcp(&["read:labs"], &["alex"]).await?;
    let env = labs
        .ok(
            "search_observations",
            json!({"profile": "alex", "query": "LDL"}),
        )
        .await?;
    assert!(
        !env["data"]["results"]
            .as_array()
            .map(Vec::is_empty)
            .unwrap_or(true)
    );
    let wear = c.mcp(&["read:wearables"], &["alex"]).await?;
    let env = wear
        .ok(
            "search_observations",
            json!({"profile": "alex", "query": "LDL"}),
        )
        .await?;
    assert_eq!(
        env["data"]["results"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "{}",
        env["data"]
    );
    let by_scope = ownhealth_contract_tests::strings(&env["withheld"]["by_scope"]);
    assert_eq!(
        by_scope,
        vec!["labs"],
        "withheld.by_scope names the category"
    );
    Ok(())
}
