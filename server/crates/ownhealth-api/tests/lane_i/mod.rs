//! Builders shared by lane I's route tests (`tests/conftest.py`, `tests/factories.py`): a throwaway
//! database with the Rivera family, the `/api` router, real session cookies. Synthetic values only.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode, header};
use ownhealth_auth::sessions::{COOKIE, create_session};
use ownhealth_auth::tokens::{TokenIn, create_token};
use ownhealth_core::Runtime;
use ownhealth_core::access::all_scopes;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::demo_pdf::{BBox, LabReport, LabRow, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::observations::ObservationIn;
use ownhealth_core::provenance::SrcIn;
use ownhealth_core::review::{ReviewItemIn, create_item};
use ownhealth_core::testing::TestDb;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use tower::ServiceExt;
use uuid::Uuid;

pub type R = anyhow::Result<()>;

pub struct App {
    pub db: TestDb,
    pub dir: tempfile::TempDir,
    pub rt: Runtime,
    pub family: Family,
}

/// A migrated database with the Rivera family committed. Dev auth is off: a request without a
/// session cookie is anonymous.
pub async fn app() -> anyhow::Result<App> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let mut settings = db.settings(dir.path())?;
    settings.dev_auth = false;
    let rt = Runtime::with_pool(settings, db.pool.clone())?;
    let mut c = db.pool.acquire().await?;
    let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
    drop(c);
    Ok(App { db, dir, rt, family })
}

impl App {
    pub fn api(&self) -> Router {
        ownhealth_api::router().with_state(self.rt.clone())
    }

    /// `Cookie: oh_session=...` for one of the family's users.
    pub async fn cookie(&self, who: &str) -> anyhow::Result<String> {
        let mut c = self.rt.pool.acquire().await?;
        let secret = create_session(&mut c, self.family.user(who), None, Some("test"), "passkey").await?;
        Ok(format!("{COOKIE}={secret}"))
    }

    /// A PAT of alex's with every scope.
    pub async fn pat(&self) -> anyhow::Result<String> {
        let mut c = self.rt.pool.acquire().await?;
        let t = create_token(
            &mut c,
            &TokenIn {
                user_id: self.family.user("alex"),
                label: "agent",
                profile_ids: vec![self.family.profile("alex")],
                scopes: all_scopes().into_iter().collect(),
                default_profile: None,
                days: 30,
                identity_mode: "full",
                lang: "en",
                units: "si",
            },
        )
        .await?;
        Ok(t.plaintext)
    }

    pub async fn send(&self, req: Request<Body>) -> anyhow::Result<(StatusCode, Value)> {
        let (s, _, body) = self.send_raw(req).await?;
        Ok((s, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    pub async fn send_raw(&self, req: Request<Body>) -> anyhow::Result<(StatusCode, http::HeaderMap, Vec<u8>)> {
        let res = self.api().oneshot(req).await?;
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 26).await?;
        Ok((status, headers, bytes.to_vec()))
    }

    pub async fn get(&self, who: &str, uri: &str) -> anyhow::Result<(StatusCode, Value)> {
        let req = Request::get(uri)
            .header(header::COOKIE, self.cookie(who).await?)
            .body(Body::empty())?;
        self.send(req).await
    }

    pub async fn post(&self, who: &str, uri: &str, body: Option<Value>) -> anyhow::Result<(StatusCode, Value)> {
        let b = Request::post(uri).header(header::COOKIE, self.cookie(who).await?);
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))?,
            None => b.body(Body::empty())?,
        };
        self.send(req).await
    }

    /// A multipart upload: (field, filename, bytes) parts plus text fields.
    pub async fn upload(
        &self,
        who: &str,
        fields: &[(&str, &str)],
        files: &[(&str, &[u8])],
    ) -> anyhow::Result<(StatusCode, Value)> {
        let boundary = "ownhealth-test-boundary";
        let mut body: Vec<u8> = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        for (filename, bytes) in files {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{filename}\"\r\n\
                     Content-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let req = Request::post("/imports")
            .header(header::COOKIE, self.cookie(who).await?)
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))?;
        self.send(req).await
    }

    pub async fn conn(&self) -> anyhow::Result<sqlx::pool::PoolConnection<sqlx::Postgres>> {
        Ok(self.rt.pool.acquire().await?)
    }

    pub async fn count(&self, sql: &str) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&self.rt.pool)
            .await?)
    }
}

