//! The household: relatives, slugs, grants, locks, sensitive switches, deletion and their audit
//! rows (api/family.py). Python: tests/security/test_family.py, test_export_delete.py.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{Client, strings, uniq};
use serde_json::{Value, json};

fn profile<'a>(list: &'a Value, slug: &str) -> Option<&'a Value> {
    list.as_array()
        .into_iter()
        .flatten()
        .find(|p| p["slug"] == slug)
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn relatives_get_slugs_and_a_guardian_grant() -> anyhow::Result<()> {
    let c = Client::new()?;
    let u = uniq();
    let r = c
        .post(
            "/api/household/profiles",
            &json!({"display_name": format!("Ángela{u} Núñez"), "role": "guardian", "relationship": "parent",
                    "birth_date": "1950-01-31", "sex_at_birth": "female", "country": "ES"}),
        )
        .await?
        .expect(201)?;
    let slug = r.json["slug"].as_str().context("slug")?.to_string();
    assert_eq!(
        slug,
        format!("angela{u}"),
        "NFKD, ASCII, first word, lower case"
    );
    assert!(r.json["id"].is_string());
    assert_eq!(r.json.as_object().map(|o| o.len()), Some(2));
    // the same first word again gets -2
    let again = c
        .post(
            "/api/household/profiles",
            &json!({"display_name": format!("Angela{u} Other"), "role": "guardian"}),
        )
        .await?
        .expect(201)?;
    assert_eq!(again.json["slug"], format!("angela{u}-2"));
    // "me" is never a slug
    let me = c
        .post(
            "/api/household/profiles",
            &json!({"display_name": "Me", "role": "guardian", "slug": "me"}),
        )
        .await?
        .expect(201)?;
    assert!(
        me.json["slug"]
            .as_str()
            .is_some_and(|s| s.starts_with("person")),
        "{}",
        me.json
    );

    // a caregiver cannot be the creator's role; a blank name is refused
    let r = c
        .post(
            "/api/household/profiles",
            &json!({"display_name": "Someone", "role": "caregiver"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (400, Some("bad_input")));
    let r = c
        .post(
            "/api/household/profiles",
            &json!({"display_name": "   ", "role": "guardian"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (400, Some("bad_input")));
    let r = c
        .post(
            "/api/household/profiles",
            &json!({"display_name": "X", "role": "owner"}),
        )
        .await?;
    assert_eq!(r.status, 422);

    let profiles = c.get("/api/profiles").await?.expect(200)?.json;
    let p = profile(&profiles, &slug).context("new profile not listed")?;
    assert_eq!(p["role"], "guardian");
    assert_eq!(p["relationship"], "parent");
    assert_eq!(p["is_self"], false);
    assert_eq!(p["is_minor"], false);
    assert_eq!(p["initials"], "ÁN");
    assert_eq!(p["sex_at_birth"], "female");
    assert_eq!(
        strings(&p["scopes"]).len(),
        10,
        "a guardian of an unclaimed profile has every scope"
    );
    // the caller first, then partner, children, parents, others
    let order: Vec<&str> = profiles
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["relationship"].as_str())
        .collect();
    let rank = |r: &str| match r {
        "self" => 0,
        "partner" => 1,
        "child" => 2,
        "parent" => 3,
        _ => 4,
    };
    assert!(
        order.windows(2).all(|w| rank(w[0]) <= rank(w[1])),
        "{order:?}"
    );

    let hh = c.get("/api/household").await?.expect(200)?.json;
    let hp = profile(&hh["profiles"], &slug).context("household profile")?;
    assert_eq!(
        (hp["can_manage"].clone(), hp["claimed"].clone()),
        (json!(true), json!(false))
    );
    assert_eq!(hp["locks"], json!([]));
    assert_eq!(hp["genome_sensitive_agents"], false);
    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "grant_change"
        && e["tool"] == "POST /api/household/profiles"
        && e["profile"] == json!(slug)
        && e["args_summary"] == "create;role=guardian"));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn grants_give_a_member_a_profile_with_the_categories_asked() -> anyhow::Result<()> {
    let c = Client::new()?;
    let slug = c.new_relative().await?;
    let m = c.enrolled_member(&format!("Granted {}", uniq())).await?;
    assert_eq!(
        m.client.get("/api/profiles").await?.expect(200)?.json,
        json!([])
    );
    // a profile the member cannot see is 404 profile_not_granted, with what they can use
    let r = m
        .client
        .get(&format!("/api/profiles/{slug}/status"))
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("profile_not_granted")));
    assert_eq!(r.json["detail"]["allowed"], json!([]));

    let r = c.put("/api/grants", &json!({"user_id": m.user_id, "profile": slug, "role": "viewer", "categories": ["read:labs", "nope"]})).await?;
    assert_eq!((r.status, r.code()), (422, Some("bad_input")));
    let r = c
        .put("/api/grants", &json!({"user_id": m.user_id, "profile": slug, "role": "viewer", "categories": ["read:summary", "read:labs"]}))
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"ok": true}));
    let ps = m.client.get("/api/profiles").await?.expect(200)?.json;
    let p = profile(&ps, &slug).context("granted profile")?;
    assert_eq!(p["role"], "viewer");
    assert_eq!(strings(&p["scopes"]), vec!["read:labs", "read:summary"]);
    // a viewer may read labs but not records, and cannot write
    m.client
        .get(&format!("/api/profiles/{slug}/observations"))
        .await?
        .expect(200)?;
    let r = m
        .client
        .get(&format!("/api/profiles/{slug}/records/medication"))
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("insufficient_scope")));
    let r = m
        .client
        .post(
            &format!("/api/profiles/{slug}/records/condition"),
            &json!({"code_text": "Asthma", "clinical_status": "active"}),
        )
        .await?;
    assert_eq!(r.status, 403, "{}", String::from_utf8_lossy(&r.body));
    // a viewer cannot manage the profile
    let r = m
        .client
        .put(
            &format!("/api/profiles/{slug}/locks"),
            &json!({"locks": []}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("forbidden")));

    // grants need step-up for a session; the member's own grant row shows in their household view
    let r = m
        .client
        .put(
            "/api/grants",
            &json!({"user_id": m.user_id, "profile": slug, "role": "guardian"}),
        )
        .await?;
    assert!(r.status == 403, "{}", String::from_utf8_lossy(&r.body));
    let hh = m.client.get("/api/household").await?.expect(200)?.json;
    assert!(
        hh["grants"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|g| g["profile"] == json!(slug) && g["role"] == "viewer")
    );

    // the owner's own grant cannot change; an unknown member is 404
    let me = c.get("/api/me").await?.expect(200)?.json;
    let alex = me["user"]["id"].as_str().context("alex id")?;
    let r = c
        .put(
            "/api/grants",
            &json!({"user_id": alex, "profile": "alex", "role": "viewer"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("owner_grant")));
    let r = c.put("/api/grants", &json!({"user_id": "00000000-0000-4000-8000-000000000000", "profile": slug, "role": "viewer"})).await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));

    // revoke, then revoking again is 404
    let r = c
        .delete(&format!("/api/grants?user_id={}&profile={slug}", m.user_id))
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"ok": true}));
    let r = c
        .delete(&format!("/api/grants?user_id={}&profile={slug}", m.user_id))
        .await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));
    assert_eq!(m.client.get("/api/profiles").await?.json, json!([]));
    let audit = c.audit(500).await?;
    let mine: Vec<&Value> = audit
        .iter()
        .filter(|e| e["profile"] == json!(slug) && e["action"] == "grant_change")
        .collect();
    let summaries: Vec<&str> = mine
        .iter()
        .filter_map(|e| e["args_summary"].as_str())
        .collect();
    assert!(
        summaries.contains(&"role=viewer;categories=read:labs,read:summary"),
        "{summaries:?}"
    );
    assert!(summaries.contains(&"revoke"), "{summaries:?}");
    assert!(mine.iter().any(|e| e["tool"] == "PUT /api/grants"
        && e["resource_type"] == "user"
        && e["resource_ids"] == json!([m.user_id])));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn locks_and_sensitive_switches() -> anyhow::Result<()> {
    let c = Client::new()?;
    let slug = c.new_relative().await?;
    let r = c
        .put(
            &format!("/api/profiles/{slug}/locks"),
            &json!({"locks": ["read:wearables", "read:genome"]}),
        )
        .await?
        .expect(200)?;
    assert_eq!(
        r.json,
        json!({"ok": true, "locks": ["read:genome", "read:wearables"]})
    );
    let r = c
        .put(
            &format!("/api/profiles/{slug}/locks"),
            &json!({"locks": ["read:everything"]}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (422, Some("bad_input")));
    let hh = c.get("/api/household").await?.expect(200)?.json;
    assert_eq!(
        profile(&hh["profiles"], &slug).context("p")?["locks"],
        json!(["read:genome", "read:wearables"])
    );
    // a guardian (not the owner) loses locked scopes
    let ps = c.get("/api/profiles").await?.expect(200)?.json;
    let scopes = strings(&profile(&ps, &slug).context("p")?["scopes"]);
    assert!(
        !scopes.contains(&"read:wearables".to_string())
            && !scopes.contains(&"read:genome".to_string()),
        "{scopes:?}"
    );
    c.put(
        &format!("/api/profiles/{slug}/locks"),
        &json!({"locks": []}),
    )
    .await?
    .expect(200)?;

    let r = c
        .put(
            &format!("/api/profiles/{slug}/sensitive"),
            &json!({"ui": true}),
        )
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"ok": true, "ui": true, "agents": false}));
    let r = c
        .put(
            &format!("/api/profiles/{slug}/sensitive"),
            &json!({"agents": true, "ui": false}),
        )
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"ok": true, "ui": false, "agents": true}));
    // a minor's sensitive findings stay withheld
    let r = c
        .put("/api/profiles/lucia/sensitive", &json!({"ui": true}))
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("minor")));
    // turning a switch off needs no step-up and is allowed for a minor
    let r = c
        .put("/api/profiles/lucia/sensitive", &json!({"ui": false}))
        .await?
        .expect(200)?;
    assert_eq!(r.json["ui"], false);
    // a viewer cannot manage Sam's profile
    let r = c
        .put("/api/profiles/sam/locks", &json!({"locks": []}))
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("forbidden")));
    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "lock_change"
        && e["profile"] == json!(slug)
        && e["args_summary"] == "read:genome,read:wearables"));
    assert!(audit.iter().any(|e| e["action"] == "consent_change"
        && e["profile"] == json!(slug)
        && e["args_summary"] == "ui=False;agents=True"));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn deleting_a_profile_needs_its_slug_and_removes_it() -> anyhow::Result<()> {
    let c = Client::new()?;
    let slug = c.new_relative().await?;
    c.post(
        &format!("/api/profiles/{slug}/manual-observations"),
        &json!({"name": "Peso", "value": "70.5", "unit": "kg", "effective_at": "2026-10-01T08:00:00Z", "category": "vital-signs"}),
    )
    .await?
    .expect(201)?;
    let r = c
        .delete(&format!("/api/profiles/{slug}?confirm=wrong"))
        .await?;
    assert_eq!((r.status, r.code()), (422, Some("confirm_mismatch")));
    let r = c
        .delete(&format!("/api/profiles/{slug}?confirm={slug}"))
        .await?
        .expect(200)?;
    assert_shape("profile_deleted", &r.json)?;
    assert_eq!(r.json["deleted"], json!(slug));
    assert!(
        r.json["rows"]
            .as_object()
            .is_some_and(|o| o.values().any(|v| v.as_i64().unwrap_or(0) > 0)),
        "{}",
        r.json
    );
    let r = c.get(&format!("/api/profiles/{slug}/status")).await?;
    assert_eq!((r.status, r.code()), (404, Some("profile_not_granted")));
    // Alex cannot delete Sam's profile (viewer) and the demo owner's own needs the slug
    let r = c.delete("/api/profiles/sam?confirm=sam").await?;
    assert_eq!((r.status, r.code()), (403, Some("forbidden")));
    let audit = c.audit(500).await?;
    assert!(audit.iter().any(|e| e["action"] == "purge"
        && e["tool"] == "DELETE /api/profiles"
        && e["resource_type"] == "profile"));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn a_member_closes_their_account() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.enrolled_member(&format!("Leaving {}", uniq())).await?;
    let r = m
        .client
        .delete("/api/account?confirm=someone%20else")
        .await?;
    assert_eq!((r.status, r.code()), (422, Some("confirm_mismatch")));
    let r = m
        .client
        .delete(&format!(
            "/api/account?confirm={}",
            m.name.replace(' ', "%20")
        ))
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"ok": true, "profile_deleted": null}));
    assert!(r.set_cookie("oh_session").is_some());
    // the login is gone: the password no longer works
    let r = c
        .post(
            "/api/auth/password/login",
            &json!({"name": m.name, "password": m.password}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (401, Some("login_failed")));
    let hh = c.get("/api/household").await?.expect(200)?.json;
    assert!(
        !hh["members"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|x| x["user_id"] == json!(m.user_id))
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn invites_that_claim_a_profile() -> anyhow::Result<()> {
    let c = Client::new()?;
    let slug = c.new_relative().await?;
    let name = format!("Claimer {}", uniq());
    let inv = c
        .post(
            "/api/household/invites",
            &json!({"display_name": name, "claim_profile": slug}),
        )
        .await?
        .expect(201)?;
    let code = inv.json["code"].as_str().context("code")?;
    let info = c
        .get(&format!("/api/auth/invite/{code}"))
        .await?
        .expect(200)?;
    assert_eq!(info.json["claims_profile"], true);
    let password = format!("claim password {}", uniq());
    let r = c
        .post(
            "/api/auth/enroll/password",
            &json!({"code": code, "password": password}),
        )
        .await?
        .expect(200)?;
    let s = c.with_session(&r.cookie_value("oh_session").context("cookie")?);
    let ps = s.get("/api/profiles").await?.expect(200)?.json;
    let p = profile(&ps, &slug).context("claimed profile")?;
    assert_eq!(
        (p["role"].clone(), p["is_self"].clone()),
        (json!("owner"), json!(true))
    );
    // Alex keeps a guardian grant but no longer manages a claimed profile
    let hh = c.get("/api/household").await?.expect(200)?.json;
    let hp = profile(&hh["profiles"], &slug).context("p")?;
    assert_eq!(
        (hp["claimed"].clone(), hp["can_manage"].clone()),
        (json!(true), json!(false))
    );
    // a claimed profile cannot be claimed again
    let r = c
        .post(
            "/api/household/invites",
            &json!({"display_name": "Second", "claim_profile": slug}),
        )
        .await?;
    assert!(
        r.status == 403 || r.status == 409,
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    Ok(())
}
