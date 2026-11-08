//! Fixtures for lane F (`tests/mcp/conftest.py`, `mcp_checks.py`): a committed Rivera household with
//! labs, records, a note and genome findings, a PAT factory, an MCP client over the real router, and
//! fakes for the lane services. Synthetic data only.
//!
//! The fakes are deliberately careless: the genome fake returns sensitive and minor-withheld findings
//! and the state fake returns gene x drug flags and medications to every caller, so the tests prove the
//! MCP layer itself keeps them away from tokens that may not see them.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use chrono::{NaiveDate, TimeZone, Utc};
use futures::future::BoxFuture;
use http::{HeaderMap, Request, StatusCode};
use ownhealth_auth::tokens::{TokenIn, create_token};
use ownhealth_core::Runtime;
use ownhealth_core::access::{Scopes, all_scopes, set_genome_switches};
use ownhealth_core::blobs::BlobStore;
use ownhealth_core::demo::{Family, create_rivera_family};
use ownhealth_core::demo_pdf::{LabReport, LabRow, lab_report_pdf};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef, ensure_src};
use ownhealth_core::records::{RecordIn, RecordOptions, active_medications, add_record};
use ownhealth_core::testing::TestDb;
use ownhealth_core::{Error, Result};
use ownhealth_mcp::envelope::dumps as py_dumps;
use ownhealth_mcp::services::{FindingsArgs, Lanes, LookupError, ProposedRow, Services, VariantArgs};
use ownhealth_normalize::Normalized;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use tower::ServiceExt;
use uuid::Uuid;

pub type R = anyhow::Result<()>;

pub fn all() -> Scopes {
    all_scopes()
}

pub fn scopes(list: &[&str]) -> Scopes {
    list.iter().map(|s| s.to_string()).collect()
}

pub fn without(list: &[&str]) -> Scopes {
    let mut s = all_scopes();
    for x in list {
        s.remove(*x);
    }
    s
}

// ------------------------------------------------------------------ world ----

pub struct World {
    pub db: TestDb,
    pub dir: tempfile::TempDir,
    pub rt: Runtime,
    pub family: Family,
    pub src: HashMap<String, String>,
    pub doc_id: Uuid,
    pub boxes: Vec<[f64; 4]>,
    pub fakes: Arc<Fakes>,
}

impl World {
    pub fn pid(&self, slug: &str) -> Uuid {
        self.family.profiles[slug]
    }

    pub fn user(&self, slug: &str) -> Uuid {
        self.family.users[slug]
    }

    pub fn s(&self, key: &str) -> String {
        self.src[key].clone()
    }

    pub async fn conn(&self) -> anyhow::Result<sqlx::pool::PoolConnection<sqlx::Postgres>> {
        Ok(self.db.pool.acquire().await?)
    }

    /// The router with the careless fakes (the `fakes` fixture).
    pub fn app(&self) -> Router {
        let svc: Arc<dyn Services> = self.fakes.clone();
        ownhealth_mcp::router_with(self.rt.clone(), svc)
    }

    /// The router with the services as they ship.
    pub fn real_app(&self) -> Router {
        ownhealth_mcp::router(self.rt.clone())
    }

    pub async fn token(&self, t: Tok) -> anyhow::Result<String> {
        let mut c = self.conn().await?;
        let profile_ids = t.profiles.iter().map(|p| self.pid(p)).collect();
        let tok = create_token(
            &mut c,
            &TokenIn {
                user_id: self.user(t.user),
                label: t.label,
                profile_ids,
                scopes: t.scopes.iter().cloned().collect(),
                default_profile: None,
                days: 90,
                identity_mode: t.identity_mode,
                lang: t.lang,
                units: t.units,
            },
        )
        .await?;
        Ok(tok.plaintext)
    }

    /// `make_token()` with every default.
    pub async fn tok(&self) -> anyhow::Result<String> {
        self.token(Tok::default()).await
    }

    pub async fn tok_scopes(&self, s: Scopes) -> anyhow::Result<String> {
        self.token(Tok {
            scopes: s,
            ..Tok::default()
        })
        .await
    }

    pub async fn connect(&self, token: &str) -> anyhow::Result<Client> {
        Client::connect(self.app(), Some(token), "2026-07-28").await
    }

    pub async fn connect_real(&self, token: &str) -> anyhow::Result<Client> {
        Client::connect(self.real_app(), Some(token), "2026-07-28").await
    }

    pub async fn token_id(&self, plaintext: &str) -> anyhow::Result<Uuid> {
        Ok(sqlx::query_scalar("select id from api_token where prefix = $1")
            .bind(&plaintext[..15])
            .fetch_one(&self.db.pool)
            .await?)
    }

    pub async fn exec(&self, sql: &str) -> R {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&self.db.pool)
            .await?;
        Ok(())
    }

    pub async fn set_switch(&self, slug: &str, on: bool) -> R {
        let mut c = self.conn().await?;
        set_genome_switches(&mut c, self.pid(slug), None, Some(on)).await?;
        Ok(())
    }

    pub async fn set_limit(&self, tok: &str, col: &str, v: i64) -> R {
        let sql = format!("update api_token set {col} = $1 where prefix = $2");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(v)
            .bind(&tok[..15])
            .execute(&self.db.pool)
            .await?;
        Ok(())
    }
}

pub struct Tok {
    pub scopes: Scopes,
    pub profiles: Vec<&'static str>,
    pub user: &'static str,
    pub label: &'static str,
    pub lang: &'static str,
    pub identity_mode: &'static str,
    pub units: &'static str,
}

impl Default for Tok {
    fn default() -> Self {
        Tok {
            scopes: all_scopes(),
            profiles: vec!["alex"],
            user: "alex",
            label: "Claude Code · test",
            lang: "en",
            identity_mode: "full",
            units: "si",
        }
    }
}

