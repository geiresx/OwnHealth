//! Full export of one profile (`export/__init__.py`): FHIR R4 NDJSON plus the original files, in
//! one zip. A UI action behind step-up auth; never an MCP tool (mcp.md 9.2, security-privacy.md 12).
//!
//! ```text
//! export-manifest.json                  format, versions, and sha256 + size of every entry
//! fhir/Patient.ndjson                   the person
//! fhir/<ResourceType>.ndjson            every clinical row, all lifecycle states (meta.tag says which)
//! fhir/DocumentReference.ndjson         one per document, pointing at files/
//! fhir/Provenance.ndjson                one per src id: which facts it backs, document, page, bbox
//! files/<sha256>/<filename>             the original uploads, byte for byte
//! genome/<dataset>/...                  genome-facts.json, the VCF and its index
//! ownhealth/<table>.ndjson              rows with no FHIR shape (wearables, notes, findings, review, audit)
//! ```
//!
//! Rows stream from Postgres into the zip, and files stream from the blob store, so memory stays flat:
//! each NDJSON file builds in a spooled temp file (memory up to 8 MB, then disk) and is copied in when
//! the export finishes; files from the blob store go straight in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Read, Seek, Write};

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use futures::TryStreamExt;
use ownhealth_core::blobs::{BlobError, BlobStore};
use ownhealth_core::pyjson::{py_date, py_isoformat_utc};
use ownhealth_core::{Error, Result};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{AssertSqlSafe, PgConnection, Row};
use tempfile::SpooledTempFile;
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

pub const FORMAT: &str = "ownhealth.export/v1";
pub const LIFECYCLE_TAG: &str = "urn:ownhealth:lifecycle";
/// Not part of a person's export: access control and secrets, inbox configuration (v0.2: token hashes,
/// secret references, routing rules), and tables that only repeat the profile.
const SKIP_RAW: [&str; 5] = ["profile", "profile_grant", "api_token", "sender_rule", "source"];
/// These name the person `patient`, not `subject`.
const PATIENT_FIELD: [&str; 3] = ["AllergyIntolerance", "FamilyMemberHistory", "Immunization"];
const GENOME_FILES: [(&str, &str); 4] = [
    ("facts_blob", "genome-facts.json"),
    ("vcf_blob", "genome.vcf.gz"),
    ("tbi_blob", "genome.vcf.gz.tbi"),
    ("coverage_blob", "coverage.bin"),
];

fn zip_err(e: zip::result::ZipError) -> Error {
    Error::Io(io::Error::other(e))
}

fn safe_name(name: Option<&str>) -> String {
    let base = name.unwrap_or("").rsplit('/').next().unwrap_or("");
    let cleaned = Regex::new(r"[^A-Za-z0-9._-]+")
        .map(|re| re.replace_all(base, "_").into_owned())
        .unwrap_or_default();
    let trimmed: String = cleaned.trim_matches(['.', '_']).chars().take(80).collect();
    if trimmed.is_empty() { "file".into() } else { trimmed }
}

/// Python's `json.dumps(..., ensure_ascii=True)` on top of serde_json's output: non-ASCII only
/// occurs inside strings, so escaping every such character there is exact.
fn ensure_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

fn zip_time(now: DateTime<Utc>) -> zip::DateTime {
    zip::DateTime::from_date_and_time(
        u16::try_from(now.year()).unwrap_or(1980),
        now.month() as u8,
        now.day() as u8,
        now.hour() as u8,
        now.minute() as u8,
        now.second() as u8,
    )
    .unwrap_or_default()
}

struct Lines {
    buf: SpooledTempFile,
    hash: Sha256,
    lines: u64,
    bytes: u64,
}

/// Writes zip entries and remembers sha256, size and line count for the manifest.
struct Writer<W: Write + Seek> {
    zip: ZipWriter<W>,
    files: Vec<Value>,
    paths: HashSet<String>,
    open: BTreeMap<String, Lines>,
}

impl<W: Write + Seek> Writer<W> {
    fn new(out: W) -> Self {
        Writer {
            zip: ZipWriter::new(out),
            files: Vec::new(),
            paths: HashSet::new(),
            open: BTreeMap::new(),
        }
    }

    fn line(&mut self, path: &str, obj: &Value) -> Result<()> {
        let mut data = serde_json::to_vec(obj)?;
        data.push(b'\n');
        let l = self.open.entry(path.to_string()).or_insert_with(|| Lines {
            buf: SpooledTempFile::new(8 << 20),
            hash: Sha256::new(),
            lines: 0,
            bytes: 0,
        });
        l.buf.write_all(&data)?;
        l.hash.update(&data);
        l.lines += 1;
        l.bytes += data.len() as u64;
        Ok(())
    }

