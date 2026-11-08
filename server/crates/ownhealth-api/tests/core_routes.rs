//! Lane I: `api/core.py` (tests/test_app.py, tests/security/test_sources_scope.py): who am I, the
//! profile list, and where a value came from.

mod lane_i;

use axum::body::Body;
use http::{Request, StatusCode};
use lane_i::{R, app, lab_document, obs};
use ownhealth_core::acceptance::Check;
use ownhealth_core::observations::{CommitOptions, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::review::{self, CandidateIn};
use serde_json::{Value, json};

fn slugs(v: &Value) -> Vec<String> {
    v.as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p["slug"].as_str().map(String::from))
        .collect()
}

#[tokio::test]
async fn test_me_needs_a_session() -> R {
    let a = app().await?;
    let (s, v) = a.send(Request::get("/me").body(Body::empty())?).await?;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(
        v,
        json!({"detail": {"code": "unauthenticated", "message": "Log in first."}})
    );
    Ok(())
}

#[tokio::test]
async fn test_me_profiles_and_roles() -> R {
    let a = app().await?;
    let (s, me) = a.get("alex", "/me").await?;
    assert_eq!(s, StatusCode::OK, "{me}");
    // the mockup's order: the caller, partner, children, parents
    assert_eq!(slugs(&me["profiles"]), ["alex", "sam", "lucia", "carmen"]);
    let roles: Vec<(String, String)> = me["profiles"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|p| {
            (
                p["slug"].as_str().unwrap_or("").into(),
                p["role"].as_str().unwrap_or("").into(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("alex", "owner"),
            ("sam", "viewer"),
            ("lucia", "guardian"),
            ("carmen", "caregiver")
        ]
        .map(|(a, b)| (a.to_string(), b.to_string()))
    );
    let lucia = &me["profiles"][2];
    assert_eq!(lucia["is_minor"], json!(true));
    assert_eq!(lucia["initials"], "LR");
    assert_eq!(me["profiles"][0]["is_self"], json!(true));
    assert_eq!(me["auth"], json!({"kind": "session"}));
    assert_eq!(me["user"]["id"], json!(a.family.user("alex")));
    assert_eq!(me["household"]["id"], json!(a.family.household_id));
    assert_eq!(me["review_open"], json!(0));
    assert_eq!(me["demo"], json!(false));
    let keys: Vec<&String> = me["profiles"][0]
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    assert_eq!(
        keys,
        [
            "id",
            "slug",
            "display_name",
            "initials",
            "role",
            "relationship",
            "age",
            "sex_at_birth",
            "country",
            "is_minor",
            "is_self",
            "unit_pref",
            "genome_sensitive_ui",
            "scopes"
        ]
    );
    let scopes = me["profiles"][0]["scopes"].as_array().cloned().unwrap_or_default();
    let mut sorted = scopes.clone();
    sorted.sort_by(|x, y| x.as_str().cmp(&y.as_str()));
    assert_eq!(scopes, sorted);

    let (_, sam) = a.get("sam", "/profiles").await?;
    let mut got = slugs(&sam);
    got.sort();
    assert_eq!(got, ["alex", "lucia", "sam"]);
    assert_eq!(slugs(&sam)[0], "sam");
    Ok(())
}

/// The review flow of test_app.py ends in a source lookup: page, status and document come back.
#[tokio::test]
async fn test_review_api_and_source_lookup() -> R {
    let a = app().await?;
    let mut tx = a.rt.pool.begin().await?;
    let (doc_id, boxes) = lab_document(&mut tx, &a, "alex").await?;
    let run = review::create_extraction_run(&mut tx, doc_id, "rules:test", "1", b"x", None, None).await?;
    let res = review::submit_candidate(
        &mut tx,
        run,
        &CandidateIn {
            page_no: 1,
            bbox: boxes[0],
            cells: json!({}),
            row_conf: 0.99,
            observation: obs(a.family.profile("alex"), "97", json!({})),
            checks: vec![Check::fail("loinc", "loinc_unknown")],
        },
        "rules_text",
        "matched",
        0.95,
        false,
    )
    .await?;
    tx.commit().await?;
    let item = res.review_item_id.ok_or_else(|| anyhow::anyhow!("not queued"))?;

    let (_, listed) = a.get("alex", "/review").await?;
    assert!(
        listed["items"]
            .as_array()
            .is_some_and(|i| i.iter().any(|x| x["id"] == json!(item)))
    );
    assert!(listed["open"].as_i64().unwrap_or(0) >= 1);
    let (_, me) = a.get("alex", "/me").await?;
    assert_eq!(me["review_open"], json!(1));
    let (s, out) = a
        .post("alex", &format!("/review/{item}/accept"), Some(json!({"edits": null})))
        .await?;
    assert_eq!(s, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "accepted");
    assert_eq!(out["result_ids"].as_array().map(Vec::len), Some(1));
    let obs_id = out["result_ids"][0].as_str().unwrap_or_default().to_string();
    let src_id: String = sqlx::query_scalar("select src_id from observation where id = $1::uuid")
        .bind(&obs_id)
        .fetch_one(&a.rt.pool)
        .await?;
    let (s, src) = a.get("alex", &format!("/sources/{src_id}")).await?;
    assert_eq!(s, StatusCode::OK, "{src}");
    assert_eq!(src["location"]["page"], json!(1));
    assert_eq!(src["location"]["units"], "pt");
    assert_eq!(src["status"], "current");
    assert_eq!(src["document"]["id"], json!(doc_id));
    assert_eq!(src["text"], Value::Null);
    assert!(src["created_at"].as_str().is_some_and(|t| t.ends_with('Z')), "{src}");
    let keys: Vec<&String> = src.as_object().map(|m| m.keys().collect()).unwrap_or_default();
    assert_eq!(
        keys,
        [
            "src_id",
            "kind",
            "label",
            "status",
            "facts",
            "document",
            "location",
            "import_ref",
            "created_at",
            "text"
        ]
    );
    assert_eq!(src["facts"][0]["table"], "observation");
    assert_eq!(src["facts"][0]["id"], json!(obs_id));

    // Sam is a viewer of Alex (read:summary only): a lab source needs read:labs, and he cannot review.
    let (s, denied) = a.get("sam", &format!("/sources/{src_id}")).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{denied}");
    assert!(!denied.to_string().contains("document"));
    let (s, _) = a
        .post("alex", &format!("/review/{item}/accept"), Some(json!({})))
        .await?;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, v) = a.get("sam", "/sources/src_doesnotexist").await?;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v, json!({"detail": {"code": "unknown", "message": "Unknown source."}}));
    let actions: Vec<String> = sqlx::query_scalar("select action from audit_event where actor_id = $1")
        .bind(a.family.user("alex"))
        .fetch_all(&a.rt.pool)
        .await?;
    assert!(actions.iter().any(|x| x == "review_accept") && actions.iter().any(|x| x == "read"));
    let tools: Vec<Option<String>> =
        sqlx::query_scalar("select tool from audit_event where resource_type = 'src' and actor_id = $1")
            .bind(a.family.user("alex"))
            .fetch_all(&a.rt.pool)
            .await?;
    assert_eq!(tools, [Some("GET /api/sources".to_string())]);
    Ok(())
}