/// One observation as the Python `_obs` builder made it.
#[derive(Clone)]
pub struct Obs {
    pub name: String,
    pub value: String,
    pub unit: Option<String>,
    pub group: String,
    pub loinc: Option<String>,
    pub day: String,
    pub low: Option<String>,
    pub high: Option<String>,
    pub flag: Option<String>,
    pub category: String,
    pub sensitivity: Vec<String>,
    pub document_id: Option<Uuid>,
    pub value_text: Option<String>,
    /// Fields set after the model is built (`printed=` in Python).
    pub printed: Map<String, Value>,
}

pub fn obs(name: &str, value: &str, unit: Option<&str>, group: &str, loinc: Option<&str>, day: &str) -> Obs {
    Obs {
        name: name.into(),
        value: value.into(),
        unit: unit.map(str::to_string),
        group: group.into(),
        loinc: loinc.map(str::to_string),
        day: day.into(),
        low: None,
        high: None,
        flag: None,
        category: "laboratory".into(),
        sensitivity: Vec::new(),
        document_id: None,
        value_text: None,
        printed: Map::new(),
    }
}

impl Obs {
    pub fn high(mut self, h: &str) -> Obs {
        self.high = Some(h.into());
        self
    }
    pub fn low(mut self, l: &str) -> Obs {
        self.low = Some(l.into());
        self
    }
    pub fn flag(mut self, f: &str) -> Obs {
        self.flag = Some(f.into());
        self
    }
    pub fn category(mut self, c: &str) -> Obs {
        self.category = c.into();
        self
    }
    pub fn sensitive(mut self, s: &str) -> Obs {
        self.sensitivity = vec![s.into()];
        self
    }
    pub fn doc(mut self, d: Uuid) -> Obs {
        self.document_id = Some(d);
        self
    }
    pub fn text(mut self, t: &str) -> Obs {
        self.value_text = Some(t.into());
        self
    }
    pub fn printed(mut self, k: &str, v: Value) -> Obs {
        self.printed.insert(k.into(), v);
        self
    }

    pub fn model(&self, profile_id: Uuid) -> anyhow::Result<ObservationIn> {
        let num = self.value_text.is_none().then(|| self.value.clone());
        let printed_ref_range = match (&self.low, &self.high) {
            (Some(l), Some(h)) => Some(format!("{l} - {h}")),
            (None, Some(h)) => Some(format!("< {h}")),
            _ => None,
        };
        let mut v = json!({
            "profile_id": profile_id,
            "category": self.category,
            "printed_name": self.name,
            "printed_value": self.value_text.clone().unwrap_or_else(|| self.value.clone()),
            "printed_unit": self.unit,
            "printed_ref_range": printed_ref_range,
            "value_num": num,
            "value_text": self.value_text,
            "ref_low": self.low,
            "ref_high": self.high,
            "loinc": self.loinc,
            "alias_group": self.group,
            "unit_ucum": self.unit,
            "value_canonical": num,
            "unit_canonical": if num.is_some() { json!(self.unit) } else { Value::Null },
            "interpretation": self.flag,
            "derivation_version": "test",
            "effective_at": format!("{}T08:00:00+00:00", self.day),
            "sensitivity": self.sensitivity,
            "document_id": self.document_id,
        });
        if let Some(m) = v.as_object_mut() {
            m.extend(self.printed.clone());
        }
        Ok(ObservationIn::from_json(v)?)
    }
}

pub fn manual(profile_id: Uuid) -> SrcIn {
    let mut s = SrcIn::new("manual");
    s.profile_id = Some(profile_id);
    s.actor_kind = "system".into();
    s
}

pub fn region(profile_id: Uuid, doc: Uuid, bbox: [f64; 4]) -> SrcIn {
    let mut s = SrcIn::new("document_region");
    s.profile_id = Some(profile_id);
    s.document_id = Some(doc);
    s.page_no = Some(1);
    s.bbox = Some(bbox);
    s
}

pub async fn add_obs(c: &mut PgConnection, profile_id: Uuid, o: Obs, src: Option<SrcIn>) -> anyhow::Result<String> {
    let m = o.model(profile_id)?;
    let src = src.unwrap_or_else(|| manual(profile_id));
    Ok(
        commit_observation(c, &m, &SrcRef::New(src), CommitOptions::new("person"))
            .await?
            .src_id,
    )
}

pub async fn add_record_json(c: &mut PgConnection, kind: &str, v: Value, profile_id: Uuid) -> anyhow::Result<String> {
    let r = RecordIn::from_json(kind, v)?;
    Ok(
        add_record(c, &r, &SrcRef::New(manual(profile_id)), RecordOptions::new("person"))
            .await?
            .src_id,
    )
}

pub async fn add_dataset(c: &mut PgConnection, profile_id: Uuid) -> anyhow::Result<Uuid> {
    let mut s = SrcIn::new("genome_facts");
    s.profile_id = Some(profile_id);
    s.import_ref = Some("facts".into());
    let sid = ensure_src(&mut *c, &SrcRef::New(s), Some(profile_id)).await?;
    let h: Vec<u8> = (0..32).map(|_| rand_byte()).collect();
    Ok(sqlx::query_scalar(
        "insert into genome_dataset (profile_id, sample_id, assay, facts_schema, facts_sha256, facts_blob, \
         owngenome_version, db_versions, src_id) values ($1, 'S1', 'wgs', 'ownhealth.genome-facts/v1', $2, $2, \
         '1.4.2', '{\"clinvar\": \"2026-10-28\"}', $3) returning id",
    )
    .bind(profile_id)
    .bind(h)
    .bind(sid)
    .fetch_one(c)
    .await?)
}

fn rand_byte() -> u8 {
    Uuid::new_v4().as_bytes()[0]
}

pub fn fact_hex() -> String {
    format!("f_{}", &Uuid::new_v4().simple().to_string()[..16])
}