    /// Make sure the file exists in the zip even when no line is written (Python opened it eagerly).
    fn touch(&mut self, path: &str) {
        self.open.entry(path.to_string()).or_insert_with(|| Lines {
            buf: SpooledTempFile::new(8 << 20),
            hash: Sha256::new(),
            lines: 0,
            bytes: 0,
        });
    }

    fn deflated() -> SimpleFileOptions {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(zip_time(Utc::now()))
            .large_file(true)
    }

    fn close_lines(&mut self) -> Result<()> {
        for (path, mut l) in std::mem::take(&mut self.open) {
            l.buf.rewind()?;
            self.zip.start_file(path.as_str(), Self::deflated()).map_err(zip_err)?;
            io::copy(&mut l.buf, &mut self.zip)?;
            self.files.push(json!({
                "path": path, "sha256": hex::encode(l.hash.finalize()), "bytes": l.bytes, "lines": l.lines,
            }));
        }
        Ok(())
    }

    /// Copy one blob in. False when the store does not have it (the caller records the gap).
    fn blob(&mut self, path: &str, blobs: &BlobStore, sha: &str) -> Result<bool> {
        if self.paths.contains(path) {
            return Ok(true);
        }
        // opened before the zip entry, so a vanished blob leaves no stub
        let mut src = match blobs.open(sha) {
            Ok(f) => f,
            Err(BlobError::NotFound(_)) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        self.paths.insert(path.to_string());
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .last_modified_time(zip_time(Utc::now()))
            .large_file(true);
        self.zip.start_file(path, opts).map_err(zip_err)?;
        let (mut h, mut size) = (Sha256::new(), 0u64);
        let mut chunk = vec![0u8; 1 << 20];
        loop {
            let n = src.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            self.zip.write_all(&chunk[..n])?;
            h.update(&chunk[..n]);
            size += n as u64;
        }
        self.files
            .push(json!({"path": path, "sha256": hex::encode(h.finalize()), "bytes": size, "source_blob": sha}));
        Ok(true)
    }

    fn finish(mut self, manifest: &Value) -> Result<W> {
        let text = ensure_ascii(&serde_json::to_string_pretty(manifest)?);
        let opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(zip_time(Utc::now()));
        self.zip.start_file("export-manifest.json", opts).map_err(zip_err)?;
        self.zip.write_all(text.as_bytes())?;
        self.zip.finish().map_err(zip_err)
    }
}

/// (tables with a FHIR column, other tables) among those with a profile_id column.
async fn tables(conn: &mut PgConnection) -> Result<(Vec<String>, Vec<String>)> {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "select c.table_name::text, bool_or(f.column_name is not null) as has_fhir from information_schema.columns c \
         join information_schema.tables t on t.table_schema = c.table_schema and t.table_name = c.table_name \
         left join information_schema.columns f on f.table_schema = c.table_schema \
          and f.table_name = c.table_name and f.column_name = 'fhir' \
         where c.table_schema = current_schema() and c.column_name = 'profile_id' and t.table_type = 'BASE TABLE' \
         group by c.table_name order by c.table_name",
    )
    .fetch_all(&mut *conn)
    .await?;
    let keep = |(t, _): &&(String, bool)| !SKIP_RAW.contains(&t.as_str());
    let fhir = rows
        .iter()
        .filter(keep)
        .filter(|(_, f)| *f)
        .map(|(t, _)| t.clone())
        .collect();
    let raw = rows
        .iter()
        .filter(keep)
        .filter(|(_, f)| !*f)
        .map(|(t, _)| t.clone())
        .collect();
    Ok((fhir, raw))
}

