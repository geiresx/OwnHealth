//! Import of a genome-facts.json and its raw files (genomics.md 2, 5; `genome/ingest.py`).
//!
//! The facts file (kilobytes) is parsed into `genome_dataset`, `genome_coverage` and `genome_finding`.
//! The tabix-indexed VCF, gVCF and chip site list stay in the blob store, and the dataset points at
//! them only after their sha256 and their index's sha256 match the facts file. A file that fails any
//! check returns `Error::Invalid` (Python's `GenomeImportError`) and the caller's transaction rolls
//! back: nothing is half-imported.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;

use chrono::{DateTime, Utc};
use ownhealth_core::blobs::{BlobStore, sha256_hex_to_bytes};
use ownhealth_core::ids::short;
use ownhealth_core::ingest::ImportSummary;
use ownhealth_core::provenance::{SrcIn, SrcRef, ensure_src};
use ownhealth_core::{Error, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;

use crate::blocking;
use crate::schema::{ChipSites, chip_site_errors, validate_facts};
use crate::sensitive::{is_sensitive_gene, norm_chrom};
use crate::vcf::VcfFile;

/// role -> (data column, index column)
pub const RAW_ROLES: [(&str, &str, &str); 3] = [
    ("calls_vcf", "vcf_blob", "tbi_blob"),
    ("gvcf", "gvcf_blob", "gvcf_tbi_blob"),
    ("chip_sites", "sites_blob", "sites_tbi_blob"),
];
pub const FINGERPRINT_MIN_SITES: usize = 20;
pub const FINGERPRINT_MIN_CONCORDANCE: f64 = 0.95;
const UNKNOWN_ORDER: [&str; 3] = ["no_call", "not_covered", "not_on_array"];
const REF_ALLELES: [&str; 3] = ["*1", "*1A", "*1.001"];

/// Python's `GenomeImportError`: the file or its raw files cannot be imported, nothing is written.
fn import_error(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn genotype(v: &Value) -> Option<&Map<String, Value>> {
    v.get("genotype").and_then(Value::as_object).filter(|g| !g.is_empty())
}

/// One call_state for a finding: alt if any cited site is alt, else the first unknown state, else ref.
/// A diplotype without listed sites (CYP2D6 from reads) is ref only when both alleles are *1.
pub fn finding_call_state(fact: &Value) -> Option<&'static str> {
    let states: Vec<&str> = arr(fact, "variants")
        .iter()
        .filter_map(|v| genotype(v).and_then(|g| g.get("call_state")).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    if !states.is_empty() {
        if states.contains(&"alt") {
            return Some("alt");
        }
        if let Some(u) = UNKNOWN_ORDER.iter().find(|u| states.contains(u)) {
            return Some(u);
        }
        return Some("ref");
    }
    let diplotype = fact.get("value").and_then(|v| s(v, "diplotype")).map(str::trim);
    if let Some(d) = diplotype.filter(|d| !d.is_empty()) {
        let alleles: Vec<&str> = d.split(['/', '|']).map(str::trim).filter(|a| !a.is_empty()).collect();
        let all_ref = !alleles.is_empty() && alleles.iter().all(|a| REF_ALLELES.contains(a));
        return Some(if all_ref { "ref" } else { "alt" });
    }
    None
}

/// (sensitive, adult_onset, carrier). Sensitive covers the producer's `sensitive` and `adult_onset`
/// tags and our own gene list, because the consent switch covers both (genomics.md 7). PRS counts as
/// adult-onset so the minor rule withholds it.
pub fn sensitivity_flags(fact: &Value) -> (bool, bool, bool) {
    let level = fact.get("sensitivity").and_then(|x| s(x, "level")).unwrap_or("normal");
    let category = s(fact, "category").unwrap_or_default();
    let adult_onset = level == "adult_onset" || category == "prs";
    let sensitive = level == "sensitive" || level == "adult_onset" || is_sensitive_gene(s(fact, "gene"));
    (sensitive, adult_onset, category == "carrier")
}

fn rehash(blobs: &BlobStore, sha: &str) -> Result<String> {
    let mut file = blobs.open(sha)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn fingerprint_concordance(a: &str, b: &str) -> Option<(f64, usize)> {
    let pairs: Vec<(char, char)> = a
        .chars()
        .zip(b.chars())
        .filter(|(x, y)| *x != '.' && *y != '.')
        .collect();
    if pairs.len() < FINGERPRINT_MIN_SITES {
        return None;
    }
    let same = pairs.iter().filter(|(x, y)| x == y).count();
    Some((same as f64 / pairs.len() as f64, pairs.len()))
}

/// Python's `f"{x:.0%}"`.
fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

/// A raw file may be linked when it came with this upload or is already this profile's: the facts
/// file names raw files by sha256, and a hash on its own grants nothing.
async fn owned(conn: &mut PgConnection, profile_id: Uuid, sha: &str, attachments: &[(String, String)]) -> Result<bool> {
    if attachments.iter().any(|(_, a)| a == sha) {
        return Ok(true);
    }
    let found: Option<i32> = sqlx::query_scalar(
        "select 1 from genome_dataset where profile_id = $1 and $2 in \
         (vcf_blob, tbi_blob, gvcf_blob, gvcf_tbi_blob, sites_blob, sites_tbi_blob) limit 1",
    )
    .bind(profile_id)
    .bind(sha256_hex_to_bytes(sha)?)
    .fetch_optional(conn)
    .await?;
    Ok(found.is_some())
}

/// Column -> blob sha (bytes) for every raw file the facts list, we hold and this profile may use
/// (uploaded with it, or already its own), after a sha256 check; plus the files that are missing.
async fn raw_files(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    doc: &Value,
    attachments: &[(String, String)],
    profile_id: Uuid,
) -> Result<(HashMap<&'static str, Vec<u8>>, Vec<String>)> {
    let files = arr(doc, "files");
    let mut listed: Vec<&str> = Vec::new();
    for f in files {
        listed.extend(s(f, "sha256"));
        listed.extend(s(f, "index_sha256").filter(|i| !i.is_empty()));
    }
    for (name, sha) in attachments {
        if !listed.contains(&sha.as_str()) {
            let head: String = sha.chars().take(12).collect();
            return Err(import_error(format!(
                "{name}: its sha256 ({head}…) matches no file listed in the facts file, so it is not a file this \
                 genome-facts.json describes. Nothing was imported."
            )));
        }
    }
    let mut columns: HashMap<&'static str, Vec<u8>> = HashMap::new();
    let mut missing = Vec::new();
    for f in files {
        let role = s(f, "role").unwrap_or_default();
        let Some((_, data_col, index_col)) = RAW_ROLES.iter().find(|(r, _, _)| *r == role) else {
            continue;
        };
        if columns.contains_key(data_col) {
            return Err(import_error(format!(
                "the facts file lists two {role} files; v1 takes one per role"
            )));
        }
        let (sha, idx) = (
            s(f, "sha256").unwrap_or_default(),
            s(f, "index_sha256").unwrap_or_default(),
        );
        let path = s(f, "path").unwrap_or_default();
        let mut usable = true;
        for h in [sha, idx] {
            if !(blobs.exists(h) && owned(&mut *conn, profile_id, h, attachments).await?) {
                usable = false;
                break;
            }
        }
        if !usable {
            missing.push(format!("{role} {path}"));
            continue;
        }
        let index_label = s(f, "index_path").filter(|p| !p.is_empty()).unwrap_or("index");
        let (store, files) = (
            blobs.clone(),
            [
                (sha.to_string(), path.to_string()),
                (idx.to_string(), index_label.to_string()),
            ],
        );
        blocking(move || check_raw_file(&store, &files)).await?;
        columns.insert(data_col, sha256_hex_to_bytes(sha)?);
        columns.insert(index_col, sha256_hex_to_bytes(idx)?);
    }
    Ok((columns, missing))
}

/// The stored bytes of a raw file and its index match their sha256, and they open as a bgzipped VCF
/// with a tabix index. `files` is [(data sha, path), (index sha, index path)].
fn check_raw_file(blobs: &BlobStore, files: &[(String, String); 2]) -> Result<()> {
    for (want, label) in files {
        if rehash(blobs, want)? != *want {
            let head: String = want.chars().take(12).collect();
            return Err(import_error(format!(
                "{label}: stored bytes do not match sha256 {head}…"
            )));
        }
    }
    let [(sha, path), (idx, _)] = files;
    let opened = VcfFile::open(&blobs.path(sha)?, &blobs.path(idx)?).and_then(|mut vf| vf.check_header());
    if let Err(e) = opened {
        return Err(import_error(format!(
            "{path}: not a bgzipped VCF with a tabix index ({e})"
        )));
    }
    Ok(())
}

/// Rule 7's site-list check, with the chip_sites file opened when it is on hand.
fn chip_errors(blobs: &BlobStore, doc: &Value, has_sites: bool) -> Result<Vec<String>> {
    let file = arr(doc, "files").iter().find(|f| s(f, "role") == Some("chip_sites"));
    let (Some(f), true) = (file, has_sites) else {
        return Ok(chip_site_errors(doc, None));
    };
    let vf = VcfFile::open(
        &blobs.path(s(f, "sha256").unwrap_or_default())?,
        &blobs.path(s(f, "index_sha256").unwrap_or_default())?,
    )?;
    let vf = RefCell::new(vf);
    let probed = |chrom: &str, pos: i64| -> bool {
        let c = norm_chrom(chrom);
        let Ok(mut vf) = vf.try_borrow_mut() else {
            return false;
        };
        for name in [c.clone(), c[3..].to_string()] {
            match vf.fetch_contig(&name, pos, pos) {
                Ok(Some(records)) => return records.iter().any(|r| r.pos == pos),
                Ok(None) | Err(_) => continue,
            }
        }
        false
    };
    Ok(chip_site_errors(doc, Some(&probed)))
}

async fn finding_src_id(conn: &mut PgConnection, fact_id: &str) -> Result<String> {
    let mut sid = format!("src_g_{}", fact_id.get(2..12).unwrap_or_default());
    let taken: Option<i32> = sqlx::query_scalar("select 1 from src where id = $1")
        .bind(&sid)
        .fetch_optional(conn)
        .await?;
    if taken.is_some() {
        sid = format!("{sid}_{}", short(4));
    }
    Ok(sid)
}

fn invalid(filename: &str, errors: &[String]) -> Error {
    import_error(format!(
        "{filename} is not a valid genome-facts file:\n- {}",
        errors.join("\n- ")
    ))
}

/// Python `a or b` over two JSON values.
fn or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    a.filter(|v| crate::findings::truthy(v)).or(b)
}

/// A JSON number as Python handed it to psycopg: integers exactly, floats as float8.
fn number_parts(v: Option<&Value>) -> (Option<i64>, Option<f64>) {
    match v {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) => (Some(i), None),
            None => (None, n.as_f64()),
        },
        _ => (None, None),
    }
}