#[derive(Default)]
pub struct Finding<'a> {
    pub sensitive: bool,
    pub adult_onset: bool,
    pub carrier: bool,
    pub phenotype: Option<&'a str>,
    pub diplotype: Option<&'a str>,
    pub fact: Option<Value>,
}

#[allow(clippy::too_many_arguments)]
pub async fn add_finding(
    c: &mut PgConnection,
    profile_id: Uuid,
    dataset: Uuid,
    fact_id: &str,
    category: &str,
    gene: Option<&str>,
    f: Finding<'_>,
) -> anyhow::Result<String> {
    let mut s = SrcIn::new("genome_facts");
    s.profile_id = Some(profile_id);
    s.import_ref = Some(format!("facts#{fact_id}"));
    let sid = ensure_src(&mut *c, &SrcRef::New(s), Some(profile_id)).await?;
    sqlx::query(
        "insert into genome_finding (dataset_id, profile_id, fact_id, category, gene, diplotype, phenotype, \
         evidence, confidence, call_state, sensitive, adult_onset, carrier, status, src_id, fact) values \
         ($1, $2, $3, $4, $5, $6, $7, 'E1', 'high', 'alt', $8, $9, $10, 'current', $11, $12)",
    )
    .bind(dataset)
    .bind(profile_id)
    .bind(fact_id)
    .bind(category)
    .bind(gene)
    .bind(f.diplotype)
    .bind(f.phenotype)
    .bind(f.sensitive)
    .bind(f.adult_onset)
    .bind(f.carrier)
    .bind(&sid)
    .bind(f.fact.unwrap_or_else(|| json!({})))
    .execute(c)
    .await?;
    Ok(sid)
}

/// A one-page synthetic LabNorte PDF stored as a document (`factories.lab_document`).
async fn lab_document(
    c: &mut PgConnection,
    blobs: &BlobStore,
    family: &Family,
) -> anyhow::Result<(Uuid, Vec<[f64; 4]>)> {
    let rows = vec![
        LabRow::new("Colesterol LDL", "97", "mg/dL", "< 130"),
        LabRow::new("Vitamina D", "24", "ng/mL", "30 - 100").flagged("L"),
    ];
    let (pdf, boxes) = lab_report_pdf(&LabReport {
        lab: "LabNorte",
        title: "Informe",
        patient: "RIVERA, ALEX",
        birth: "14/05/1979",
        collected: "14/09/2026",
        report_id: "LN-T",
        pages: vec![rows],
    });
    let sha = blobs.put_bytes(&pdf)?;
    let mut d = NewDocument::upload(
        family.household_id,
        Some(family.profiles["alex"]),
        &sha,
        "application/pdf",
        pdf.len() as i64,
    );
    d.filename = Some("labnorte.pdf");
    d.pages = Some(1);
    let r = create_document(c, &d).await?;
    Ok((r.id, boxes.into_iter().next().unwrap_or_default()))
}

