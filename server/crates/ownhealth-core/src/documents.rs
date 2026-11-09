//! Documents (one row per stored file per profile) and the upload dispatcher (`core/documents.py`).
//!
//! The same bytes uploaded twice for one profile are one document with two `document_source`
//! rows. `dispatch_upload` sniffs a stored blob and enqueues the import job of the lane that
//! handles it.

use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::blobs::{BlobStore, sha256_hex_to_bytes};
use crate::error::{Error, Result};
use crate::ids::token_hex;
use crate::jobs::{EnqueueOptions, enqueue};

/// pdf | image | apple_health_zip | fhir_json | fhir_ndjson | genome_facts | vcf | tbi | gadgetbridge_db | unknown
pub type UploadKind = &'static str;

/// Job kind per upload kind. Handlers: import.pdf (lane B), import.fhir_bundle and
/// import.apple_health (lane C), import.genome_facts (lane D).
pub fn job_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "pdf" => "import.pdf",
        "apple_health_zip" => "import.apple_health",
        "fhir_json" | "fhir_ndjson" => "import.fhir_bundle",
        "genome_facts" => "import.genome_facts",
        "gadgetbridge_db" => "import.gadgetbridge",
        _ => return None,
    })
}

pub fn mime(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "pdf" => "application/pdf",
        "fhir_json" => "application/fhir+json",
        "fhir_ndjson" => "application/fhir+ndjson",
        "apple_health_zip" => "application/zip",
        "genome_facts" => "application/json",
        "vcf" => "application/gzip",
        "tbi" => "application/octet-stream",
        "gadgetbridge_db" => "application/vnd.sqlite3",
        _ => return None,
    })
}

fn zip_has_export_xml(path: &Path) -> Option<bool> {
    let file = std::fs::File::open(path).ok()?;
    let archive = zip::ZipArchive::new(file).ok()?;
    Some(archive.file_names().any(|n| n.is_ok_and(|n| n.ends_with("export.xml"))))
}

