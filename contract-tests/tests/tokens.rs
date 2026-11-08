//! Personal access tokens over REST (api/tokens.py, api/mcp_usage.py): the plaintext format,
//! what is stored and listed, validation, step-up, revocation, usage. Python: tests/test_tokens.py,
//! tests/mcp/test_mcp_limits.py::test_usage_route.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{ALL_SCOPES, Client, Mcp, uniq};
use serde_json::{Value, json};

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn create_list_and_revoke() -> anyhow::Result<()> {
    let c = Client::new()?;
    let label = format!("Claude Code · {}", uniq());
    let r = c
        .post(
            "/api/tokens",
            &json!({"label": format!("  {label} "), "profiles": ["alex", "lucia", "alex"], "default_profile": "lucia",
                    "scopes": ["read:labs", "read:summary", "read:labs"], "days": 30, "identity_mode": "pseudonymous",
                    "lang": "en", "units": "conventional"}),
        )
        .await?
        .expect(201)?;
    assert_shape("token_created", &r.json)?;
    let plain = r.json["plaintext"].as_str().context("plaintext")?;
    let re = regex::Regex::new(r"^oh_pat_[A-Za-z0-9]{8}_[A-Za-z0-9_-]{43}$")?;
    assert!(re.is_match(plain), "{plain}");
    let t = &r.json["token"];
    assert_eq!(t["prefix"], json!(&plain[..15]));
    assert_eq!(t["label"], json!(label), "the label is trimmed");
    assert_eq!(t["scopes"], json!(["read:labs", "read:summary"]));
    assert_eq!(
        t["profiles"],
        json!(["alex", "lucia"]),
        "profiles keep their order without duplicates"
    );
    assert_eq!(t["default_profile"], "lucia");
    assert_eq!(
        (
            t["identity_mode"].clone(),
            t["lang"].clone(),
            t["units"].clone()
        ),
        (json!("pseudonymous"), json!("en"), json!("conventional"))
    );
    assert_eq!(
        t["limits"],
        json!({"rate_per_min": 60, "egress_facts_per_day": 2000, "egress_bytes_per_day": 2000000,
               "genome_raw_lookups_per_day": 200, "document_pages_per_day": 300, "proposals_per_day": 50})
    );
    assert_eq!(
        (t["last_used_at"].clone(), t["revoked_at"].clone()),
        (Value::Null, Value::Null)
    );
    let id = t["id"].as_str().context("id")?.to_string();

    // defaults: read:summary... from PAT_DEFAULT_SCOPES, 90 days, full identity, the user's language
    let (d, _) = c.token(json!({})).await?;
    assert_eq!(
        (
            d["identity_mode"].clone(),
            d["units"].clone(),
            d["lang"].clone()
        ),
        (json!("full"), json!("si"), json!("es"))
    );
    assert_eq!(d["default_profile"], "alex", "one profile is the default");
    assert_shape("token_default_scopes", &d["scopes"])?;

    let mut list = Value::Null;
    for _ in 0..100 {
        list = c.get("/api/tokens").await?.expect(200)?.json;
        if list.to_string().contains(&id) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let listed = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["id"] == json!(id))
        .context("listed")?;
    assert_eq!(listed, t, "the list shows what the create returned");
    assert!(
        !list.to_string().contains(&plain[16..]),
        "the secret is never listed"
    );

    // the token works on /mcp, then revoking it takes it away at once
    let m = Mcp::new(plain)?;
    assert!(!m.tool_names().await?.is_empty());
    let r = c.delete(&format!("/api/tokens/{id}")).await?.expect(200)?;
    assert_eq!(r.json, json!({"id": id, "revoked": true}));
    // 0.1.1 commits after the response is sent, so give the revoke a moment to land
    let mut status = 0;
    for _ in 0..100 {
        status = m
            .post_raw(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                &[],
            )
            .await?
            .status;
        if status == 401 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(status, 401);
    let r = c.delete(&format!("/api/tokens/{id}")).await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));
    let list = c.get("/api/tokens").await?.expect(200)?.json;
    let listed = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| x["id"] == json!(id))
        .context("still listed")?;
    assert!(listed["revoked_at"].is_string());
    assert!(
        listed["last_used_at"].is_string(),
        "the MCP call stamped last_used_at"
    );

    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "token_create"
        && e["tool"] == "POST /api/tokens"
        && e["resource_ids"] == json!([id])
        && e["args_summary"] == "scopes=read:labs,read:summary;profiles=3;days=30"));
    assert!(
        audit
            .iter()
            .any(|e| e["action"] == "token_revoke" && e["resource_ids"] == json!([id]))
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn bad_token_requests() -> anyhow::Result<()> {
    let c = Client::new()?;
    let bad = |b: Value| {
        json!({"label": "x", "profiles": ["alex"]})
            .as_object()
            .cloned()
            .map(|mut m| {
                if let Some(o) = b.as_object() {
                    m.extend(o.clone());
                }
                Value::Object(m)
            })
    };
    for (body, code) in [
        (json!({"scopes": ["read:everything"]}), Some("bad_input")),
        (json!({"default_profile": "lucia"}), Some("bad_input")),
    ] {
        let r = c
            .post("/api/tokens", &bad(body.clone()).context("body")?)
            .await?;
        assert_eq!((r.status, r.code()), (422, code), "{body}");
    }
    for body in [
        json!({"days": 0}),
        json!({"days": 366}),
        json!({"scopes": []}),
        json!({"profiles": []}),
        json!({"label": ""}),
        json!({"identity_mode": "anonymous"}),
        json!({"units": "imperial"}),
        json!({"lang": "de"}),
    ] {
        let r = c
            .post("/api/tokens", &bad(body.clone()).context("body")?)
            .await?;
        assert_eq!(r.status, 422, "{body}");
        assert!(r.json["detail"].is_array(), "{body}: {}", r.json);
    }
    // a profile the caller cannot reach
    let r = c
        .post(
            "/api/tokens",
            &json!({"label": "x", "profiles": ["nobody-here"]}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("profile_not_granted")));
    assert!(
        r.json["detail"]["allowed"]
            .as_array()
            .is_some_and(|a| a.contains(&json!("alex")))
    );
    // a session without a fresh login cannot make tokens; one with it can
    let m = c.enrolled_member(&format!("Tok {}", uniq())).await?;
    let r = m
        .client
        .post("/api/tokens", &json!({"label": "x", "profiles": ["alex"]}))
        .await?;
    assert_eq!(
        r.status,
        404,
        "the member has no grant on alex: {}",
        String::from_utf8_lossy(&r.body)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn usage_counts_calls_and_is_the_owners_only() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (t, plain) = c.token(json!({"scopes": ALL_SCOPES})).await?;
    let id = t["id"].as_str().context("id")?;
    let u = c
        .get(&format!("/api/tokens/{id}/usage"))
        .await?
        .expect(200)?
        .json;
    assert_eq!(
        u["today"],
        json!({"calls": 0, "facts": 0, "bytes": 0, "lookups": 0, "proposals": 0})
    );
    assert_eq!(u["history"], json!([]));
    let m = Mcp::new(&plain)?;
    m.ok("get_health_status", json!({"profile": "alex"}))
        .await?;
    m.ok(
        "search_observations",
        json!({"profile": "alex", "query": "LDL"}),
    )
    .await?;
    let u = c
        .get(&format!("/api/tokens/{id}/usage"))
        .await?
        .expect(200)?
        .json;
    assert_shape("token_usage", &u)?;
    assert_eq!(u["token_id"], json!(id));
    assert_eq!(u["today"]["calls"], 2);
    assert!(
        u["today"]["facts"].as_i64().unwrap_or(0) > 0
            && u["today"]["bytes"].as_i64().unwrap_or(0) > 0,
        "{u}"
    );
    assert_eq!(
        u["limits"],
        json!({"rate_per_min": 60, "egress_facts_per_day": 2000, "egress_bytes_per_day": 2000000,
               "genome_raw_lookups_per_day": 200, "proposals_per_day": 50})
    );
    assert_eq!(u["history"].as_array().map(Vec::len), Some(1));
    assert_eq!(u["history"][0]["day"], u["day"]);
    // another user's token id is 404, the same as none
    let other = c.enrolled_member(&format!("Usage {}", uniq())).await?;
    let r = other.client.get(&format!("/api/tokens/{id}/usage")).await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));
    let r = c
        .get("/api/tokens/00000000-0000-4000-8000-000000000000/usage")
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn a_member_makes_a_token_after_step_up() -> anyhow::Result<()> {
    let c = Client::new()?;
    let slug = c.new_relative().await?;
    let m = c.enrolled_member(&format!("Stepper {}", uniq())).await?;
    c.put(
        "/api/grants",
        &json!({"user_id": m.user_id, "profile": slug, "role": "caregiver"}),
    )
    .await?
    .expect(200)?;
    // a password login is itself fresh: tokens work right away
    let r = m
        .client
        .post(
            "/api/tokens",
            &json!({"label": "agent", "profiles": [slug]}),
        )
        .await?
        .expect(201)?;
    assert_eq!(r.json["token"]["profiles"], json!([slug]));
    assert_eq!(r.json["token"]["lang"], "en", "the member's locale");
    // the member's tokens are theirs alone
    let mine = m.client.get("/api/tokens").await?.expect(200)?.json;
    assert_eq!(mine["items"].as_array().map(Vec::len), Some(1));
    let alex = c.get("/api/tokens").await?.expect(200)?.json;
    assert!(
        !alex["items"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["id"] == r.json["token"]["id"])
    );
    let id = r.json["token"]["id"].as_str().context("id")?;
    let r = c.delete(&format!("/api/tokens/{id}")).await?;
    assert_eq!(
        (r.status, r.code()),
        (404, Some("unknown")),
        "Alex cannot revoke the member's token"
    );
    Ok(())
}