/// The `world` fixture.
pub async fn world() -> anyhow::Result<World> {
    let db = TestDb::new().await?;
    let dir = tempfile::tempdir()?;
    let rt = Runtime::with_pool(db.settings(dir.path())?, db.pool.clone())?;
    ownhealth_mcp::register();
    let mut c = db.pool.acquire().await?;
    let fam = create_rivera_family(&mut c, &format!("Rivera {}", &Uuid::new_v4().simple().to_string()[..6])).await?;
    let alex = fam.profiles["alex"];
    let lucia = fam.profiles["lucia"];
    let mut src = HashMap::new();
    for (day, v) in [
        ("2021-11-09", "163"),
        ("2022-09-15", "96"),
        ("2023-03-21", "88"),
        ("2025-01-20", "79"),
        ("2025-09-08", "101"),
        ("2026-03-02", "118"),
    ] {
        let flag = if v.parse::<i32>()? > 130 { "H" } else { "N" };
        let o = obs("Colesterol LDL", v, Some("mg/dL"), "ldl_c", Some("13457-7"), day)
            .high("130")
            .flag(flag);
        src.insert(format!("ldl_{day}"), add_obs(&mut c, alex, o, None).await?);
    }
    let o = obs(
        "LDL directo",
        "92",
        Some("mg/dL"),
        "ldl_c",
        Some("18262-6"),
        "2023-10-04",
    )
    .high("130")
    .flag("N");
    src.insert("ldl_direct".into(), add_obs(&mut c, alex, o, None).await?);
    let (doc_id, boxes) = lab_document(&mut c, &rt.blobs, &fam).await?;
    sqlx::query("update document set collected_at = $1, reported_at = $1, provider_name = 'LabNorte' where id = $2")
        .bind(Utc.with_ymd_and_hms(2026, 9, 14, 8, 0, 0).single())
        .bind(doc_id)
        .execute(&mut *c)
        .await?;
    let o = obs(
        "Vitamina D",
        "24",
        Some("ng/mL"),
        "vitamin_d_25oh",
        Some("62292-8"),
        "2026-09-14",
    )
    .low("30")
    .high("100")
    .flag("L")
    .doc(doc_id);
    src.insert(
        "vitd".into(),
        add_obs(&mut c, alex, o, Some(region(alex, doc_id, boxes[1]))).await?,
    );
    let o = obs(
        "Colesterol LDL",
        "97",
        Some("mg/dL"),
        "ldl_c",
        Some("13457-7"),
        "2026-09-14",
    )
    .high("130")
    .flag("N")
    .doc(doc_id);
    src.insert(
        "ldl_latest".into(),
        add_obs(&mut c, alex, o, Some(region(alex, doc_id, boxes[0]))).await?,
    );
    let o = obs("Peso", "78.5", Some("kg"), "body_weight", Some("29463-7"), "2026-10-01").category("vital-signs");
    src.insert("weight".into(), add_obs(&mut c, alex, o, None).await?);
    let o = obs("PHQ-9", "6", Some("{score}"), "phq9", Some("44261-6"), "2026-06-01")
        .category("survey")
        .sensitive("mental_health");
    src.insert("phq9".into(), add_obs(&mut c, alex, o, None).await?);
    let med = json!({"profile_id": alex, "name": "Atorvastatin 20 mg", "started": "2026-03-10"});
    src.insert("statin".into(), add_record_json(&mut c, "medication", med, alex).await?);
    let note_src = ensure_src(&mut c, &SrcRef::New(manual(alex)), Some(alex)).await?;
    sqlx::query(
        "insert into note (profile_id, kind, author_kind, lang, attach_to, body, effective_at, src_id) \
         values ($1, 'priorities', 'person', 'en', 'status', 'Recheck vitamin D in winter.', now(), $2)",
    )
    .bind(alex)
    .bind(&note_src)
    .execute(&mut *c)
    .await?;
    src.insert("note".into(), note_src);
    let ds = add_dataset(&mut c, alex).await?;
    let f = Finding {
        phenotype: Some("Decreased function"),
        diplotype: Some("*1/*5"),
        ..Finding::default()
    };
    src.insert(
        "slco1b1".into(),
        add_finding(&mut c, alex, ds, &fact_hex(), "pgx", Some("SLCO1B1"), f).await?,
    );
    let f = Finding {
        sensitive: true,
        adult_onset: true,
        fact: Some(
            json!({"variants": [{"chrom": "chr19", "pos": 44908684, "ref": "T", "alt": "C", "rsid": "rs429358"}]}),
        ),
        ..Finding::default()
    };
    src.insert(
        "apoe".into(),
        add_finding(&mut c, alex, ds, &fact_hex(), "risk_marker", Some("APOE"), f).await?,
    );
    let f = Finding {
        carrier: true,
        ..Finding::default()
    };
    src.insert(
        "hfe".into(),
        add_finding(&mut c, alex, ds, &fact_hex(), "carrier", Some("HFE"), f).await?,
    );
    let ds_l = add_dataset(&mut c, lucia).await?;
    let f = Finding {
        phenotype: Some("Normal metabolizer"),
        ..Finding::default()
    };
    src.insert(
        "lucia_pgx".into(),
        add_finding(&mut c, lucia, ds_l, &fact_hex(), "pgx", Some("CYP2C19"), f).await?,
    );
    let f = Finding {
        carrier: true,
        ..Finding::default()
    };
    src.insert(
        "lucia_carrier".into(),
        add_finding(&mut c, lucia, ds_l, &fact_hex(), "carrier", Some("CFTR"), f).await?,
    );
    let o = obs(
        "Colesterol LDL",
        "110",
        Some("mg/dL"),
        "ldl_c",
        Some("13457-7"),
        "2026-08-01",
    )
    .high("130");
    src.insert("sam_ldl".into(), add_obs(&mut c, fam.profiles["sam"], o, None).await?);
    drop(c);
    Ok(World {
        db,
        dir,
        rt,
        family: fam,
        src,
        doc_id,
        boxes,
        fakes: Arc::new(Fakes::default()),
    })
}

// ----------------------------------------------------------------- client ----

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error {}: {}", self.code, self.message)
    }
}

#[derive(Debug, Clone)]
pub struct ToolRes {
    pub is_error: bool,
    pub sc: Value,
    pub text: String,
    pub raw: Value,
}

impl ToolRes {
    /// structuredContent of a successful call.
    pub fn ok(&self) -> Value {
        assert!(!self.is_error, "{}", self.sc);
        self.sc.clone()
    }

    pub fn err(&self) -> Value {
        assert!(self.is_error, "{}", self.sc);
        self.sc.clone()
    }
}

/// An MCP client session over the router (Python's `connect` fixture): `2026-07-28` sends the
/// per-request `_meta`; `legacy` runs the 2025-11-25 handshake first.
pub struct Client {
    app: Router,
    token: Option<String>,
    pub protocol_version: String,
    legacy: bool,
    id: AtomicI64,
}

pub struct Http {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Http {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub async fn send(app: &Router, req: Request<Body>) -> anyhow::Result<Http> {
    let res = app.clone().oneshot(req).await?;
    let status = res.status();
    let headers = res.headers().clone();
    let body = to_bytes(res.into_body(), 64 << 20).await?.to_vec();
    Ok(Http { status, headers, body })
}

pub fn post(body: &Value, headers: &[(&str, &str)]) -> anyhow::Result<Request<Body>> {
    let mut b = Request::post("/mcp")
        .header("host", "127.0.0.1:8080")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    Ok(b.body(Body::from(body.to_string()))?)
}

impl Client {
    pub async fn connect(app: Router, token: Option<&str>, mode: &str) -> anyhow::Result<Client> {
        let legacy = mode == "legacy";
        let mut c = Client {
            app,
            token: token.map(str::to_string),
            protocol_version: if legacy {
                "2025-11-25".into()
            } else {
                "2026-07-28".into()
            },
            legacy,
            id: AtomicI64::new(1),
        };
        if legacy {
            let init = c
                .rpc(
                    "initialize",
                    json!({"protocolVersion": "2025-11-25", "capabilities": {},
                           "clientInfo": {"name": "ownhealth-tests", "version": "1"}}),
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            c.protocol_version = init["protocolVersion"].as_str().unwrap_or_default().to_string();
        }
        Ok(c)
    }

    pub async fn raw(&self, method: &str, params: Value) -> anyhow::Result<Http> {
        let mut params = params;
        if !self.legacy
            && method != "initialize"
            && let Some(m) = params.as_object_mut()
        {
            {
                m.insert(
                    "_meta".into(),
                    json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                           "io.modelcontextprotocol/clientCapabilities": {},
                           "io.modelcontextprotocol/clientInfo": {"name": "ownhealth-tests", "version": "1"}}),
                );
            }
        }
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let auth = self.token.as_ref().map(|t| format!("Bearer {t}"));
        let mut headers: Vec<(&str, &str)> = vec![("mcp-protocol-version", self.protocol_version.as_str())];
        if method == "initialize" {
            headers.clear();
        }
        let name_key = match method {
            "tools/call" | "prompts/get" => Some("name"),
            "resources/read" => Some("uri"),
            _ => None,
        };
        let name = name_key.and_then(|k| params[k].as_str()).map(str::to_string);
        if !self.legacy && method != "initialize" {
            headers.push(("mcp-method", method));
            if let Some(n) = &name {
                headers.push(("mcp-name", n.as_str()));
            }
        }
        if let Some(a) = &auth {
            headers.push(("authorization", a.as_str()));
        }
        send(&self.app, post(&body, &headers)?).await
    }