/// Import the genome-facts.json stored as `document_id` (its profile must be set).
///
/// `attachments` maps uploaded file names to their blob sha256 (the .vcf.gz and .tbi uploaded with it).
/// Raw files already linked to this profile under the sha256 the facts file names are used too.
/// Re-importing the same bytes is a no-op; a new file for the same sample supersedes the old one.
/// `imported_at` backdates the import (demo seed); default now.
pub async fn import(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    attachments: &[(String, String)],
    actor_user: Option<Uuid>,
    imported_at: Option<DateTime<Utc>>,
) -> Result<ImportSummary> {
    let row: Option<(Uuid, Option<Uuid>, String, Option<String>)> = sqlx::query_as(
        "select household_id, profile_id, encode(blob_sha256, 'hex') as sha, filename from document where id = $1",
    )
    .bind(document_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((household_id, profile_id, sha, filename)) = row else {
        return Err(import_error(format!("document {document_id} not found")));
    };
    let Some(profile_id) = profile_id else {
        return Err(import_error("assign the genome-facts file to a profile first"));
    };
    let filename = filename
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| "genome-facts.json".into());
    let bytes = blobs.get_bytes(&sha)?;
    let doc: Value = match std::str::from_utf8(&bytes)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(t).map_err(|e| e.to_string()))
    {
        Ok(doc) => doc,
        Err(e) => return Err(import_error(format!("{filename}: not valid JSON ({e})"))),
    };
    let errors = validate_facts(&doc, ChipSites::Defer);
    if !errors.is_empty() {
        return Err(invalid(&filename, &errors));
    }
    let facts = arr(&doc, "facts");
    let mut summary = ImportSummary::new(document_id.to_string());
    let sha_bytes = sha256_hex_to_bytes(&sha)?;
    let same: Option<Uuid> = sqlx::query_scalar("select profile_id from genome_dataset where facts_sha256 = $1")
        .bind(&sha_bytes)
        .fetch_optional(&mut *conn)
        .await?;
    if let Some(owner) = same {
        if owner != profile_id {
            return Err(import_error(format!(
                "{filename}: this exact file is already imported for another profile"
            )));
        }
        summary.skipped = facts.len() as i64;
        summary.reasons.insert("duplicate_facts_file".into(), 1);
        return Ok(summary);
    }

    let subject = doc.get("subject").cloned().unwrap_or_else(|| json!({}));
    let sample_id = s(&subject, "sample_id").unwrap_or_default().to_string();
    let elsewhere: Option<String> = sqlx::query_scalar(
        "select p.slug from genome_dataset d join profile p on p.id = d.profile_id where d.sample_id = $1 \
         and d.profile_id <> $2 and d.status = 'current' and p.household_id = $3",
    )
    .bind(&sample_id)
    .bind(profile_id)
    .bind(household_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(slug) = elsewhere.filter(|s| !s.is_empty()) {
        return Err(import_error(format!(
            "sample {sample_id} is already imported for profile '{slug}'; nothing was imported"
        )));
    }

    let fingerprint = subject.get("fingerprint").cloned().unwrap_or(Value::Null);
    if let Some(fp) = s(&fingerprint, "genotypes").filter(|f| !f.is_empty()) {
        let others: Vec<(String, Option<String>)> = sqlx::query_as(
            "select sample_id, subject->'fingerprint'->>'genotypes' as g from genome_dataset where profile_id = $1 \
             and status = 'current' and subject->'fingerprint'->>'panel' = $2",
        )
        .bind(profile_id)
        .bind(s(&fingerprint, "panel"))
        .fetch_all(&mut *conn)
        .await?;
        for (other, g) in others {
            if let Some((share, n)) = fingerprint_concordance(fp, g.as_deref().unwrap_or_default())
                && share < FINGERPRINT_MIN_CONCORDANCE
            {
                return Err(import_error(format!(
                    "fingerprint mismatch: this file agrees with source {other} at {} of {n} sites (at least {} \
                     expected for one person). It looks like a different person or a sample swap; nothing was \
                     imported.",
                    pct(share),
                    pct(FINGERPRINT_MIN_CONCORDANCE)
                )));
            }
        }
    }

    let (raw, missing) = raw_files(&mut *conn, blobs, &doc, attachments, profile_id).await?;
    if !missing.is_empty() {
        summary.reasons.insert("raw_file_missing".into(), missing.len() as i64);
    }
    let errors = {
        let (store, doc, has_sites) = (blobs.clone(), doc.clone(), raw.contains_key("sites_blob"));
        blocking(move || chip_errors(&store, &doc, has_sites)).await?
    };
    if !errors.is_empty() {
        return Err(invalid(&filename, &errors));
    }

    let prov = doc.get("provenance").cloned().unwrap_or_else(|| json!({}));
    let assay = doc.get("assay").cloned().unwrap_or_else(|| json!({}));
    let assay_type = s(&assay, "type").unwrap_or_default();
    let old: Vec<Uuid> = sqlx::query_scalar(
        "select id from genome_dataset where profile_id = $1 and sample_id = $2 and status = 'current'",
    )
    .bind(profile_id)
    .bind(&sample_id)
    .fetch_all(&mut *conn)
    .await?;
    for old_id in &old {
        sqlx::query(
            "update genome_dataset set status = 'superseded', status_reason = 'a newer facts file for this sample', \
             superseded_at = now() where id = $1",
        )
        .bind(old_id)
        .execute(&mut *conn)
        .await?;
    }
    let mut previous: HashMap<String, Uuid> = HashMap::new();
    if !old.is_empty() {
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("select id, fact_id from genome_finding where dataset_id = any($1) and status = 'current'")
                .bind(&old)
                .fetch_all(&mut *conn)
                .await?;
        for (id, fid) in rows {
            previous.insert(fid, id);
        }
        sqlx::query(
            "update genome_finding set status = 'superseded', status_reason = 're-run', superseded_at = now() \
             where dataset_id = any($1) and status = 'current'",
        )
        .bind(&old)
        .execute(&mut *conn)
        .await?;
    }

    let pipeline = s(&prov, "pipeline").unwrap_or_default();
    let version = s(&prov, "version").unwrap_or_default();
    let generated: String = s(&prov, "generated_utc").unwrap_or_default().chars().take(10).collect();
    let actor_kind = if actor_user.is_some() { "user" } else { "system" };
    let new_src = |import_ref: String, label: String, id: String| SrcIn {
        profile_id: Some(profile_id),
        document_id: Some(document_id),
        import_ref: Some(import_ref),
        label: Some(label),
        actor_kind: actor_kind.into(),
        actor_id: actor_user,
        id: Some(id),
        ..SrcIn::new("genome_facts")
    };
    let dataset_src = ensure_src(
        &mut *conn,
        &SrcRef::New(new_src(
            filename.clone(),
            format!("{pipeline} {version} · {assay_type} · {generated}"),
            format!("src_gd_{}", short(8)),
        )),
        None,
    )
    .await?;
    let db_versions = prov.get("db_versions").cloned().unwrap_or_else(|| json!({}));
    let producer: Map<String, Value> = prov
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| *k != "db_versions")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let qc = doc
        .get("qc")
        .filter(|q| crate::findings::truthy(q))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let col = |c: &str| raw.get(c).cloned();
    let dataset_id: Uuid = sqlx::query_scalar(
        "insert into genome_dataset (profile_id, sample_id, assay, facts_schema, facts_sha256, facts_blob, \
         vcf_blob, tbi_blob, gvcf_blob, gvcf_tbi_blob, sites_blob, sites_tbi_blob, build, owngenome_version, \
         db_versions, src_id, document_id, assay_detail, producer, subject, qc, imported_at) \
         values ($1, $2, $3, $4, $5, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, \
         coalesce(cast($21 as timestamptz), now())) returning id",
    )
    .bind(profile_id)
    .bind(&sample_id)
    .bind(assay_type)
    .bind(s(&doc, "schema"))
    .bind(&sha_bytes)
    .bind(col("vcf_blob"))
    .bind(col("tbi_blob"))
    .bind(col("gvcf_blob"))
    .bind(col("gvcf_tbi_blob"))
    .bind(col("sites_blob"))
    .bind(col("sites_tbi_blob"))
    .bind(s(&assay, "build"))
    .bind(format!("{pipeline} {version}"))
    .bind(Json(&db_versions))
    .bind(&dataset_src)
    .bind(document_id)
    .bind(Json(&assay))
    .bind(Json(&producer))
    .bind(Json(&subject))
    .bind(Json(&qc))
    .bind(imported_at)
    .fetch_one(&mut *conn)
    .await?;

    for c in arr(&doc, "coverage") {
        sqlx::query(
            "insert into genome_coverage (dataset_id, scope, target, panel, state, reason, fraction_callable, note) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) on conflict (dataset_id, scope, target) do nothing",
        )
        .bind(dataset_id)
        .bind(s(c, "scope"))
        .bind(s(c, "id"))
        .bind(s(c, "panel"))
        .bind(s(c, "state"))
        .bind(s(c, "reason"))
        .bind(c.get("fraction_callable").and_then(Value::as_f64))
        .bind(s(c, "note"))
        .execute(&mut *conn)
        .await?;
    }

    let mut raised = 0;
    let empty = json!({});
    for fact in facts {
        let (sensitive, adult_onset, carrier) = sensitivity_flags(fact);
        let level = fact.get("sensitivity").and_then(|x| s(x, "level")).unwrap_or("normal");
        if sensitive && level == "normal" {
            raised += 1;
        }
        let first = arr(fact, "variants").first().unwrap_or(&empty);
        let value = fact
            .get("value")
            .filter(|v| crate::findings::truthy(v))
            .unwrap_or(&empty);
        let score = value.get("activity_score").filter(|v| v.is_number());
        let (score_int, score_float) = number_parts(score);
        let fid = s(fact, "id").unwrap_or_default();
        let gene = s(fact, "gene");
        let category = s(fact, "category").unwrap_or_default();
        let src_id = finding_src_id(&mut *conn, fid).await?;
        let src = ensure_src(
            &mut *conn,
            &SrcRef::New(new_src(
                format!("{filename}#{fid}"),
                format!(
                    "{pipeline} {version} · {}",
                    gene.filter(|g| !g.is_empty()).unwrap_or(category)
                ),
                src_id,
            )),
            None,
        )
        .await?;
        sqlx::query(
            "insert into genome_finding (dataset_id, profile_id, fact_id, category, gene, rsid, hgvs, vrs_id, \
             clinvar_variation_id, diplotype, phenotype, activity_score, zygosity, evidence, confidence, \
             call_state, sensitive, adult_onset, carrier, src_id, supersedes, fact, recorded_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, coalesce(cast($12 as numeric), \
             cast(cast($13 as float8) as numeric)), $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, \
             coalesce(cast($24 as timestamptz), now()))",
        )
        .bind(dataset_id)
        .bind(profile_id)
        .bind(fid)
        .bind(category)
        .bind(gene)
        .bind(s(first, "rsid"))
        .bind(or(first.get("hgvs_c"), first.get("hgvs_p")).and_then(Value::as_str))
        .bind(s(first, "vrs_id"))
        .bind(first.get("clinvar_variation_id").and_then(Value::as_i64))
        .bind(or(value.get("diplotype"), value.get("haplotype")).and_then(Value::as_str))
        .bind(s(value, "phenotype"))
        .bind(score_int)
        .bind(score_float)
        .bind(genotype(first).and_then(|g| g.get("zygosity")).and_then(Value::as_str))
        .bind(fact.get("evidence").and_then(|e| s(e, "level")))
        .bind(fact.get("confidence").and_then(|c| s(c, "level")))
        .bind(finding_call_state(fact))
        .bind(sensitive)
        .bind(adult_onset)
        .bind(carrier)
        .bind(&src)
        .bind(previous.get(fid))
        .bind(Json(fact))
        .bind(imported_at)
        .execute(&mut *conn)
        .await?;
    }
    summary.accepted = facts.len() as i64;
    if raised > 0 {
        summary.reasons.insert("sensitivity_raised_by_gene_list".into(), raised);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_state_of_a_finding() {
        let alt = json!({"variants": [{"genotype": {"call_state": "ref"}}, {"genotype": {"call_state": "alt"}}]});
        assert_eq!(finding_call_state(&alt), Some("alt"));
        let unknown = json!({"variants": [{"genotype": {"call_state": "not_on_array"}}, {"genotype": {"call_state": "no_call"}}]});
        assert_eq!(finding_call_state(&unknown), Some("no_call"));
        assert_eq!(
            finding_call_state(&json!({"value": {"diplotype": "*1/*1"}})),
            Some("ref")
        );
        assert_eq!(
            finding_call_state(&json!({"value": {"diplotype": "*1/*41"}})),
            Some("alt")
        );
        assert_eq!(finding_call_state(&json!({"value": {}})), None);
    }

    #[test]
    fn fingerprint_share() {
        let a = "0".repeat(37);
        assert_eq!(fingerprint_concordance(&a, &a), Some((1.0, 37)));
        assert_eq!(fingerprint_concordance(&a, "0000"), None);
        assert_eq!(pct(0.5135), "51%");
        assert_eq!(pct(0.95), "95%");
    }
}