async fn columns(conn: &mut PgConnection, table: &str) -> Result<HashSet<String>> {
    let cols: Vec<String> = sqlx::query_scalar(
        "select column_name::text from information_schema.columns where table_schema = current_schema() \
         and table_name = $1",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await?;
    Ok(cols.into_iter().collect())
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// A float4 as psycopg returned it (parsed from Postgres' shortest text) and Python printed it.
fn py_float(v: f32) -> String {
    ownhealth_core::pyjson::f32_json(v).to_string()
}

/// `json.dumps({"document_id": ..., "page": ..., "bbox": ...})` with Python's default separators.
fn location(document_id: Uuid, page: Option<i32>, bbox: Option<&[f32]>) -> String {
    let page = page.map_or_else(|| "null".into(), |p| p.to_string());
    let bbox = bbox.map_or_else(
        || "null".into(),
        |b| format!("[{}]", b.iter().map(|v| py_float(*v)).collect::<Vec<_>>().join(", ")),
    );
    format!("{{\"document_id\": \"{document_id}\", \"page\": {page}, \"bbox\": {bbox}}}")
}

/// Run blocking file work (blob copies, deflate) without stalling the async runtime. On the
/// multi-thread runtime `serve` uses, the worker hands its other tasks to another thread first;
/// on a current-thread runtime (unit tests) `block_in_place` would panic, so it runs inline.
pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

fn bump(counts: &mut Map<String, Value>, key: &str, by: u64) {
    let n = counts.get(key).and_then(Value::as_u64).unwrap_or(0);
    counts.insert(key.to_string(), json!(n + by));
}

/// A stored `resourceType` names a zip entry (`fhir/<type>.ndjson`), so only a plain FHIR type
/// name (ASCII letters and digits) is used; anything else (`../../x`) exports as `Basic`.
fn safe_resource_type(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Write the full export of one profile into `out` and return the manifest. `out` is written to,
/// never stored: the export does not land in the blob store or in backups.
///
/// The export runs several queries (FHIR tables, documents, sources). For a consistent archive
/// the caller passes a connection inside a `repeatable read` transaction, so a concurrent import
/// cannot add a source whose document the export already skipped.
pub async fn export_profile<W: Write + Seek>(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    profile_id: Uuid,
    out: W,
    fmt: &str,
) -> Result<Value> {
    if fmt != "zip" {
        return Err(Error::Invalid(format!("unknown export format {fmt}")));
    }
    type ProfileRow = (
        Uuid,
        String,
        String,
        Option<NaiveDate>,
        Option<String>,
        bool,
        Option<String>,
    );
    let prof: Option<ProfileRow> = sqlx::query_as(
        "select id, slug, display_name, birth_date, sex_at_birth, deceased, country from profile where id = $1",
    )
    .bind(profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((pid, slug, display_name, birth, sex, deceased, country)) = prof else {
        return Err(Error::NotFound("profile not found".into()));
    };
    let mut w = Writer::new(out);
    let mut patient = Map::new();
    patient.insert("resourceType".into(), json!("Patient"));
    patient.insert("id".into(), json!(pid.to_string()));
    patient.insert("name".into(), json!([{"text": display_name}]));
    if let Some(b) = birth {
        patient.insert("birthDate".into(), json!(py_date(&b)));
    }
    if let Some(g) = sex.as_deref().filter(|s| ["female", "male", "unknown"].contains(s)) {
        patient.insert("gender".into(), json!(g));
    }
    if deceased {
        patient.insert("deceasedBoolean".into(), json!(true));
    }
    if let Some(c) = country.as_deref().filter(|c| !c.is_empty()) {
        patient.insert("address".into(), json!([{"country": c}]));
    }
    w.line("fhir/Patient.ndjson", &Value::Object(patient))?;
    let patient_ref = json!({"reference": format!("Patient/{pid}")});
    let (fhir_tables, raw_tables) = tables(conn).await?;
    let mut cites: HashMap<String, Vec<String>> = HashMap::new();
    let mut counts = Map::new();
    for table in &fhir_tables {
        let cols = columns(conn, table).await?;
        let status = if cols.contains("status") {
            "status::text"
        } else {
            "null::text"
        };
        let src = if cols.contains("src_id") {
            "src_id::text"
        } else {
            "null::text"
        };
        let q = format!(
            "select id::text, fhir, {status}, {src} from {} where profile_id = $1 order by id",
            quote(table)
        );
        let mut rows = sqlx::query_as::<_, (String, Option<Value>, Option<String>, Option<String>)>(AssertSqlSafe(q))
            .bind(profile_id)
            .fetch(&mut *conn);
        while let Some((id, fhir, status, src_id)) = rows.try_next().await? {
            let mut res = match fhir {
                Some(Value::Object(m)) => m,
                _ => Map::new(),
            };
            let rtype = res
                .get("resourceType")
                .and_then(Value::as_str)
                .filter(|s| safe_resource_type(s))
                .unwrap_or("Basic")
                .to_string();
            res.insert("resourceType".into(), json!(rtype));
            if !res.contains_key("id") {
                res.insert("id".into(), json!(id));
            }
            let mut meta = match res.get("meta") {
                Some(Value::Object(m)) => m.clone(),
                _ => Map::new(),
            };
            if let Some(s) = status.filter(|s| !s.is_empty()) {
                let mut tags = match meta.get("tag") {
                    Some(Value::Array(a)) => a.clone(),
                    _ => Vec::new(),
                };
                tags.push(json!({"system": LIFECYCLE_TAG, "code": s}));
                meta.insert("tag".into(), Value::Array(tags));
            }
            if let Some(s) = src_id.filter(|s| !s.is_empty()) {
                meta.insert("source".into(), json!(format!("urn:ownhealth:src:{s}")));
                let rid = match &res["id"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                cites.entry(s).or_default().push(format!("{rtype}/{rid}"));
            }
            res.insert("meta".into(), Value::Object(meta));
            if !res.contains_key("subject") && !res.contains_key("patient") {
                let field = if PATIENT_FIELD.contains(&rtype.as_str()) {
                    "patient"
                } else {
                    "subject"
                };
                res.insert(field.into(), patient_ref.clone());
            }
            w.line(&format!("fhir/{rtype}.ndjson"), &Value::Object(res))?;
            bump(&mut counts, &rtype, 1);
        }
    }

    // documents and their files; a file the blob store lacks is listed in the manifest, never dropped quietly
    let mut missing = Vec::new();
    w.touch("fhir/DocumentReference.ndjson");
    type DocRow = (
        Uuid,
        Vec<u8>,
        Option<String>,
        String,
        i64,
        Option<String>,
        String,
        Option<DateTime<Utc>>,
        DateTime<Utc>,
    );
    let docs: Vec<DocRow> = sqlx::query_as(
        "select id, blob_sha256, filename, mime, size_bytes, doc_type, status, collected_at, created_at \
         from document where profile_id = $1 order by created_at, id",
    )
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    for (id, sha, filename, mime, size, doc_type, status, collected, created) in &docs {
        let sha = hex::encode(sha);
        let path = format!("files/{sha}/{}", safe_name(filename.as_deref()));
        let doc = json!({
            "resourceType": "DocumentReference",
            "id": id.to_string(),
            "status": if status == "active" { "current" } else { "entered-in-error" },
            "type": {"text": doc_type.as_deref().unwrap_or("other")},
            "subject": patient_ref,
            "date": py_isoformat_utc(&collected.unwrap_or(*created)),
            "content": [{"attachment": {"contentType": mime, "url": path, "title": filename, "size": size}}],
        });
        w.line("fhir/DocumentReference.ndjson", &doc)?;
        if !blocking(|| w.blob(&path, blobs, &sha))? {
            missing.push(json!({"path": path, "source_blob": sha}));
        }
    }
    counts.insert("DocumentReference".into(), json!(docs.len()));

    // provenance: one per src id of the profile
    w.touch("fhir/Provenance.ndjson");
    {
        let mut rows = sqlx::query(
            "select id, kind, actor_kind, document_id, page_no, bbox, import_ref, created_at from src \
             where profile_id = $1 order by id",
        )
        .bind(profile_id)
        .fetch(&mut *conn);
        while let Some(s) = rows.try_next().await? {
            let id: String = s.try_get("id")?;
            let kind: String = s.try_get("kind")?;
            let actor_kind: String = s.try_get("actor_kind")?;
            let document_id: Option<Uuid> = s.try_get("document_id")?;
            let page_no: Option<i32> = s.try_get("page_no")?;
            let bbox: Option<Vec<f32>> = s.try_get("bbox")?;
            let import_ref: Option<String> = s.try_get("import_ref")?;
            let created_at: DateTime<Utc> = s.try_get("created_at")?;
            let targets: Vec<Value> = match cites.get(&id) {
                Some(refs) if !refs.is_empty() => refs.iter().map(|r| json!({"reference": r})).collect(),
                _ => vec![patient_ref.clone()],
            };
            let mut entry = Map::new();
            entry.insert("resourceType".into(), json!("Provenance"));
            entry.insert("id".into(), json!(id.replace('_', "-")));
            entry.insert("target".into(), Value::Array(targets));
            entry.insert("recorded".into(), json!(py_isoformat_utc(&created_at)));
            entry.insert(
                "agent".into(),
                json!([{"type": {"text": actor_kind}, "who": {"display": kind}}]),
            );
            let mut ext = vec![json!({"url": "urn:ownhealth:src", "valueString": id})];
            let mut entity = None;
            if let Some(doc) = document_id {
                entity = Some(json!([{"role": "source", "what": {"reference": format!("DocumentReference/{doc}")}}]));
                ext.push(
                    json!({"url": "urn:ownhealth:location", "valueString": location(doc, page_no, bbox.as_deref())}),
                );
            }
            if let Some(r) = import_ref.filter(|r| !r.is_empty()) {
                ext.push(json!({"url": "urn:ownhealth:import-ref", "valueString": r}));
            }
            // Python's key order: "extension" is in the literal, "entity" is added after it
            entry.insert("extension".into(), Value::Array(ext));
            if let Some(e) = entity {
                entry.insert("entity".into(), e);
            }
            w.line("fhir/Provenance.ndjson", &Value::Object(entry))?;
            bump(&mut counts, "Provenance", 1);
        }
    }

    // genome files
    let genome: Vec<(Uuid, Vec<Option<String>>)> = sqlx::query_as(
        "select id, array[encode(facts_blob, 'hex'), encode(vcf_blob, 'hex'), encode(tbi_blob, 'hex'), \
         encode(coverage_blob, 'hex')] from genome_dataset where profile_id = $1 order by id",
    )
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    for (gid, shas) in genome {
        for ((_, name), sha) in GENOME_FILES.iter().zip(shas) {
            if let Some(sha) = sha {
                let path = format!("genome/{gid}/{name}");
                if !blocking(|| w.blob(&path, blobs, &sha))? {
                    missing.push(json!({"path": path, "source_blob": sha}));
                }
            }
        }
    }

    // everything else, as stored
    for table in &raw_tables {
        let path = format!("ownhealth/{table}.ndjson");
        w.touch(&path);
        let q = format!("select to_jsonb(t) from {} t where profile_id = $1", quote(table));
        let mut rows = sqlx::query_scalar::<_, Value>(AssertSqlSafe(q))
            .bind(profile_id)
            .fetch(&mut *conn);
        let mut n = 0u64;
        while let Some(row) = rows.try_next().await? {
            w.line(&path, &row)?;
            n += 1;
        }
        counts.insert(format!("ownhealth/{table}"), json!(n));
    }
    // label photos (migration 1101): the link has no profile_id, so it is reached through its record
    {
        let path = "ownhealth/medication_label.ndjson";
        w.touch(path);
        let mut rows = sqlx::query_scalar::<_, Value>(
            "select to_jsonb(l) from medication_label l join medication_statement m on m.id = l.medication_id \
             where m.profile_id = $1 order by l.medication_id, l.document_id",
        )
        .bind(profile_id)
        .fetch(&mut *conn);
        let mut n = 0u64;
        while let Some(row) = rows.try_next().await? {
            w.line(path, &row)?;
            n += 1;
        }
        counts.insert("ownhealth/medication_label".into(), json!(n));
    }
    blocking(|| w.close_lines())?;
    let versions: Vec<i32> = sqlx::query_scalar("select version from schema_migrations order by version")
        .fetch_all(&mut *conn)
        .await?;
    let manifest = json!({
        "format": FORMAT,
        "generated_at": py_isoformat_utc(&Utc::now()),
        "ownhealth_version": env!("CARGO_PKG_VERSION"),
        "schema_migrations": versions,
        "fhir_version": "4.0.1",
        "profile": {"id": pid.to_string(), "slug": slug, "display_name": display_name},
        "counts": counts,
        "files": w.files,
        "missing_blobs": missing,
    });
    blocking(|| w.finish(&manifest))?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_type_with_a_path_is_not_a_zip_entry_name() {
        assert!(safe_resource_type("Observation"));
        assert!(safe_resource_type("MedicationStatement"));
        for bad in ["", "../../x", "a/b", "a\\b", "x.ndjson", "Obs ervation"] {
            assert!(!safe_resource_type(bad), "{bad:?}");
        }
    }

    #[test]
    fn safe_name_matches_python() {
        assert_eq!(
            safe_name(Some("a/b/LabNorte 2026-09-14.pdf")),
            "LabNorte_2026-09-14.pdf"
        );
        assert_eq!(safe_name(Some("..hidden")), "hidden");
        assert_eq!(safe_name(None), "file");
        assert_eq!(safe_name(Some("análisis.pdf")), "an_lisis.pdf");
    }

    #[test]
    fn ensure_ascii_escapes_like_python() {
        assert_eq!(ensure_ascii("\"Lucía\" 😀"), "\"Luc\\u00eda\" \\ud83d\\ude00");
    }

    #[test]
    fn location_is_pythons_json_dumps() {
        let id = Uuid::nil();
        assert_eq!(
            location(id, Some(1), Some(&[56.0, 600.45, 1.0, 2.5])),
            format!("{{\"document_id\": \"{id}\", \"page\": 1, \"bbox\": [56.0, 600.45, 1.0, 2.5]}}")
        );
    }
}