    pub async fn rpc(&self, method: &str, params: Value) -> std::result::Result<Value, RpcError> {
        let r = self.raw(method, params).await.map_err(|e| RpcError {
            code: 0,
            message: e.to_string(),
        })?;
        let v = r.json();
        if let Some(e) = v.get("error") {
            return Err(RpcError {
                code: e["code"].as_i64().unwrap_or_default(),
                message: e["message"].as_str().unwrap_or_default().to_string(),
            });
        }
        if r.status != StatusCode::OK {
            return Err(RpcError {
                code: i64::from(r.status.as_u16()),
                message: String::from_utf8_lossy(&r.body).into_owned(),
            });
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<ToolRes> {
        let r = self
            .rpc("tools/call", json!({"name": name, "arguments": args}))
            .await
            .map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
        Ok(ToolRes {
            is_error: r["isError"].as_bool().unwrap_or(false),
            sc: r.get("structuredContent").cloned().unwrap_or(Value::Null),
            text: r
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            raw: r,
        })
    }

    /// `call` of test_mcp_tools.py: a successful result is checked for envelope, text block and src.
    pub async fn call(&self, name: &str, args: Value) -> anyhow::Result<ToolRes> {
        let r = self.call_tool(name, args).await?;
        if !r.is_error {
            assert_envelope(&r.sc);
            assert_eq!(r.raw.pointer("/content/0/type"), Some(&json!("text")));
            assert!(r.text.starts_with(&format!("OwnHealth {name}")), "{}", r.text);
            for f in facts(&r.sc["data"]) {
                assert!(crate::common::truthy(&f["src"]), "{f}");
            }
        }
        Ok(r)
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<Value>> {
        let r = self
            .rpc("tools/list", json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(r["tools"].as_array().cloned().unwrap_or_default())
    }

    pub async fn tool_names(&self) -> anyhow::Result<BTreeSet<String>> {
        Ok(self
            .list_tools()
            .await?
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect())
    }

    pub async fn list_resources(&self) -> anyhow::Result<Vec<String>> {
        let r = self
            .rpc("resources/list", json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(r["resources"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["uri"].as_str().map(str::to_string)).collect())
            .unwrap_or_default())
    }

    pub async fn read_resource(&self, uri: &str) -> std::result::Result<Value, RpcError> {
        self.rpc("resources/read", json!({"uri": uri})).await
    }

    /// The text of the first content block of a resource.
    pub async fn read_text(&self, uri: &str) -> anyhow::Result<String> {
        let r = self
            .read_resource(uri)
            .await
            .map_err(|e| anyhow::anyhow!("{uri}: {e}"))?;
        Ok(r.pointer("/contents/0/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

// ------------------------------------------------------------- checks ----

pub fn truthy(v: &Value) -> bool {
    ownhealth_mcp::envelope::truthy(v)
}

pub fn assert_envelope(env: &Value) {
    let keys: Vec<&str> = env
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let want: Vec<&str> = ownhealth_mcp::envelope::ENVELOPE_KEYS.to_vec();
    assert_eq!(keys, want);
    let w: BTreeSet<&str> = env["withheld"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    assert_eq!(w, ["by_scope", "minor_rule", "sensitive"].into_iter().collect());
    assert!(truthy(&env["notice"]));
}

/// Every object that carries a src.
pub fn facts(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    fn go(n: &Value, out: &mut Vec<Value>) {
        match n {
            Value::Object(m) => {
                if m.get("src").is_some_and(truthy) {
                    out.push(n.clone());
                }
                for v in m.values() {
                    go(v, out);
                }
            }
            Value::Array(a) => a.iter().for_each(|v| go(v, out)),
            _ => {}
        }
    }
    go(v, &mut out);
    out
}

pub fn all_srcs(v: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in facts(v) {
        match &f["src"] {
            Value::String(s) => {
                out.insert(s.clone());
            }
            Value::Array(a) => out.extend(a.iter().filter_map(|x| x.as_str().map(str::to_string))),
            _ => {}
        }
    }
    out
}

/// `json.dumps(obj, ensure_ascii=False)`, for "is this string anywhere" checks.
pub fn dumps(v: &Value) -> String {
    py_dumps(v)
}

/// Paths where `marker` appears in a string that is not the text of an untrusted_text wrapper.
pub fn marker_outside_wrappers(v: &Value, marker: &str) -> Vec<String> {
    let mut bad = Vec::new();
    fn go(n: &Value, path: String, inside: bool, marker: &str, bad: &mut Vec<String>) {
        match n {
            Value::Object(m) => {
                let wrapped = m.get("trust").and_then(Value::as_str) == Some("untrusted_text");
                for (k, v) in m {
                    go(v, format!("{path}.{k}"), wrapped && k == "text", marker, bad);
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    go(v, format!("{path}[{i}]"), false, marker, bad);
                }
            }
            Value::String(s) if s.contains(marker) && !inside => bad.push(path),
            _ => {}
        }
    }
    go(v, "$".into(), false, marker, &mut bad);
    bad
}

pub fn marker_outside_tags(text: &str, marker: &str) -> bool {
    let re = regex::Regex::new(r#"(?s)<untrusted_text origin="[a-z_]+">.*?</untrusted_text>"#).unwrap();
    re.replace_all(text, "").contains(marker)
}

pub fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}

// -------------------------------------------------------------- fakes ----

/// Which stand-in `build_state` / `render` / `check_drug` / `page_words` answer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateMode {
    #[default]
    Normal,
    /// 79 flags (test_trend_and_status_budgets).
    ManyFlags,
    /// Three 4000-character notes.
    Noted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrugMode {
    #[default]
    Normal,
    ManyLabs,
}

#[derive(Default)]
pub struct Fakes {
    pub state: Mutex<StateMode>,
    pub drug: Mutex<DrugMode>,
    pub page_words: Mutex<Option<Vec<String>>>,
}

impl Fakes {
    pub fn set_state(&self, m: StateMode) {
        *self.state.lock().unwrap() = m;
    }
    pub fn set_drug(&self, m: DrugMode) {
        *self.drug.lock().unwrap() = m;
    }
    pub fn set_page_words(&self, w: Vec<String>) {
        *self.page_words.lock().unwrap() = Some(w);
    }
    fn state_mode(&self) -> StateMode {
        *self.state.lock().unwrap()
    }
}

fn canon(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sort(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    py_dumps(&sort(v))
}

fn title(s: &str) -> String {
    let mut out = String::new();
    let mut prev_letter = false;
    for ch in s.chars() {
        if prev_letter {
            out.extend(ch.to_lowercase());
        } else {
            out.extend(ch.to_uppercase());
        }
        prev_letter = ch.is_alphabetic();
    }
    out
}

async fn fake_build_state(conn: &mut PgConnection, profile_id: Uuid, as_of: NaiveDate) -> Result<Value> {
    type Row = (
        String,
        Option<String>,
        String,
        Option<rust_decimal::Decimal>,
        Option<String>,
        Option<String>,
        chrono::DateTime<Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "select src_id, loinc, alias_group, value_num, unit_ucum, interpretation, effective_at from observation \
         where profile_id = $1 and status = 'current' and interpretation in ('H','L') and effective_at::date <= $2 \
         order by effective_at desc",
    )
    .bind(profile_id)
    .bind(as_of)
    .fetch_all(&mut *conn)
    .await?;
    let mut flags: Vec<Value> = rows
        .iter()
        .map(|(src, loinc, group, v, unit, interp, at)| {
            json!({
                "kind": "out_of_range",
                "reason": if interp.as_deref() == Some("H") { "LAB_HIGH" } else { "LAB_LOW" },
                "code": format!("LOINC:{}", loinc.clone().unwrap_or_else(|| "None".into())),
                "alias_group": group,
                "value": v.map(|d| d.to_string()).unwrap_or_else(|| "None".into()),
                "unit": unit,
                "flag": interp,
                "date": at.date_naive().format("%Y-%m-%d").to_string(),
                "src": src,
            })
        })
        .collect();
    let meds = active_medications(&mut *conn, profile_id, Some(as_of), None, None, true).await?;
    let gene: Option<String> =
        sqlx::query_scalar("select src_id from genome_finding where profile_id = $1 and gene = 'SLCO1B1'")
            .bind(profile_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let (Some(g), Some(m)) = (gene, meds.first()) {
        flags.push(
            json!({"kind": "gene_drug", "reason": "GENE_DRUG", "gene": "SLCO1B1", "drug": "atorvastatin",
                          "src": [g, m["src_id"]]}),
        );
    }
    let notes: Vec<(String, Option<String>)> = sqlx::query_as("select body, src_id from note where profile_id = $1")
        .bind(profile_id)
        .fetch_all(&mut *conn)
        .await?;
    let ranked: Vec<Value> = flags
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let mut m = Map::new();
            m.insert("rank".into(), json!(i + 1));
            if let Value::Object(o) = f {
                m.extend(o);
            }
            Value::Object(m)
        })
        .collect();
    let mut body = json!({
        "schema": "ownhealth.state/v1",
        "rules": "ownhealth-rules 0.1.0",
        "as_of": as_of.format("%Y-%m-%d").to_string(),
        "flags": ranked,
        "medications": meds.iter().map(|m| json!({"name": m["name"], "since": m["started"], "src": m["src_id"]})).collect::<Vec<_>>(),
        "never_measured": [{"alias_group": "hcv_ab", "rule": "infect.hcv.once_adult"}],
        "stale": [],
        "notes": notes.iter().map(|(b, s)| json!({"author_kind": "person", "body": b, "src": s})).collect::<Vec<_>>(),
        "data_freshness": {"labs": "2026-09-14", "genome": "2026-10-02"},
    });
    let h = hex::encode(Sha256::digest(canon(&body).as_bytes()));
    body["state_hash"] = json!(format!("sha256:{h}"));
    Ok(body)
}

fn fake_render(body: &Value, lang: &str) -> Value {
    let mut out: Map<String, Value> = body
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !["schema", "rules", "as_of"].contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    out.insert("rules_version".into(), body["rules"].clone());
    let word = if lang == "en" {
        "Out of the lab's range"
    } else {
        "Fuera del rango del laboratorio"
    };
    let flags: Vec<Value> = body["flags"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut f| {
            if f["kind"] == "out_of_range" {
                f["text"] = json!(word);
            }
            f
        })
        .collect();
    out.insert("flags".into(), json!(flags));
    let notes: Vec<Value> = body["notes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|n| json!({"author_kind": n["author_kind"], "text": {"trust": "untrusted_text", "origin": "person_note", "text": n["body"]}, "src": n["src"]}))
        .collect();
    out.insert("notes".into(), json!(notes));
    out.insert("limitations".into(), json!(["Flags use each lab's own printed range."]));
    Value::Object(out)
}

impl Services for Fakes {
    fn rules_version(&self) -> Result<String> {
        Ok("ownhealth-rules 0.1.0".into())
    }

    fn rule_pack(&self) -> Result<Value> {
        Lanes.rule_pack()
    }

    fn build_state<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>> {
        let mode = self.state_mode();
        Box::pin(async move {
            match mode {
                StateMode::Normal => fake_build_state(conn, profile_id, as_of).await,
                StateMode::ManyFlags => {
                    let flags: Vec<Value> = (1..80)
                        .map(|i| {
                            json!({"rank": i, "kind": "out_of_range", "code": format!("LOINC:{i}-0"), "value": "1",
                                        "unit": "mg/dL", "src": "src_none"})
                        })
                        .collect();
                    Ok(json!({"rules": "ownhealth-rules 0.1.0", "flags": flags, "state_hash": "sha256:abc"}))
                }
                StateMode::Noted => {
                    let notes: Vec<Value> = (1..4)
                        .map(|i| {
                            json!({"author_kind": "person", "date": format!("2026-10-0{i}"), "text": "x".repeat(4000),
                                        "src": format!("src_n{i}")})
                        })
                        .collect();
                    let flags = json!([{"rank": 1, "kind": "lab_range", "rule": "r", "code": "LOINC:1-0", "text": "t", "src": "src_f"}]);
                    Ok(
                        json!({"rules": "ownhealth-rules 0.1.0", "flags": flags, "notes": notes, "state_hash": "sha256:abc"}),
                    )
                }
            }
        })
    }

    fn render(&self, body: &Value, lang: &str, _units: &str, _identity_mode: &str) -> Result<Value> {
        match self.state_mode() {
            StateMode::ManyFlags | StateMode::Noted => {
                let mut b = body.clone();
                b["rules_version"] = body["rules"].clone();
                Ok(b)
            }
            _ => Ok(fake_render(body, lang)),
        }
    }

    fn check_drug<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        drug: &'a str,
        _scopes: &'a Scopes,
        _lang: &'a str,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>> {
        let mode = *self.drug.lock().unwrap();
        Box::pin(async move {
            if mode == DrugMode::ManyLabs {
                let labs: Vec<Value> = (0..300)
                    .map(|i| json!({"group": format!("analyte_{i:03}"), "name": "Analito ".repeat(20), "src": format!("src_l{i}")}))
                    .collect();
                return Ok(json!({"drug": {"name": drug, "query": drug}, "pgx": [], "relevant_labs": labs}));
            }
            let g: Option<String> =
                sqlx::query_scalar("select src_id from genome_finding where profile_id = $1 and gene = 'SLCO1B1'")
                    .bind(profile_id)
                    .fetch_optional(&mut *conn)
                    .await?;
            let meds = active_medications(&mut *conn, profile_id, Some(as_of), None, None, true).await?;
            let pgx = match g {
                Some(g) => {
                    json!([{"gene": "SLCO1B1", "phenotype": "Decreased function", "call_state": "alt", "src": g}])
                }
                None => json!([]),
            };
            Ok(json!({
                "drug": {"name": title(drug), "cpic_drug_id": "RxNorm:83367", "currently_taking": !meds.is_empty()},
                "pgx": pgx,
                "phenoconversion": [],
                "current_medications_relevant": meds.iter().map(|m| json!({"name": m["name"], "src": m["src_id"]})).collect::<Vec<_>>(),
                "relevant_labs": [],
                "guidance": [{"source": "CPIC", "applies_to": "Decreased function", "quote": "Prescribe a lower dose.",
                              "url": "https://cpicpgx.org/guidelines/"}],
                "not_assessed": ["drug-drug interactions (no open-licence source configured)"],
                "db_versions": {"cpic": "2026-09"},
            }))
        })
    }

    fn resolve_drug<'a>(&'a self, _conn: &'a mut PgConnection, name: &'a str) -> BoxFuture<'a, Result<Option<Value>>> {
        let hit = name.to_lowercase().contains("statin");
        let name = name.to_string();
        Box::pin(async move { Ok(hit.then(|| json!({"cpic_drug_id": "RxNorm:83367", "name": name}))) })
    }

    fn list_findings<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        _gate: &'a ownhealth_genome::Gate,
        q: &'a FindingsArgs,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            type Row = (
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                String,
                String,
                String,
                String,
            );
            let rows: Vec<Row> = sqlx::query_as(
                "select fact_id, category, gene, diplotype, phenotype, evidence, confidence, call_state, src_id \
                 from genome_finding where profile_id = $1 and status = 'current' \
                 and ($2::text is null or category = $2) and ($3::text is null or gene = $3) order by gene",
            )
            .bind(profile_id)
            .bind(&q.category)
            .bind(&q.gene)
            .fetch_all(conn)
            .await?;
            let findings: Vec<Value> = rows
                .into_iter()
                .map(|(id, category, gene, diplotype, phenotype, evidence, confidence, call_state, src)| {
                    json!({"id": id, "category": category, "gene": gene, "diplotype": diplotype, "phenotype": phenotype,
                           "evidence": evidence, "confidence": confidence, "call_state": call_state, "src": src})
                })
                .collect();
            Ok(json!({
                "findings": findings,
                "not_assessed": [{"gene": "CYP2D6", "what": "structural variants", "reason": "no read data in source"}],
                "withheld_sensitive": 0,
                "withheld_minor": 0,
                "db_versions": {"clinvar": "2026-10-28"},
                "assay": {"type": "wgs", "build": "GRCh38"},
            }))
        })
    }

    fn lookup_variant<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        _blobs: &'a BlobStore,
        profile_id: Uuid,
        _gate: &'a ownhealth_genome::Gate,
        q: &'a VariantArgs,
    ) -> BoxFuture<'a, std::result::Result<Value, LookupError>> {
        Box::pin(async move {
            let src: Option<String> =
                sqlx::query_scalar("select src_id from genome_dataset where profile_id = $1 limit 1")
                    .bind(profile_id)
                    .fetch_optional(conn)
                    .await
                    .map_err(|e| LookupError::Other(Error::Db(e)))?;
            if let Some(region) = &q.region {
                let page: i64 = q.cursor.as_deref().and_then(|c| c.parse().ok()).unwrap_or(0);
                let calls: Vec<Value> = (page..page + 2)
                    .map(|i| json!({"source": "wgs", "call_state": "ref", "pos": 1000 + i, "src": src}))
                    .collect();
                let next = if page < 2 {
                    json!((page + 2).to_string())
                } else {
                    Value::Null
                };
                return Ok(json!({"query": region, "calls": calls, "next_cursor": next}));
            }
            Ok(json!({
                "query": q.rsid.clone().or_else(|| q.hgvs.clone()),
                "variant": {"gene": "HFE", "chrom": "chr6", "pos": 26092913},
                "calls": [{"source": "wgs", "call_state": "alt", "genotype": "G/A", "src": src}],
                "db_versions": {"clinvar": "2026-10-28"},
            }))
        })
    }

    fn resolve_analyte<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        _name: &'a str,
        _lang: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    fn normalize_row<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        row: &'a ProposedRow,
    ) -> BoxFuture<'a, Result<Normalized>> {
        Box::pin(async move {
            let text = row.printed_value.replace(',', ".");
            let value: rust_decimal::Decimal = text
                .parse()
                .map_err(|_| Error::Invalid(format!("not a number: {text}")))?;
            let obs = ObservationIn::from_json(json!({
                "profile_id": row.profile_id,
                "printed_name": row.printed_name,
                "printed_value": row.printed_value,
                "printed_unit": row.printed_unit,
                "value_num": value.to_string(),
                "alias_group": "ferritin",
                "loinc": "2276-4",
                "unit_ucum": row.printed_unit,
                "value_canonical": value.to_string(),
                "unit_canonical": row.printed_unit,
                "derivation_version": "test",
                "effective_at": ownhealth_core::pyjson::py_isoformat(&row.effective_at),
            }))?;
            let plausible = value < rust_decimal::Decimal::from(100_000);
            let check: ownhealth_core::acceptance::Check = serde_json::from_value(
                json!({"name": "plausible", "ok": plausible, "reason": if plausible { Value::Null } else { json!("implausible") }}),
            )?;
            Ok(Normalized {
                observation: obs,
                checks: vec![check],
            })
        })
    }

