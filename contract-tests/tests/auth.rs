//! Login flows over plain HTTP at 127.0.0.1, where passkeys cannot run (auth/password.py
//! `passkeys_possible`): invite, password enrolment, sessions and their cookie, password login,
//! recovery codes, step-up, logout, CSRF, and PATs refused on REST. Python: tests/security.

use anyhow::Context;
use ownhealth_contract_tests::shape::assert_shape;
use ownhealth_contract_tests::{Client, uniq};
use reqwest::Method;
use serde_json::{Value, json};

fn is_recovery_code(v: &Value) -> bool {
    let alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    v.as_str().is_some_and(|s| {
        let parts: Vec<&str> = s.split('-').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| p.len() == 4 && p.chars().all(|c| alphabet.contains(c)))
    })
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn state_as_the_dev_user_and_methods_on_plain_http() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (s, v) = c.get_json("/api/auth/state").await?;
    assert_eq!(s, 200);
    assert_eq!(v["setup_needed"], false);
    assert_eq!(v["passkeys_possible"], false);
    assert_eq!(v["password_allowed"], true);
    assert_eq!(v["oidc"], json!({"enabled": false, "label": null}));
    assert_eq!(v["auth_kind"], "dev");
    assert_eq!(v["method"], Value::Null);
    assert_eq!(v["step_up_fresh"], true);
    assert_eq!(v["step_up_seconds"], 300);
    assert_eq!(v["user"]["display_name"], "Alex Rivera");
    let keys: Vec<&String> = v["user"].as_object().context("user")?.keys().collect();
    assert_eq!(keys.len(), 6, "{keys:?}");

    // no passkey ceremony at a plain-HTTP address that is not localhost
    for path in ["/api/auth/setup/begin", "/api/auth/login/begin"] {
        let r = c.post(path, &json!({"display_name": "X"})).await?;
        assert_eq!(
            (r.status, r.code()),
            (400, Some("passkeys_unavailable")),
            "{path}"
        );
    }
    // first run is over: the demo has an owner
    let r = c
        .post(
            "/api/auth/setup/password",
            &json!({"display_name": "Someone", "password": "long enough password"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (409, Some("already_set_up")));
    // OIDC is not configured
    let r = c.get("/api/auth/oidc/login").await?;
    assert_eq!((r.status, r.code()), (404, Some("oidc_disabled")));
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn invite_enrol_with_password_and_the_session_cookie() -> anyhow::Result<()> {
    let c = Client::new()?;
    let name = format!("Invited {}", uniq());
    let inv = c
        .post(
            "/api/household/invites",
            &json!({"display_name": name, "locale": "en"}),
        )
        .await?
        .expect(201)?;
    assert_shape("invite_created", &inv.json)?;
    let code = inv.json["code"].as_str().context("code")?.to_string();
    assert_eq!(inv.json["expires_in_days"], 7);
    assert_eq!(inv.json["url"], format!("{}/enroll?code={code}", c.base));

    let info = c
        .get(&format!("/api/auth/invite/{code}"))
        .await?
        .expect(200)?;
    assert_eq!(
        info.json,
        json!({"display_name": name, "household": "Rivera (demo)", "claims_profile": false,
               "passkeys_possible": false, "password_allowed": true, "oidc": false})
    );
    let r = c.get("/api/auth/invite/not-a-real-invite-code").await?;
    assert_eq!((r.status, r.code()), (404, Some("invite_invalid")));

    // the household shows the pending invite
    let hh = c.get("/api/household").await?.expect(200)?;
    let m = hh.json["members"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["display_name"] == name)
        .context("member")?;
    assert_eq!(
        (m["invite_pending"].clone(), m["enrolled"].clone()),
        (json!(true), json!(false))
    );

    // too short a password is the model's 422
    let r = c
        .post(
            "/api/auth/enroll/password",
            &json!({"code": code, "password": "short"}),
        )
        .await?;
    assert_eq!(r.status, 422);
    assert!(r.json["detail"].is_array(), "{}", r.json);

    let password = format!("correct horse {}", uniq());
    let r = c
        .post(
            "/api/auth/enroll/password",
            &json!({"code": code, "password": password}),
        )
        .await?
        .expect(200)?;
    let codes = r.json["recovery_codes"].as_array().context("codes")?;
    assert_eq!(codes.len(), 10);
    assert!(codes.iter().all(is_recovery_code), "{codes:?}");
    assert!(r.json["user_id"].is_string());
    let cookie = r.set_cookie("oh_session").context("no oh_session cookie")?;
    let lower = cookie.to_lowercase();
    assert!(lower.contains("httponly"), "{cookie}");
    assert!(lower.contains("samesite=lax"), "{cookie}");
    assert!(lower.contains("path=/"), "{cookie}");
    assert!(lower.contains("max-age=43200"), "{cookie}");
    assert!(
        !lower.contains("secure"),
        "plain http must not set Secure: {cookie}"
    );
    let secret = r.cookie_value("oh_session").context("value")?;
    assert_eq!(secret.len(), 43, "token_urlsafe(32)");

    // the invite is used up
    let r = c
        .post(
            "/api/auth/enroll/password",
            &json!({"code": code, "password": password}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (400, Some("invite_invalid")));

    // the session is the member, with step-up fresh after a password login
    let s = c.with_session(&secret);
    let st = s.get("/api/auth/state").await?.expect(200)?;
    assert_eq!(st.json["auth_kind"], "session");
    assert_eq!(st.json["method"], "password");
    assert_eq!(st.json["step_up_fresh"], true);
    assert_eq!(st.json["user"]["display_name"], name);
    assert_eq!(st.json["user"]["recovery_codes_left"], 10);
    assert_eq!(st.json["user"]["passkeys"], 0);
    assert_shape("auth_state_session", &st.json)?;
    let me = s.get("/api/me").await?.expect(200)?;
    assert_eq!(me.json["auth"]["kind"], "session");
    assert_eq!(me.json["user"]["display_name"], name);
    assert_eq!(
        me.json["profiles"],
        json!([]),
        "a new member sees no profile until granted"
    );
    assert_eq!(me.json["review_open"], 0);

    // the enrolment is in the member's own audit log as a login (Alex does not see it)
    let audit = s.audit(500).await?;
    assert!(
        !c.audit(500)
            .await?
            .iter()
            .any(|e| e["actor"] == json!(name))
    );
    assert!(
        audit.iter().any(|e| e["action"] == "login"
            && e["tool"] == "auth:password"
            && e["actor"] == json!(name)),
        "no login audit row"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn password_login_logout_and_wrong_passwords() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.enrolled_member(&format!("Login {}", uniq())).await?;
    let r = c
        .post(
            "/api/auth/password/login",
            &json!({"name": m.name, "password": "wrong password here"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (401, Some("login_failed")));
    assert!(r.set_cookie("oh_session").is_none());
    let r = c
        .post(
            "/api/auth/password/login",
            &json!({"name": "Nobody At All", "password": "wrong password here"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (401, Some("login_failed")));
    // the name matches case-insensitively, with spaces trimmed
    let r = c
        .post(
            "/api/auth/password/login",
            &json!({"name": format!("  {}  ", m.name.to_uppercase()), "password": m.password}),
        )
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"user_id": m.user_id}));
    let secret = r.cookie_value("oh_session").context("cookie")?;
    assert_ne!(secret, m.session, "every login is a new session");
    let s = c.with_session(&secret);
    assert_eq!(s.get("/api/me").await?.json["user"]["id"], json!(m.user_id));

    // logout revokes the session and clears the cookie
    let r = s.post("/api/auth/logout", &json!({})).await?.expect(200)?;
    assert_eq!(r.json, json!({"ok": true}));
    let cleared = r
        .set_cookie("oh_session")
        .context("logout must clear the cookie")?;
    assert!(
        cleared.to_lowercase().contains("max-age=0") || cleared.contains("expires="),
        "{cleared}"
    );
    // the old cookie no longer is the member (dev auth answers as Alex again)
    let me = s.get("/api/me").await?.expect(200)?;
    assert_eq!(me.json["auth"]["kind"], "dev");
    assert_eq!(me.json["user"]["display_name"], "Alex Rivera");
    // the enrolment session is still alive
    assert_eq!(
        m.client.get("/api/me").await?.json["user"]["id"],
        json!(m.user_id)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn recovery_codes_log_in_once_without_step_up() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.enrolled_member(&format!("Recover {}", uniq())).await?;
    let code = m.recovery_codes[0].as_str().context("code")?.to_string();
    let r = c
        .post("/api/auth/recovery", &json!({"code": code}))
        .await?
        .expect(200)?;
    assert_eq!(
        r.json,
        json!({"user_id": m.user_id, "recovery_codes_left": 9})
    );
    let secret = r.cookie_value("oh_session").context("cookie")?;
    let r2 = c.post("/api/auth/recovery", &json!({"code": code})).await?;
    assert_eq!((r2.status, r2.code()), (401, Some("recovery_invalid")));
    let r2 = c
        .post("/api/auth/recovery", &json!({"code": "nope"}))
        .await?;
    assert_eq!((r2.status, r2.code()), (401, Some("recovery_invalid")));

    // codes are normalised: lower case, no dashes
    let second = m.recovery_codes[1]
        .as_str()
        .context("code")?
        .to_lowercase()
        .replace('-', " ");
    c.post("/api/auth/recovery", &json!({"code": second}))
        .await?
        .expect(200)?;

    let s = c.with_session(&secret);
    let st = s.get("/api/auth/state").await?.expect(200)?;
    assert_eq!(st.json["method"], "recovery");
    assert_eq!(st.json["step_up_fresh"], false);
    assert_eq!(st.json["user"]["recovery_codes_left"], 8);

    // a recovery session is not a fresh login: step-up routes refuse it
    let r = s
        .post("/api/tokens", &json!({"label": "x", "profiles": ["alex"]}))
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("step_up_required")));
    let r = s.post("/api/auth/recovery-codes", &json!({})).await?;
    assert_eq!((r.status, r.code()), (403, Some("step_up_required")));

    // step-up with the password, then new codes; the old ones stop working
    let b = s
        .post("/api/auth/step-up/begin", &json!({}))
        .await?
        .expect(200)?;
    assert_eq!(b.json, json!({"method": "password"}));
    let r = s
        .post(
            "/api/auth/step-up/finish",
            &json!({"password": "not the password"}),
        )
        .await?;
    assert_eq!((r.status, r.code()), (401, Some("login_failed")));
    let r = s.post("/api/auth/step-up/finish", &json!({})).await?;
    assert_eq!((r.status, r.code()), (422, Some("bad_input")));
    let r = s
        .post("/api/auth/step-up/finish", &json!({"password": m.password}))
        .await?
        .expect(200)?;
    assert_eq!(r.json, json!({"ok": true, "valid_seconds": 300}));
    assert_eq!(s.get("/api/auth/state").await?.json["step_up_fresh"], true);
    let r = s
        .post("/api/auth/recovery-codes", &json!({}))
        .await?
        .expect(200)?;
    let fresh = r.json["recovery_codes"].as_array().context("codes")?;
    assert_eq!(fresh.len(), 10);
    assert!(fresh.iter().all(is_recovery_code));
    let old = m.recovery_codes[2].as_str().context("code")?;
    let r = c.post("/api/auth/recovery", &json!({"code": old})).await?;
    assert_eq!((r.status, r.code()), (401, Some("recovery_invalid")));
    assert_eq!(
        s.get("/api/auth/state").await?.json["user"]["recovery_codes_left"],
        10
    );
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn passkey_management_on_plain_http() -> anyhow::Result<()> {
    let c = Client::new()?;
    let m = c.enrolled_member(&format!("Keys {}", uniq())).await?;
    let r = m.client.get("/api/auth/passkeys").await?.expect(200)?;
    assert_eq!(r.json, json!({"items": []}));
    let r = m
        .client
        .post("/api/auth/passkeys/begin", &json!({}))
        .await?;
    assert_eq!((r.status, r.code()), (400, Some("passkeys_unavailable")));
    // the password is another way in, so deleting is allowed, and an unknown id is 404
    let r = m.client.delete("/api/auth/passkeys/AAAA").await?;
    assert_eq!((r.status, r.code()), (404, Some("unknown")));
    // passwords are refused for step-up only when a passkey could be used
    let r = m
        .client
        .post("/api/auth/step-up/finish", &json!({"password": m.password}))
        .await?
        .expect(200)?;
    assert_eq!(r.json["ok"], true);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn csrf_refuses_cross_site_writes_on_cookies_and_dev_auth() -> anyhow::Result<()> {
    let c = Client::new()?;
    let body = json!({"display_name": format!("Csrf {}", uniq()), "role": "guardian"});
    let evil = [("origin", "https://evil.example")];
    let r = c
        .send(Method::POST, "/api/household/profiles", Some(&body), &evil)
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("csrf_failed")));
    let r = c
        .send(
            Method::POST,
            "/api/household/profiles",
            Some(&body),
            &[("sec-fetch-site", "cross-site")],
        )
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("csrf_failed")));
    let r = c
        .send(
            Method::POST,
            "/api/household/profiles",
            Some(&body),
            &[("referer", "https://evil.example/page")],
        )
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("csrf_failed")));
    // the public login routes check the origin too
    let r = c
        .send(
            Method::POST,
            "/api/auth/recovery",
            Some(&json!({"code": "AAAA-AAAA-AAAA"})),
            &evil,
        )
        .await?;
    assert_eq!((r.status, r.code()), (403, Some("csrf_failed")));
    // reads are not checked
    let r = c.send(Method::GET, "/api/me", None, &evil).await?;
    assert_eq!(r.status, 200);
    // our own origin and the request's own host pass
    let base = c.base.clone();
    let r = c
        .send(
            Method::POST,
            "/api/household/profiles",
            Some(&body),
            &[("origin", base.as_str())],
        )
        .await?;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let host = base.trim_start_matches("http://").to_string();
    let r = c
        .send(
            Method::POST,
            "/api/household/profiles",
            Some(&json!({"display_name": format!("Csrf {}", uniq()), "role": "guardian"})),
            &[
                ("origin", format!("https://{host}").as_str()),
                ("sec-fetch-site", "same-origin"),
            ],
        )
        .await?;
    assert_eq!(r.status, 201);
    Ok(())
}

#[tokio::test]
#[ignore = "needs a running backend: contract-tests/run.sh rust"]
async fn access_tokens_do_not_work_on_rest() -> anyhow::Result<()> {
    let c = Client::new()?;
    let (_, pat) = c.token(json!({"scopes": ["read:summary"]})).await?;
    let auth = format!("Bearer {pat}");
    for path in [
        "/api/me",
        "/api/profiles",
        "/api/profiles/alex/status",
        "/api/tokens",
    ] {
        let r = c
            .send(Method::GET, path, None, &[("authorization", auth.as_str())])
            .await?;
        assert_eq!((r.status, r.code()), (403, Some("forbidden")), "{path}");
    }
    // a bad bearer or another scheme is no caller at all, never the dev user
    for h in [
        "Bearer oh_pat_AAAAAAAA_nope",
        "Basic YWxleDpwdw==",
        "Bearer ",
    ] {
        let r = c
            .send(Method::GET, "/api/me", None, &[("authorization", h)])
            .await?;
        assert_eq!((r.status, r.code()), (401, Some("unauthenticated")), "{h}");
        assert_eq!(r.json["detail"]["message"], "Log in first.");
    }
    Ok(())
}
