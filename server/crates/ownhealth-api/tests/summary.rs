//! `POST /api/profiles/{p}/summary.pdf`: step-up, read permission (a caregiver prints, a viewer gets
//! what its scopes allow), the sensitive genome gate (ticked, switched on, never for a minor), the
//! input checks and the audit row. People call over HTTP with real session cookies.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use ownhealth_genome::synthetic;
use ownhealth_ingest_pdf::textcheck::assert_text_layer;
use serde_json::{Value, json};
use support::Env;

type R = anyhow::Result<()>;

/// The Rivera family with Alex's and Lucía's synthetic genome facts (APOE e3/e4 is sensitive).
async fn env() -> anyhow::Result<Env> {
    let env = Env::new(true).await?;
    let mut c = env.pool().acquire().await?;
    let raw = synthetic::alex_raw_files()?;
    let (facts, att) = synthetic::with_files(&synthetic::alex_facts(), &env.rt.blobs, &raw)?;
    for (slug, facts, att) in [("alex", facts, att), ("lucia", synthetic::lucia_facts(), Vec::new())] {
        synthetic::seed_genome(
            &mut c,
            &env.rt.blobs,
            env.fam().household_id,
            env.profile(slug),
            &facts,
            &att,
            "genome-facts.json",
            None,
        )
        .await?;
    }
    Ok(env)
}

fn lines(pdf: &[u8]) -> Vec<String> {
    assert!(pdf.starts_with(b"%PDF-"), "not a PDF");
    assert_text_layer(pdf, |_| Ok(()))
}

async fn sensitive_ui(env: &Env, slug: &str, on: bool) -> R {
    sqlx::query("update profile set genome_sensitive_ui = $2 where id = $1")
        .bind(env.profile(slug))
        .bind(on)
        .execute(env.pool())
        .await?;
    Ok(())
}

#[tokio::test]
async fn printing_needs_a_session_and_a_fresh_step_up() -> R {
    let env = env().await?;
    let body = json!({"lang": "en", "specialty": "gp"});
    let r = env.anon().post("/api/profiles/alex/summary.pdf", &body).await?;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{:?}", r.json);
    let r = env
        .stale(env.user("alex"))
        .await?
        .post("/api/profiles/alex/summary.pdf", &body)
        .await?;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "step_up_required");
    let n = env
        .scalar_i64("select count(*) from audit_event where tool = 'POST /api/profiles/{p}/summary.pdf'")
        .await?;
    assert_eq!(n, 0, "a refused print writes no audit row");
    Ok(())
}

#[tokio::test]
async fn the_owner_prints_a_pdf_and_an_audit_row_is_written() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for (lang, title) in [
        ("es", "Resumen para el médico · Cardiología"),
        ("en", "Doctor summary · Cardiology"),
    ] {
        let r = alex
            .post(
                "/api/profiles/alex/summary.pdf",
                &json!({"lang": lang, "specialty": "cardiology"}),
            )
            .await?;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        assert_eq!(r.header("content-type"), "application/pdf");
        assert_eq!(r.header("cache-control"), "no-store");
        assert!(
            r.header("content-disposition")
                .starts_with("inline; filename=\"ownhealth-summary-alex-cardiology-")
        );
        let text = lines(&r.bytes);
        assert_eq!(text[0], title);
        assert!(text.iter().any(|l| l.contains("CYP2C19")), "{text:#?}");
    }
    type Row = (
        String,
        String,
        Option<String>,
        Option<Vec<String>>,
        Option<String>,
        Option<Value>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select via, action, resource_type, src_ids, args_summary, withheld from audit_event \
         where tool = 'POST /api/profiles/{p}/summary.pdf' and profile_id = $1 order by id",
    )
    .bind(env.profile("alex"))
    .fetch_all(env.pool())
    .await?;
    assert_eq!(rows.len(), 2);
    let (via, action, kind, srcs, args, withheld) = &rows[0];
    assert_eq!((via.as_str(), action.as_str()), ("export", "export"));
    assert_eq!(kind.as_deref(), Some("summary"));
    assert!(
        srcs.as_ref().is_some_and(|s| !s.is_empty()),
        "the printed src ids are named"
    );
    assert!(
        args.as_deref()
            .is_some_and(|a| a.starts_with("specialty=cardiology;lang=es;sensitive=0")),
        "{args:?}"
    );
    assert_eq!(withheld.as_ref().map(|w| w["by_scope"].clone()), Some(json!([])));
    Ok(())
}

#[tokio::test]
async fn a_caregiver_prints_and_a_viewer_gets_labs_records_and_genome_withheld() -> R {
    let env = env().await?;
    // Alex is Carmen's caregiver
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post(
            "/api/profiles/carmen/summary.pdf",
            &json!({"lang": "es", "specialty": "gp"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // Sam is a viewer of Alex: read:summary only
    let mut sam = env.as_user(env.user("sam")).await?;
    let r = sam
        .post(
            "/api/profiles/alex/summary.pdf",
            &json!({"lang": "en", "specialty": "gp"}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let text = lines(&r.bytes);
    assert!(text.iter().any(|l| l.starts_with("Lab values: not shown")), "{text:#?}");
    assert!(
        text.iter().any(|l| l.starts_with("Genome findings: not shown")),
        "{text:#?}"
    );
    assert!(
        !text.iter().any(|l| l.contains("CYP2C19") || l.contains("Lab range")),
        "{text:#?}"
    );
    let w: Value = sqlx::query_scalar(
        "select withheld from audit_event where tool = 'POST /api/profiles/{p}/summary.pdf' and actor_id = $1",
    )
    .bind(env.user("sam"))
    .fetch_one(env.pool())
    .await?;
    assert_eq!(w["by_scope"], json!(["labs", "records", "genome"]));
    Ok(())
}

#[tokio::test]
async fn a_sensitive_finding_prints_only_when_ticked_and_never_for_a_minor() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let mut apoe = async |slug: &str, tick: bool| -> anyhow::Result<bool> {
        let body = json!({"lang": "en", "specialty": "gp", "include_sensitive": tick});
        let r = alex.post(&format!("/api/profiles/{slug}/summary.pdf"), &body).await?;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let text = lines(&r.bytes);
        // the sheet goes to a third party: it never says that a gated finding exists, nor how many
        assert!(!text.iter().any(|l| l.contains("not printed")), "{text:#?}");
        Ok(text.iter().any(|l| l.contains("APOE")))
    };
    // the profile's UI switch is off: ticking alone does not open it
    sensitive_ui(&env, "alex", false).await?;
    assert!(!apoe("alex", true).await?);
    sensitive_ui(&env, "alex", true).await?;
    assert!(!apoe("alex", false).await?, "absent unless ticked");
    assert!(apoe("alex", true).await?, "ticked and allowed");
    // Lucía is 9: never, whatever the switch and the tick say
    sensitive_ui(&env, "lucia", true).await?;
    assert!(!apoe("lucia", true).await?);
    // nor by asking for a date on which she is an adult: a print is never dated after today
    let body = json!({"lang": "en", "specialty": "gp", "include_sensitive": true, "as_of": "2045-01-01"});
    let r = alex.post("/api/profiles/lucia/summary.pdf", &body).await?;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{:?}", r.json);
    Ok(())
}

#[tokio::test]
async fn an_unknown_specialty_or_language_is_refused() -> R {
    let env = env().await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for body in [
        json!({"lang": "en", "specialty": "dermatology"}),
        json!({"lang": "fr", "specialty": "gp"}),
    ] {
        let r = alex.post("/api/profiles/alex/summary.pdf", &body).await?;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    Ok(())
}