    fn ground_value<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        _blobs: &'a BlobStore,
        _document_id: Uuid,
        _page_no: i32,
        _name: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<Option<[f64; 4]>>> {
        let v = value.replace(',', ".");
        Box::pin(async move {
            Ok(match v.as_str() {
                "97" => Some([300.0, 120.0, 320.0, 129.0]),
                "24" => Some([300.0, 136.0, 320.0, 145.0]),
                _ => None,
            })
        })
    }

    fn page_words<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        _blobs: &'a BlobStore,
        _document_id: Uuid,
        _page_no: i32,
    ) -> BoxFuture<'a, Result<Vec<String>>> {
        let words = self
            .page_words
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| vec!["Colesterol".into(), "LDL".into()]);
        Box::pin(async move { Ok(words) })
    }
}

/// The services as they ship, with planted page words (Python monkeypatches `pdf.page_words` in
/// the `planted` fixture whichever services run).
pub struct RealWords(pub Vec<String>);

impl Services for RealWords {
    fn rules_version(&self) -> Result<String> {
        Lanes.rules_version()
    }
    fn rule_pack(&self) -> Result<Value> {
        Lanes.rule_pack()
    }
    fn build_state<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>> {
        Lanes.build_state(conn, profile_id, as_of)
    }
    fn render(&self, body: &Value, lang: &str, units: &str, identity_mode: &str) -> Result<Value> {
        Lanes.render(body, lang, units, identity_mode)
    }
    fn check_drug<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        drug: &'a str,
        scopes: &'a Scopes,
        lang: &'a str,
        as_of: NaiveDate,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(Lanes.check_drug(conn, profile_id, drug, scopes, lang, as_of))
    }
    fn resolve_drug<'a>(&'a self, conn: &'a mut PgConnection, name: &'a str) -> BoxFuture<'a, Result<Option<Value>>> {
        Box::pin(Lanes.resolve_drug(conn, name))
    }
    fn list_findings<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        profile_id: Uuid,
        gate: &'a ownhealth_genome::Gate,
        q: &'a FindingsArgs,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(Lanes.list_findings(conn, profile_id, gate, q))
    }
    fn lookup_variant<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        profile_id: Uuid,
        gate: &'a ownhealth_genome::Gate,
        q: &'a VariantArgs,
    ) -> BoxFuture<'a, std::result::Result<Value, LookupError>> {
        Box::pin(Lanes.lookup_variant(conn, blobs, profile_id, gate, q))
    }
    fn resolve_analyte<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        name: &'a str,
        lang: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(Lanes.resolve_analyte(conn, name, lang))
    }
    fn normalize_row<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        row: &'a ProposedRow,
    ) -> BoxFuture<'a, Result<Normalized>> {
        Box::pin(Lanes.normalize_row(conn, row))
    }
    fn ground_value<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        blobs: &'a BlobStore,
        document_id: Uuid,
        page_no: i32,
        name: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, Result<Option<[f64; 4]>>> {
        Box::pin(Lanes.ground_value(conn, blobs, document_id, page_no, name, value))
    }
    fn page_words<'a>(
        &'a self,
        _conn: &'a mut PgConnection,
        _blobs: &'a BlobStore,
        _document_id: Uuid,
        _page_no: i32,
    ) -> BoxFuture<'a, Result<Vec<String>>> {
        let words = self.0.clone();
        Box::pin(async move { Ok(words) })
    }
}