async fn source(a: &lane_i::App, who: &str, extra: Value) -> anyhow::Result<String> {
    let pid = a.family.profile(who);
    let mut src = SrcIn::new("manual");
    src.profile_id = Some(pid);
    src.label = Some("LabNorte".into());
    src.actor_kind = "system".into();
    let mut c = a.conn().await?;
    let done = commit_observation(
        &mut c,
        &obs(pid, "97", extra),
        &SrcRef::New(src),
        CommitOptions::new("import"),
    )
    .await?;
    Ok(done.src_id)
}

#[tokio::test]
async fn test_viewer_cannot_resolve_lab_or_sensitive_sources() -> R {
    let a = app().await?;
    let phq9 = json!({"value_num": "12", "printed_value": "12", "value_canonical": "12", "alias_group": "phq9",
                      "loinc": "44261-6", "category": "survey", "sensitivity": ["mental_health"]});
    let lab = source(&a, "alex", json!({})).await?;
    let sensitive = source(&a, "alex", phq9.clone()).await?;
    // sam is a viewer on alex: read:summary only
    let (s, v) = a.get("sam", &format!("/sources/{lab}")).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::FORBIDDEN, json!("insufficient_scope")),
        "{v}"
    );
    assert_eq!(v["detail"]["message"], "This needs read:labs.");
    assert_eq!(v["detail"]["allowed"], json!([]));
    assert!(!v.to_string().contains("document"));
    let (s, v) = a.get("sam", &format!("/sources/{sensitive}")).await?;
    assert!(matches!(s, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "{v}");
    assert!(!v.to_string().contains("facts"));
    // a caregiver has read:vitals on carmen but not read:sensitive: the sensitive source is unknown
    let carmen = source(&a, "carmen", phq9).await?;
    let (s, v) = a.get("alex", &format!("/sources/{carmen}")).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("unknown")),
        "{v}"
    );
    assert_eq!(a.get("alex", &format!("/sources/{lab}")).await?.0, StatusCode::OK);
    assert_eq!(a.get("alex", &format!("/sources/{sensitive}")).await?.0, StatusCode::OK);
    // no grant at all (sam on carmen) is unknown too, never profile_not_granted
    let (s, v) = a.get("sam", &format!("/sources/{carmen}")).await?;
    assert_eq!(
        (s, v["detail"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("unknown"))
    );
    Ok(())
}

#[tokio::test]
async fn test_source_page_text_needs_documents_text_and_a_valid_flag() -> R {
    let a = app().await?;
    let mut tx = a.rt.pool.begin().await?;
    let (doc_id, _) = lab_document(&mut tx, &a, "alex").await?;
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, text_content) \
         values ($1, 1, 595, 842, true, 'Colesterol LDL 97 mg/dL')",
    )
    .bind(doc_id)
    .execute(&mut *tx)
    .await?;
    let pid = a.family.profile("alex");
    let mut src = SrcIn::new("document_region");
    src.profile_id = Some(pid);
    src.document_id = Some(doc_id);
    src.page_no = Some(1);
    src.bbox = Some([10.0, 20.0, 30.0, 40.0]);
    src.actor_kind = "system".into();
    let done = commit_observation(
        &mut tx,
        &obs(pid, "97", json!({})),
        &SrcRef::New(src),
        CommitOptions::new("import"),
    )
    .await?;
    tx.commit().await?;
    let (s, v) = a
        .get("alex", &format!("/sources/{}?include_text=true", done.src_id))
        .await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["text"],
        json!({"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "Colesterol LDL 97 mg/dL"})
    );
    assert_eq!(v["location"]["bbox"], json!([10.0, 20.0, 30.0, 40.0]));
    let (_, plain) = a.get("alex", &format!("/sources/{}", done.src_id)).await?;
    assert_eq!(plain["text"], Value::Null);
    let (s, bad) = a
        .get("alex", &format!("/sources/{}?include_text=maybe", done.src_id))
        .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad["detail"][0]["type"], "bool_parsing");
    assert_eq!(bad["detail"][0]["loc"], json!(["query", "include_text"]));
    Ok(())
}