/// `factories.obs`: an LDL value with every derived field set. `extra` overrides or adds fields.
pub fn obs(profile_id: Uuid, value: &str, extra: Value) -> ObservationIn {
    let mut v = json!({
        "profile_id": profile_id,
        "printed_name": "Colesterol LDL",
        "printed_value": value,
        "printed_unit": "mg/dL",
        "printed_ref_range": "< 130",
        "value_num": value,
        "ref_high": "130",
        "loinc": "13457-7",
        "alias_group": "ldl_c",
        "unit_ucum": "mg/dL",
        "value_canonical": value,
        "unit_canonical": "mg/dL",
        "derivation_version": "test",
        "effective_at": "2026-09-14T08:00:00+00:00",
    });
    if let (Some(m), Value::Object(e)) = (v.as_object_mut(), extra) {
        for (k, x) in e {
            if k == "day" {
                let day = x.as_str().unwrap_or("2026-09-14").to_string();
                m.insert("effective_at".into(), json!(format!("{day}T08:00:00+00:00")));
            } else {
                m.insert(k, x);
            }
        }
    }
    ObservationIn::from_json(v).unwrap_or_else(|e| panic!("bad test observation: {e}"))
}

pub fn manual_src() -> SrcIn {
    SrcIn::manual(None)
}

/// A one-page synthetic LabNorte PDF stored as a document. Returns (document_id, bboxes of page 1).
pub async fn lab_document(conn: &mut PgConnection, app: &App, slug: &str) -> anyhow::Result<(Uuid, Vec<BBox>)> {
    let (pdf, boxes) = lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id: "LN-T",
        pages: vec![vec![
            LabRow::new("Colesterol LDL", "97", "mg/dL", "< 130"),
            LabRow::new("Vitamina D", "24", "ng/mL", "30 - 100").flagged("L"),
        ]],
    });
    let sha = app.rt.blobs.put_bytes(&pdf)?;
    let r = create_document(
        conn,
        &NewDocument {
            filename: Some("labnorte.pdf"),
            pages: Some(1),
            ..NewDocument::upload(
                app.family.household_id,
                Some(app.family.profile(slug)),
                &sha,
                "application/pdf",
                pdf.len() as i64,
            )
        },
    )
    .await?;
    Ok((r.id, boxes.into_iter().next().unwrap_or_default()))
}

/// A synthetic LabNorte PDF with one glucose row (test_app.py's upload).
pub fn glucose_pdf(report_id: &str) -> Vec<u8> {
    lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id,
        pages: vec![vec![LabRow::new("Glucosa", "92", "mg/dL", "")]],
    })
    .0
}

/// A review item on `who`'s profile (security/test_review_scoping.py `_item`). Without a payload it
/// holds an LDL observation; `extra` goes into that observation.
pub async fn item(app: &App, who: &str, kind: &str, payload: Option<Value>, extra: Value) -> anyhow::Result<Uuid> {
    let pid = app.family.profile(who);
    let payload = match payload {
        Some(p) => p,
        None => {
            let o = serde_json::to_value(obs(pid, "97", extra))?;
            json!({"observation": o, "src": {"kind": "manual", "profile_id": pid.to_string()}})
        }
    };
    let mut c = app.conn().await?;
    Ok(create_item(
        &mut c,
        &ReviewItemIn {
            kind: kind.into(),
            household_id: Some(app.family.household_id),
            profile_id: Some(pid),
            payload,
            reasons: vec!["x".into()],
            ..Default::default()
        },
    )
    .await?)
}

pub fn obj(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}