/// Classify an upload from its name and first bytes (and, for zips, its member list).
pub fn sniff(filename: &str, head: &[u8], blob_path: Option<&Path>) -> UploadKind {
    let name = filename.to_lowercase();
    if head.starts_with(b"%PDF-") {
        return "pdf";
    }
    if head.starts_with(b"\x89PNG")
        || head.starts_with(b"\xff\xd8\xff")
        || name.ends_with(".heic")
        || name.ends_with(".heif")
    {
        return "image";
    }
    if head.starts_with(b"SQLite format 3\0") {
        return "gadgetbridge_db";
    }
    if head.starts_with(b"PK\x03\x04") {
        // Truncated or corrupt: refused like any unknown file.
        return match blob_path.and_then(zip_has_export_xml) {
            Some(true) => "apple_health_zip",
            _ => "unknown",
        };
    }
    if name.ends_with(".tbi") {
        return "tbi";
    }
    if name.ends_with(".vcf.gz") || name.ends_with(".vcf.bgz") || head.starts_with(b"\x1f\x8b") {
        return "vcf";
    }
    // Python's bytes.lstrip() strips ASCII whitespace.
    let start = head
        .iter()
        .position(|b| !b" \t\n\r\x0b\x0c".contains(b))
        .unwrap_or(head.len());
    if head[start..].starts_with(b"{") {
        let contains = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
        if contains(b"ownhealth.genome-facts/") {
            return "genome_facts";
        }
        if name.ends_with(".ndjson") {
            return "fhir_ndjson";
        }
        if contains(b"\"resourceType\"") {
            return "fhir_json";
        }
    }
    "unknown"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentRef {
    pub id: Uuid,
    pub created: bool,
    pub reactivated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewDocument<'a> {
    pub household_id: Uuid,
    pub profile_id: Option<Uuid>,
    pub blob_sha256: &'a str,
    pub mime: &'a str,
    pub size_bytes: i64,
    pub filename: Option<&'a str>,
    /// upload | email | folder | api | ...
    pub channel: &'a str,
    pub source_ref: Option<&'a str>,
    pub pages: Option<i32>,
    pub doc_type: Option<&'a str>,
    /// The inbox it came through (`source.id`), on the document and its `document_source` row.
    pub source_id: Option<Uuid>,
    /// The original this is an unlocked copy of (a password PDF).
    pub derived_from: Option<Uuid>,
}

impl<'a> NewDocument<'a> {
    pub fn upload(
        household_id: Uuid,
        profile_id: Option<Uuid>,
        blob_sha256: &'a str,
        mime: &'a str,
        size_bytes: i64,
    ) -> Self {
        NewDocument {
            household_id,
            profile_id,
            blob_sha256,
            mime,
            size_bytes,
            filename: None,
            channel: "upload",
            source_ref: None,
            pages: None,
            doc_type: None,
            source_id: None,
            derived_from: None,
        }
    }
}

/// Python `(filename or "")[:255] or None`: at most 255 characters, empty means none.
fn short_filename(f: Option<&str>) -> Option<String> {
    let s: String = f.unwrap_or("").chars().take(255).collect();
    (!s.is_empty()).then_some(s)
}

/// Insert a document or return the existing one for the same profile and bytes.
pub async fn create_document(conn: &mut PgConnection, d: &NewDocument<'_>) -> Result<DocumentRef> {
    let sha = sha256_hex_to_bytes(d.blob_sha256)?;
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "select id, status from document where coalesce(profile_id, household_id) = coalesce($1, $2) and blob_sha256 = $3",
    )
    .bind(d.profile_id)
    .bind(d.household_id)
    .bind(&sha)
    .fetch_optional(&mut *conn)
    .await?;
    // The same bytes uploaded again after a retraction bring the document back; its import runs
    // again (the retraction cleared the import flags).
    let reactivated = row.as_ref().is_some_and(|(_, s)| s == "retracted");
    if reactivated && let Some((id, _)) = &row {
        sqlx::query("update document set status = 'active', status_reason = null where id = $1")
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    let (id, created) = match row {
        Some((id, _)) => (id, false),
        None => {
            // A concurrent upload of the same bytes can insert between the select and here: the
            // conflict then yields no id and the row that won is the duplicate.
            let id: Option<Uuid> = sqlx::query_scalar(
                "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, pages, doc_type, \
                 source_id, derived_from) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 on conflict ((coalesce(profile_id, household_id)), blob_sha256) do nothing returning id",
            )
            .bind(d.household_id)
            .bind(d.profile_id)
            .bind(&sha)
            .bind(short_filename(d.filename))
            .bind(d.mime)
            .bind(d.size_bytes)
            .bind(d.pages)
            .bind(d.doc_type)
            .bind(d.source_id)
            .bind(d.derived_from)
            .fetch_optional(&mut *conn)
            .await?;
            match id {
                Some(id) => (id, true),
                None => {
                    let id: Uuid = sqlx::query_scalar(
                        "select id from document where coalesce(profile_id, household_id) = coalesce($1, $2) and blob_sha256 = $3",
                    )
                    .bind(d.profile_id)
                    .bind(d.household_id)
                    .bind(&sha)
                    .fetch_one(&mut *conn)
                    .await?;
                    (id, false)
                }
            }
        }
    };
    sqlx::query("insert into document_source (document_id, channel, source_ref, source_id) values ($1, $2, $3, $4)")
        .bind(id)
        .bind(d.channel)
        .bind(d.source_ref)
        .bind(d.source_id)
        .execute(conn)
        .await?;
    Ok(DocumentRef {
        id,
        created,
        reactivated,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub filename: String,
    pub sha256: String,
    pub size: i64,
}

fn job_id_json(id: Option<i64>) -> Value {
    id.map_or(Value::Null, |i| json!(i))
}

/// Create documents and enqueue import jobs for files already in the blob store.
///
/// Returns `{"items": [{filename, kind, sha256, document_id?, duplicate?, job_id?, attached_to?}]}`
/// in the key order Python wrote. A genome-facts file takes the .vcf.gz and .tbi uploaded with
/// it; those alone are refused (`Error::Upload`).
pub async fn dispatch_upload(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    household_id: Uuid,
    profile_id: Uuid,
    files: &[StoredFile],
    _actor_user: Option<Uuid>,
) -> Result<Value> {
    let mut kinds: Vec<(&StoredFile, UploadKind)> = Vec::new();
    for f in files {
        let path = blobs.path(&f.sha256)?;
        let mut head = Vec::with_capacity(4096);
        std::fs::File::open(&path)?.take(4096).read_to_end(&mut head)?;
        kinds.push((f, sniff(&f.filename, &head, Some(&path))));
    }
    let has_facts = kinds.iter().any(|(_, k)| *k == "genome_facts");
    let mut items = Vec::new();
    for (f, kind) in &kinds {
        let mut item = Map::new();
        item.insert("filename".into(), json!(f.filename));
        item.insert("kind".into(), json!(kind));
        item.insert("sha256".into(), json!(f.sha256));
        match *kind {
            "unknown" => {
                return Err(Error::Upload(format!(
                    "{}: not a PDF, image, Apple Health export, FHIR bundle or genome facts file",
                    f.filename
                )));
            }
            "vcf" | "tbi" => {
                if !has_facts {
                    return Err(Error::Upload(format!(
                        "{}: upload it together with its genome-facts.json",
                        f.filename
                    )));
                }
                item.insert("attached_to".into(), json!("genome_facts"));
            }
            "pdf" | "image" => {
                let m = mime(kind).unwrap_or(if f.filename.to_lowercase().ends_with(".png") {
                    "image/png"
                } else {
                    "image/jpeg"
                });
                let r = create_document(
                    &mut *conn,
                    &NewDocument {
                        filename: Some(&f.filename),
                        ..NewDocument::upload(household_id, Some(profile_id), &f.sha256, m, f.size)
                    },
                )
                .await?;
                item.insert("document_id".into(), json!(r.id.to_string()));
                item.insert("duplicate".into(), json!(!(r.created || r.reactivated)));
                // Images too: the PDF handler files them as pending_document items.
                if r.created || r.reactivated {
                    let mut payload = json!({
                        "document_id": r.id.to_string(),
                        "profile_id": profile_id.to_string(),
                        "household_id": household_id.to_string(),
                    });
                    let mut key = format!("import.pdf:{}", r.id);
                    if r.reactivated {
                        // The earlier extraction run is done; import it again.
                        payload["force"] = json!(true);
                        key = format!("{key}:reupload:{}", token_hex(4));
                    }
                    let id = enqueue(&mut *conn, "import.pdf", &payload, EnqueueOptions::dedupe(&key)).await?;
                    item.insert("job_id".into(), job_id_json(id));
                }
            }
            _ => {
                let job = job_kind(kind).ok_or_else(|| Error::Upload(format!("{}: no importer", f.filename)))?;
                let mut payload = json!({
                    "profile_id": profile_id.to_string(),
                    "household_id": household_id.to_string(),
                    "blob_sha256": f.sha256,
                    "filename": f.filename,
                });
                if *kind == "fhir_ndjson" {
                    payload["format"] = json!("ndjson");
                }
                if *kind == "genome_facts" {
                    let attached: Map<String, Value> = kinds
                        .iter()
                        .filter(|(_, k)| matches!(*k, "vcf" | "tbi"))
                        .map(|(g, _)| (g.filename.clone(), json!(g.sha256)))
                        .collect();
                    payload["files"] = Value::Object(attached);
                }
                let key = format!("{job}:{profile_id}:{}", f.sha256);
                let id = enqueue(&mut *conn, job, &payload, EnqueueOptions::dedupe(&key)).await?;
                item.insert("job_id".into(), job_id_json(id));
            }
        }
        items.push(Value::Object(item));
    }
    Ok(json!({ "items": items }))
}

const SENSITIVE_FACT_TABLES: [&str; 7] = [
    "condition",
    "medication_statement",
    "allergy",
    "immunization",
    "procedure",
    "encounter",
    "note",
];

/// The rule of `document_is_sensitive` as a SQL boolean over `doc` (a bind like `$1` or a column
/// like `d.id`), for queries that must leave sensitive documents out.
pub fn sensitive_document_sql(doc: &str) -> String {
    let via_src = SENSITIVE_FACT_TABLES
        .iter()
        .map(|t| {
            format!(
                "exists (select 1 from {t} f join src s on s.id = f.src_id where s.document_id = {doc} \
                 and cardinality(f.sensitivity) > 0)"
            )
        })
        .collect::<Vec<_>>()
        .join(" or ");
    format!(
        "(exists (select 1 from observation o left join src s on s.id = o.src_id \
         where (o.document_id = {doc} or s.document_id = {doc}) and cardinality(o.sensitivity) > 0) \
         or exists (select 1 from review_item r where r.document_id = {doc} \
         and jsonb_typeof(r.payload->'observation'->'sensitivity') = 'array' \
         and jsonb_array_length(r.payload->'observation'->'sensitivity') > 0) \
         or {via_src})"
    )
}

/// True when any value read from this document is sensitive (mental health, reproductive, sexual
/// health), whatever its status: the original file and its page text still hold it. Held review
/// rows count too. Without read:sensitive the file, the page renders and the words stay closed.
pub async fn document_is_sensitive(conn: &mut PgConnection, document_id: Uuid) -> Result<bool> {
    let sql = format!("select {}", sensitive_document_sql("$1::uuid"));
    let v: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(document_id)
        .fetch_one(conn)
        .await?;
    Ok(v.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_matches_python() {
        assert_eq!(sniff("a.pdf", b"%PDF-1.7", None), "pdf");
        assert_eq!(sniff("x", b"\x89PNG\r\n", None), "image");
        assert_eq!(sniff("x.HEIC", b"....", None), "image");
        assert_eq!(sniff("x.zip", b"PK\x03\x04", None), "unknown");
        assert_eq!(sniff("x.vcf.gz", b"\x1f\x8b", None), "vcf");
        assert_eq!(sniff("x.vcf.gz.tbi", b"\x1f\x8b", None), "tbi");
        assert_eq!(
            sniff("f.json", b"  {\"schema\": \"ownhealth.genome-facts/1\"}", None),
            "genome_facts"
        );
        assert_eq!(
            sniff("b.ndjson", b"{\"resourceType\":\"Observation\"}", None),
            "fhir_ndjson"
        );
        assert_eq!(sniff("b.json", b"\n{\"resourceType\":\"Bundle\"}", None), "fhir_json");
        assert_eq!(sniff("b.json", b"{\"a\":1}", None), "unknown");
        assert_eq!(sniff("b.txt", b"hello", None), "unknown");
    }

    #[test]
    fn sniff_reads_the_zip_member_list() -> anyhow::Result<()> {
        use std::io::Write;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("export.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&path)?);
        z.start_file(
            "apple_health_export/export.xml",
            zip::write::SimpleFileOptions::default(),
        )?;
        z.write_all(b"<HealthData/>")?;
        z.finish()?;
        assert_eq!(sniff("export.zip", b"PK\x03\x04", Some(&path)), "apple_health_zip");
        let bad = dir.path().join("bad.zip");
        std::fs::write(&bad, b"PK\x03\x04truncated")?;
        assert_eq!(sniff("bad.zip", b"PK\x03\x04", Some(&bad)), "unknown");
        Ok(())
    }

    #[test]
    fn filenames_are_cut_at_255_characters() {
        assert_eq!(short_filename(Some("")), None);
        assert_eq!(short_filename(None), None);
        assert_eq!(
            short_filename(Some(&"é".repeat(300))).map(|s| s.chars().count()),
            Some(255)
        );
    }
}
