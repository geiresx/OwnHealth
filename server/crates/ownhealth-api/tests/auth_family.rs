//! Household roles, relatives without a login, invites and claiming, locks, switches, the audit
//! viewer (`tests/security/test_family.py`).

#[path = "auth_support.rs"]
mod support;

use std::collections::BTreeSet;

use axum::http::StatusCode;
use ownhealth_auth::household;
use ownhealth_auth::testing::SoftKey;
use ownhealth_core::access::{Principal, accessible_profiles, effective_permission};
use ownhealth_core::audit::{AuditEvent, write_audit};
use serde_json::{Value, json};
use support::Env;
use uuid::Uuid;

type R = anyhow::Result<()>;

async fn perm(env: &Env, user: Uuid, profile: Uuid) -> anyhow::Result<ownhealth_core::access::Permission> {
    let mut c = env.pool().acquire().await?;
    let hh: Uuid = sqlx::query_scalar("select household_id from app_user where id = $1")
        .bind(user)
        .fetch_one(&mut *c)
        .await?;
    Ok(effective_permission(&mut c, &Principal::session(user, hh), profile).await?)
}

#[tokio::test]
async fn household_view_is_scoped() -> R {
    let env = Env::new(true).await?;
    let alex = env.as_user(env.user("alex")).await?.get("/api/household").await?.json;
    let names: BTreeSet<&str> = alex["members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["display_name"].as_str())
        .collect();
    assert_eq!(names, BTreeSet::from(["Alex Rivera", "Sam Rivera"]));
    let by_slug = |slug: &str| -> Value {
        alex["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["slug"] == slug)
            .cloned()
            .unwrap_or(Value::Null)
    };
    // guardian of an unclaimed profile manages it; a caregiver and a viewer do not
    assert_eq!(by_slug("alex")["can_manage"], true);
    assert_eq!(by_slug("lucia")["can_manage"], true);
    assert_eq!(by_slug("carmen")["can_manage"], false);
    assert_eq!(by_slug("sam")["can_manage"], false);
    // the caller first, then partner, children, parents
    assert_eq!(alex["profiles"][0]["slug"], "alex");
    assert_eq!(alex["profiles"][0]["is_self"], true);
    // Alex sees all grants on profiles he manages, plus his own; not Sam's grant on sam
    let seen: BTreeSet<(String, String, String)> = alex["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|g| {
            (
                g["user_id"].as_str().unwrap_or_default().to_string(),
                g["profile"].as_str().unwrap_or_default().to_string(),
                g["role"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let (a, s) = (env.user("alex").to_string(), env.user("sam").to_string());
    assert!(seen.contains(&(s.clone(), "alex".into(), "viewer".into())));
    assert!(seen.contains(&(a, "sam".into(), "viewer".into())));
    assert!(!seen.contains(&(s, "sam".into(), "owner".into())));
    Ok(())
}

#[tokio::test]
async fn relative_guardian_manages_until_claimed() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post(
            "/api/household/profiles",
            &json!({"display_name": "Mateo Rivera", "role": "guardian", "relationship": "child",
                    "birth_date": "2022-03-01"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    assert_eq!(r.json["slug"], "mateo");
    let mateo: Uuid = serde_json::from_value(r.json["id"].clone())?;
    // the guardian grants Sam guardian too, with step-up
    let body = json!({"user_id": env.user("sam"), "profile": "mateo", "role": "guardian"});
    let r = env.stale(env.user("alex")).await?.put("/api/grants", &body).await?;
    assert_eq!(r.code(), "step_up_required");
    assert_eq!(alex.put("/api/grants", &body).await?.status, StatusCode::OK);
    assert_eq!(perm(&env, env.user("sam"), mateo).await?.role, "guardian");
    // a minor's sensitive genome switch stays off
    let r = alex.put("/api/profiles/mateo/sensitive", &json!({"ui": true})).await?;
    assert_eq!((r.status, r.code()), (StatusCode::CONFLICT, "minor"));
    // the caregiver of Carmen cannot manage her grants or locks, nor export
    let mut carmen = body.clone();
    carmen["profile"] = json!("carmen");
    carmen["role"] = json!("viewer");
    assert_eq!(alex.put("/api/grants", &carmen).await?.status, StatusCode::FORBIDDEN);
    let r = alex
        .put("/api/profiles/carmen/locks", &json!({"locks": ["read:labs"]}))
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = alex.post("/api/profiles/carmen/export", &json!({"fmt": "zip"})).await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // nobody becomes owner through a grant, and the owner's grant does not change
    let mut owner = body.clone();
    owner["role"] = json!("owner");
    let r = alex.put("/api/grants", &owner).await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        r.json["detail"][0]["msg"],
        "Input should be 'guardian', 'caregiver' or 'viewer'"
    );
    let own = json!({"user_id": env.user("alex"), "profile": "alex", "role": "viewer"});
    let r = alex.put("/api/grants", &own).await?;
    assert_eq!((r.status, r.code()), (StatusCode::CONFLICT, "owner_grant"));
    let r = alex
        .put(
            "/api/grants",
            &json!({"user_id": env.user("sam"), "profile": "mateo", "role": "viewer",
                                    "categories": ["read:nope"]}),
        )
        .await?;
    assert_eq!(
        (r.status, r.json["detail"]["message"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Unknown categories ['read:nope']")
        )
    );
    Ok(())
}

#[tokio::test]
async fn invite_claims_profile() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    // Alex is Carmen's caregiver: he may invite her to claim her own profile, nothing more
    let inv = alex
        .post(
            "/api/household/invites",
            &json!({"display_name": "Carmen Ortega", "claim_profile": "carmen"}),
        )
        .await?;
    assert_eq!(inv.status, StatusCode::CREATED, "{:?}", inv.json);
    let code = inv.json["code"].as_str().unwrap_or_default().to_string();
    assert!(
        inv.json["url"]
            .as_str()
            .unwrap_or_default()
            .ends_with(&format!("/enroll?code={code}"))
    );
    assert_eq!(inv.json["expires_in_days"], 7);
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam
        .post(
            "/api/household/invites",
            &json!({"display_name": "x", "claim_profile": "carmen"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let mut carmen = env.anon();
    let info = carmen.get(&format!("/api/auth/invite/{code}")).await?.json;
    assert_eq!(
        (&info["display_name"], &info["claims_profile"]),
        (&json!("Carmen Ortega"), &json!(true))
    );
    let mut key = SoftKey::default();
    let opts = carmen
        .post("/api/auth/enroll/begin", &json!({"code": code}))
        .await?
        .json;
    let done = carmen
        .post(
            "/api/auth/enroll/finish",
            &json!({"code": code, "credential": key.create(&opts, None)}),
        )
        .await?;
    assert_eq!(done.status, StatusCode::OK, "{:?}", done.json);
    assert_eq!(done.json["recovery_codes"].as_array().map(Vec::len), Some(10));
    let household = carmen.get("/api/household").await?.json;
    let mine: Vec<(Value, Value, Value)> = household["profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| (p["slug"].clone(), p["role"].clone(), p["is_self"].clone()))
        .collect();
    assert_eq!(mine, vec![(json!("carmen"), json!("owner"), json!(true))]);
    // the invite is used up
    let r = env.anon().get(&format!("/api/auth/invite/{code}")).await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // Alex keeps his caregiver grant; now Carmen manages it
    assert_eq!(
        perm(&env, env.user("alex"), env.profile("carmen")).await?.role,
        "caregiver"
    );
    let body = json!({"user_id": env.user("alex"), "profile": "carmen", "role": "viewer"});
    assert_eq!(carmen.put("/api/grants", &body).await?.status, StatusCode::OK);
    assert_eq!(
        perm(&env, env.user("alex"), env.profile("carmen")).await?.role,
        "viewer"
    );
    let r = carmen
        .delete(&format!("/api/grants?user_id={}&profile=carmen", env.user("alex")))
        .await?;
    assert_eq!(r.status, StatusCode::OK);
    let mut c = env.pool().acquire().await?;
    let p = Principal::session(env.user("alex"), env.fam().household_id);
    let slugs: BTreeSet<String> = accessible_profiles(&mut c, &p)
        .await?
        .into_iter()
        .map(|x| x.profile.slug)
        .collect();
    assert!(!slugs.contains("carmen"));
    Ok(())
}

#[tokio::test]
async fn locks_and_switches() -> R {
    let env = Env::new(true).await?;
    let (alex_id, sam_id, alex_p) = (env.user("alex"), env.user("sam"), env.profile("alex"));
    let mut alex = env.as_user(alex_id).await?;
    let mut stale = env.stale(alex_id).await?;
    // adding a lock narrows access: no step-up
    assert!(perm(&env, sam_id, alex_p).await?.scopes.contains("read:summary"));
    let r = stale
        .put("/api/profiles/alex/locks", &json!({"locks": ["read:summary"]}))
        .await?;
    assert_eq!(r.json["locks"], json!(["read:summary"]));
    assert!(!perm(&env, sam_id, alex_p).await?.scopes.contains("read:summary"));
    assert!(
        perm(&env, alex_id, alex_p).await?.scopes.contains("read:summary"),
        "not the owner"
    );
    // removing it widens access: step-up
    let r = stale.put("/api/profiles/alex/locks", &json!({"locks": []})).await?;
    assert_eq!(r.code(), "step_up_required");
    assert_eq!(
        alex.put("/api/profiles/alex/locks", &json!({"locks": []}))
            .await?
            .status,
        StatusCode::OK
    );
    let r = alex
        .put("/api/profiles/alex/locks", &json!({"locks": ["read:nope"]}))
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    // switches: on needs step-up, off does not
    let r = stale
        .put("/api/profiles/alex/sensitive", &json!({"agents": true}))
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = alex
        .put("/api/profiles/alex/sensitive", &json!({"ui": true, "agents": true}))
        .await?;
    assert_eq!(r.json, json!({"ok": true, "ui": true, "agents": true}));
    let r = stale
        .put("/api/profiles/alex/sensitive", &json!({"agents": false}))
        .await?;
    assert_eq!(r.json["agents"], false);
    let args: Vec<String> =
        sqlx::query_scalar("select args_summary from audit_event where action = 'consent_change' order by id")
            .fetch_all(env.pool())
            .await?;
    assert_eq!(args, ["ui=True;agents=True", "ui=None;agents=False"]);
    Ok(())
}

#[tokio::test]
async fn audit_viewer_scope() -> R {
    let env = Env::new(true).await?;
    let mut c = env.pool().acquire().await?;
    for (actor, profile) in [("sam", "alex"), ("alex", "carmen"), ("alex", "sam")] {
        write_audit(
            &mut c,
            &AuditEvent {
                actor_id: Some(env.user(actor)),
                tool: Some("GET /x".into()),
                profile_id: Some(env.profile(profile)),
                ..AuditEvent::new("user", "ui", "read")
            },
        )
        .await?;
    }
    drop(c);
    let pairs = |v: &Value| -> BTreeSet<(String, String)> {
        v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| {
                (
                    e["actor"].as_str().unwrap_or_default().to_string(),
                    e["profile"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    let alex = env.as_user(env.user("alex")).await?.get("/api/audit").await?.json;
    let mine = pairs(&alex);
    assert!(
        mine.contains(&("Sam Rivera".into(), "alex".into())),
        "who read my profile"
    );
    assert!(
        mine.contains(&("Alex Rivera".into(), "carmen".into())) && mine.contains(&("Alex Rivera".into(), "sam".into()))
    );
    let sam = env.as_user(env.user("sam")).await?.get("/api/audit").await?.json;
    assert!(pairs(&sam).contains(&("Alex Rivera".into(), "sam".into())));
    // Sam is only a viewer of Alex: he does not see who else read Alex, nor Alex's caregiving reads
    assert!(!pairs(&sam).contains(&("Alex Rivera".into(), "carmen".into())));
    let filtered = env
        .as_user(env.user("alex"))
        .await?
        .get("/api/audit?profile=alex")
        .await?
        .json;
    let profiles: BTreeSet<String> = pairs(&filtered).into_iter().map(|(_, p)| p).collect();
    assert_eq!(profiles, BTreeSet::from(["alex".to_string()]));
    let r = env
        .as_user(env.user("sam"))
        .await?
        .get("/api/audit?profile=carmen")
        .await?;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // paging
    let mut a = env.as_user(env.user("alex")).await?;
    let page = a.get("/api/audit?limit=1").await?.json;
    let next = page["next_before"].as_i64().unwrap_or_default();
    assert!(next > 0 && page["items"].as_array().map(Vec::len) == Some(1));
    let rest = a.get(&format!("/api/audit?before={next}")).await?.json;
    assert!(
        rest["items"]
            .as_array()
            .is_some_and(|i| i.iter().all(|e| e["id"].as_i64() < Some(next)))
    );
    assert_eq!(
        a.get("/api/audit?limit=0").await?.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        a.get("/api/audit?limit=x").await?.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    Ok(())
}

#[tokio::test]
async fn guardian_stops_managing_once_claimed() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let kid = alex
        .post(
            "/api/household/profiles",
            &json!({"display_name": "Mateo Rivera", "role": "guardian"}),
        )
        .await?
        .json;
    let (kid_id, slug): (Uuid, String) = (
        serde_json::from_value(kid["id"].clone())?,
        serde_json::from_value(kid["slug"].clone())?,
    );
    let path = format!("/api/profiles/{slug}/locks");
    assert_eq!(alex.put(&path, &json!({"locks": []})).await?.status, StatusCode::OK);
    let mut tx = env.pool().begin().await?;
    let (_, code) = household::invite_member(
        &mut tx,
        env.fam().household_id,
        env.user("alex"),
        "Mateo Rivera",
        "es",
        Some(kid_id),
    )
    .await?;
    household::redeem_invite(&mut tx, &code).await?;
    tx.commit().await?;
    assert_eq!(
        perm(&env, env.user("alex"), kid_id).await?.role,
        "guardian",
        "the grant stays"
    );
    assert_eq!(
        alex.put(&path, &json!({"locks": []})).await?.status,
        StatusCode::FORBIDDEN
    );
    let r = alex
        .post(&format!("/api/profiles/{slug}/export"), &json!({"fmt": "zip"}))
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_relatives_profile_always_has_a_manager() -> R {
    let env = Env::new(true).await?;
    let r = env
        .as_user(env.user("alex"))
        .await?
        .post(
            "/api/household/profiles",
            &json!({"display_name": "Pilar Ortega", "role": "caregiver"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, "bad_input"));
    Ok(())
}

#[tokio::test]
async fn a_caregiver_cannot_take_over_a_managed_profile_with_a_claim_invite() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let mut sam = env.as_user(env.user("sam")).await?;
    // Alex is Lucía's guardian and makes Sam her caregiver: Sam may not send the claim invite
    let grant = json!({"user_id": env.user("sam"), "profile": "lucia", "role": "caregiver"});
    assert_eq!(alex.put("/api/grants", &grant).await?.status, StatusCode::OK);
    let r = sam
        .post(
            "/api/household/invites",
            &json!({"display_name": "Lucía Rivera", "claim_profile": "lucia"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::FORBIDDEN, "forbidden"));
    // the guardian may
    let r = alex
        .post(
            "/api/household/invites",
            &json!({"display_name": "Lucía Rivera", "claim_profile": "lucia"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED);
    // a caregiver's invite for a profile nobody manages stops working once someone does
    let inv = alex
        .post(
            "/api/household/invites",
            &json!({"display_name": "Carmen Ortega", "claim_profile": "carmen"}),
        )
        .await?;
    assert_eq!(inv.status, StatusCode::CREATED);
    sqlx::query(
        "insert into profile_grant (user_id, profile_id, role, categories) \
         select $1, $2, 'guardian', categories from profile_grant where user_id = $3 and profile_id = $2",
    )
    .bind(env.user("sam"))
    .bind(env.profile("carmen"))
    .bind(env.user("alex"))
    .execute(env.pool())
    .await?;
    let code = inv.json["code"].clone();
    let mut carmen = env.anon();
    let mut key = SoftKey::default();
    let opts = carmen
        .post("/api/auth/enroll/begin", &json!({"code": code}))
        .await?
        .json;
    let r = carmen
        .post(
            "/api/auth/enroll/finish",
            &json!({"code": code, "credential": key.create(&opts, None)}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, "invite_invalid"));
    assert_eq!(
        perm(&env, env.user("sam"), env.profile("carmen")).await?.role,
        "guardian"
    );
    Ok(())
}

#[tokio::test]
async fn only_an_owner_invites_a_member() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post(
            "/api/household/invites",
            &json!({"display_name": "Pilar Ortega", "locale": "en"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = alex
        .post(
            "/api/household/invites",
            &json!({"display_name": "Pilar Ortega", "locale": "fr"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let members = alex.get("/api/household").await?.json["members"].clone();
    let pilar = members
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["display_name"] == "Pilar Ortega")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        (&pilar["enrolled"], &pilar["invite_pending"]),
        (&json!(false), &json!(true))
    );
    Ok(())
}
