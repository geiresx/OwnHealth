//! What a done profile delete logs when its last audit row fails. Its own test binary: tracing caches
//! callsite interest globally, so a test that installs a thread-local subscriber races with any other
//! test in the same binary that hits the same callsite first.

#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use ownhealth_core::documents::{NewDocument, create_document};
use support::Env;

/// A writer the test reads back.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut v) = self.0.lock() {
            v.extend_from_slice(b);
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_lost_file_audit_is_logged_by_profile_id_with_the_counts_and_no_name() -> anyhow::Result<()> {
    let env = Env::new(true).await?;
    let alex = env.profile("alex");
    let report = b"%PDF-1.7 synthetic LabNorte report";
    let sha = env.rt.blobs.put_bytes(report)?;
    let mut c = env.pool().acquire().await?;
    create_document(
        &mut c,
        &NewDocument::upload(
            env.fam().household_id,
            Some(alex),
            &sha,
            "application/pdf",
            i64::try_from(report.len())?,
        ),
    )
    .await?;
    // the audit row about the files fails, the way a full disk or a broken database would
    sqlx::query(
        "create function test_fail_audit() returns trigger language plpgsql as $$ begin \
         if new.resource_type = 'blob' then raise exception 'the audit is down'; end if; \
         return new; end $$",
    )
    .execute(&mut *c)
    .await?;
    sqlx::query(
        "create trigger test_fail_audit before insert on audit_event for each row \
         execute function test_fail_audit()",
    )
    .execute(&mut *c)
    .await?;
    drop(c);

    let out = Captured::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let mut me = env.as_user(env.user("alex")).await?;
    let r = me.delete("/api/profiles/alex?confirm=alex").await?;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    let text = String::from_utf8(out.0.lock().map(|v| v.clone()).unwrap_or_default())?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("could not audit what a done delete did with the files"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    for want in [format!("profile_id={alex}"), "removed=1".into(), "pending=0".into()] {
        assert!(lines[0].contains(&want), "{want} missing: {}", lines[0]);
    }
    for name in ["alex", "Alex", "Rivera"] {
        assert!(!lines[0].contains(name), "the line names the person: {}", lines[0]);
    }
    Ok(())
}
