//! Three-level dedup (ingestion.md 4, ROADMAP v0.2), plus the per-source ledger in front of it.
//!
//! 0. **Source item**: the source's own id for an item (Message-ID, Paperless id, folder path + size
//!    + mtime) was seen already: nothing is stored again ([`seen_item`], [`record_item`]).
//! 1. **File hash**: the same bytes for the same profile are one document with one more
//!    `document_source` row (`documents::create_document` does it; [`existing_document`] asks).
//! 2. **Document fingerprint**: per page a 64-bit pHash of the rendered page (Hamming <= 6) and a
//!    64-bit simhash of the normalised text (Hamming <= 3). A near-duplicate is still processed, then
//!    shown in review as "probably the same as document X" ([`find_near_duplicate`],
//!    [`flag_near_duplicate`]).
//! 3. **Observation match**: same profile, analyte group, canonical value, comparator, method and
//!    collection time, and not two different labs: one observation with one more provenance link
//!    ([`match_observation`], [`link_observation`]). A missing collection time or a different
//!    comparator is sent to review, never merged.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::blobs::sha256_hex_to_bytes;
use crate::error::Result;
use crate::observations::ObservationIn;
use crate::review::{ReviewItemIn, create_item};

pub const PHASH_MAX_DISTANCE: u32 = 6;
/// Measured on synthetic LabNorte pages (SPIKES-v0.2.md 5): a changed footer moves 0 to 2 bits, another
/// report from the same template 8 to 20.
pub const SIMHASH_MAX_DISTANCE: u32 = 4;
/// A word pair whose second word has a digit (a value, a date, a report id) counts this many times
/// more than a template pair, so the same lab template with other values is not a near-duplicate.
const DIGIT_PAIR_WEIGHT: i64 = 8;

// ------------------------------------------------------- level 0: items ----

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SourceItem {
    pub source_id: Uuid,
    pub external_id: String,
    pub blob_sha256: Option<Vec<u8>>,
    pub document_id: Option<Uuid>,
    pub quarantine_id: Option<Uuid>,
    pub seen_at: DateTime<Utc>,
}

pub async fn seen_item(conn: &mut PgConnection, source_id: Uuid, external_id: &str) -> Result<Option<SourceItem>> {
    Ok(sqlx::query_as(
        "select source_id, external_id, blob_sha256, document_id, quarantine_id, seen_at from source_item \
         where source_id = $1 and external_id = $2",
    )
    .bind(source_id)
    .bind(external_id)
    .fetch_optional(conn)
    .await?)
}

/// Remember an item. True when it was new; a second call for the same id changes nothing.
pub async fn record_item(
    conn: &mut PgConnection,
    source_id: Uuid,
    external_id: &str,
    blob_sha256: Option<&str>,
    document_id: Option<Uuid>,
    quarantine_id: Option<Uuid>,
) -> Result<bool> {
    let sha = blob_sha256.map(sha256_hex_to_bytes).transpose()?;
    let n = sqlx::query(
        "insert into source_item (source_id, external_id, blob_sha256, document_id, quarantine_id) \
         values ($1, $2, $3, $4, $5) on conflict do nothing",
    )
    .bind(source_id)
    .bind(external_id)
    .bind(sha)
    .bind(document_id)
    .bind(quarantine_id)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(n == 1)
}

// --------------------------------------------------- level 1: file hash ----

/// The document holding these bytes for this profile (or the household, when unassigned).
pub async fn existing_document(
    conn: &mut PgConnection,
    household_id: Uuid,
    profile_id: Option<Uuid>,
    sha256: &str,
) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "select id from document where coalesce(profile_id, household_id) = coalesce($1, $2) and blob_sha256 = $3",
    )
    .bind(profile_id)
    .bind(household_id)
    .bind(sha256_hex_to_bytes(sha256)?)
    .fetch_optional(conn)
    .await?)
}

// --------------------------------------------- level 2: fingerprints ----

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// FNV-1a, 64 bit: a stable hash for the simhash features (the same on every build and platform).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// 64-bit simhash of a page's text: lower-case alphanumeric words, features are word pairs, pairs
/// ending in a number weigh more. A changed footer moves a few bits; another report many more.
pub fn simhash64(text: &str) -> Option<u64> {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.len() < 2 {
        return None;
    }
    let mut weights = [0i64; 64];
    for pair in words.windows(2) {
        let weight = if pair[1].chars().any(|c| c.is_ascii_digit()) {
            DIGIT_PAIR_WEIGHT
        } else {
            1
        };
        let h = fnv1a(format!("{} {}", pair[0], pair[1]).as_bytes());
        for (bit, w) in weights.iter_mut().enumerate() {
            *w += if h >> bit & 1 == 1 { weight } else { -weight };
        }
    }
    Some(
        weights
            .iter()
            .enumerate()
            .fold(0u64, |acc, (bit, w)| if *w > 0 { acc | 1 << bit } else { acc }),
    )
}

/// 64-bit perceptual hash (DCT pHash) of a grayscale image, row-major, `width * height` bytes: area
/// resize to 32x32, 2-D DCT-II, the 8x8 lowest frequencies without the DC term against their median.
pub fn phash64(gray: &[u8], width: usize, height: usize) -> Option<u64> {
    const N: usize = 32;
    if width == 0 || height == 0 || gray.len() < width * height {
        return None;
    }
    let mut small = [[0f64; N]; N];
    for (y, row) in small.iter_mut().enumerate() {
        let (y0, y1) = (y * height / N, ((y + 1) * height / N).max(y * height / N + 1));
        for (x, cell) in row.iter_mut().enumerate() {
            let (x0, x1) = (x * width / N, ((x + 1) * width / N).max(x * width / N + 1));
            let mut sum = 0f64;
            for yy in y0..y1.min(height) {
                for xx in x0..x1.min(width) {
                    sum += f64::from(gray[yy * width + xx]);
                }
            }
            *cell = sum / ((y1.min(height) - y0) * (x1.min(width) - x0)) as f64;
        }
    }
    let cos: Vec<Vec<f64>> = (0..8)
        .map(|u| {
            (0..N)
                .map(|x| ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / (2 * N) as f64).cos())
                .collect()
        })
        .collect();
    let mut coef = Vec::with_capacity(64);
    for u in 0..8 {
        for v in 0..8 {
            let mut s = 0f64;
            for (y, row) in small.iter().enumerate() {
                for (x, p) in row.iter().enumerate() {
                    s += p * cos[u][y] * cos[v][x];
                }
            }
            coef.push(s);
        }
    }
    let mut ac: Vec<f64> = coef[1..].to_vec();
    ac.sort_by(f64::total_cmp);
    let median = ac[ac.len() / 2];
    Some(coef.iter().enumerate().fold(
        0u64,
        |acc, (i, c)| if i > 0 && *c > median { acc | 1 << i } else { acc },
    ))
}

/// Store a page's fingerprints (either may be unknown: no text, or not rendered).
pub async fn store_page_fingerprint(
    conn: &mut PgConnection,
    document_id: Uuid,
    page_no: i32,
    phash: Option<u64>,
    simhash: Option<u64>,
) -> Result<()> {
    sqlx::query(
        "update document_page set phash = coalesce($3, phash), text_simhash = coalesce($4, text_simhash) \
         where document_id = $1 and page_no = $2",
    )
    .bind(document_id)
    .bind(page_no)
    .bind(phash.map(|p| p.to_be_bytes().to_vec()))
    .bind(simhash.map(|s| s as i64))
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NearDuplicate {
    pub document_id: Uuid,
    pub pages: i32,
    pub max_phash_distance: Option<u32>,
    pub max_simhash_distance: Option<u32>,
}

type Prints = HashMap<i32, (Option<u64>, Option<u64>)>;

fn prints_of(rows: &[sqlx::postgres::PgRow]) -> Result<HashMap<Uuid, Prints>> {
    let mut out: HashMap<Uuid, Prints> = HashMap::new();
    for r in rows {
        let doc: Uuid = r.try_get("document_id")?;
        let page: i32 = r.try_get("page_no")?;
        let ph: Option<Vec<u8>> = r.try_get("phash")?;
        let sh: Option<i64> = r.try_get("text_simhash")?;
        let ph = ph.and_then(|b| <[u8; 8]>::try_from(b.as_slice()).ok().map(u64::from_be_bytes));
        out.entry(doc).or_default().insert(page, (ph, sh.map(|s| s as u64)));
    }
    Ok(out)
}

/// Compare two documents page by page: every page needs at least one fingerprint on both sides, and
/// every fingerprint both sides have must be within its distance.
fn compare(a: &Prints, b: &Prints) -> Option<(Option<u32>, Option<u32>)> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let (mut max_p, mut max_s): (Option<u32>, Option<u32>) = (None, None);
    for (page, (pa, sa)) in a {
        let (pb, sb) = b.get(page)?;
        let mut checked = false;
        if let (Some(x), Some(y)) = (pa, pb) {
            let d = hamming(*x, *y);
            if d > PHASH_MAX_DISTANCE {
                return None;
            }
            max_p = Some(max_p.map_or(d, |m| m.max(d)));
            checked = true;
        }
        if let (Some(x), Some(y)) = (sa, sb) {
            let d = hamming(*x, *y);
            if d > SIMHASH_MAX_DISTANCE {
                return None;
            }
            max_s = Some(max_s.map_or(d, |m| m.max(d)));
            checked = true;
        }
        if !checked {
            return None;
        }
    }
    Some((max_p, max_s))
}

/// The closest earlier active document of the same profile whose pages match this one's
/// fingerprints, if any. The exact-hash twin is level 1's job and is not looked at here.
pub async fn find_near_duplicate(conn: &mut PgConnection, document_id: Uuid) -> Result<Option<NearDuplicate>> {
    let rows = sqlx::query(
        "with me as (select id, household_id, profile_id, blob_sha256, created_at from document where id = $1) \
         select p.document_id, p.page_no, p.phash, p.text_simhash from document_page p join document d on d.id = p.document_id, me \
         where coalesce(d.profile_id, d.household_id) = coalesce(me.profile_id, me.household_id) \
           and d.status = 'active' \
           and (d.id = me.id or (d.blob_sha256 <> me.blob_sha256 and d.created_at <= me.created_at)) \
           and (p.phash is not null or p.text_simhash is not null)",
    )
    .bind(document_id)
    .fetch_all(&mut *conn)
    .await?;
    let prints = prints_of(&rows)?;
    let Some(mine) = prints.get(&document_id) else {
        return Ok(None);
    };
    let mut best: Option<NearDuplicate> = None;
    for (other, theirs) in &prints {
        if *other == document_id {
            continue;
        }
        if let Some((p, s)) = compare(mine, theirs) {
            let score = p.unwrap_or(0) + s.unwrap_or(0);
            if best
                .as_ref()
                .is_none_or(|b| score < b.max_phash_distance.unwrap_or(0) + b.max_simhash_distance.unwrap_or(0))
            {
                best = Some(NearDuplicate {
                    document_id: *other,
                    pages: mine.len() as i32,
                    max_phash_distance: p,
                    max_simhash_distance: s,
                });
            }
        }
    }
    Ok(best)
}

/// Mark `document_id` as probably the same as `near.document_id` and file one `near_duplicate`
/// review item (idempotent while it is open). The document is still processed; the review offers
/// "link instead of import".
pub async fn flag_near_duplicate(conn: &mut PgConnection, document_id: Uuid, near: &NearDuplicate) -> Result<Uuid> {
    let (household_id, profile_id): (Uuid, Option<Uuid>) =
        sqlx::query_as("update document set near_duplicate_of = $2 where id = $1 returning household_id, profile_id")
            .bind(document_id)
            .bind(near.document_id)
            .fetch_one(&mut *conn)
            .await?;
    let open: Option<Uuid> = sqlx::query_scalar(
        "select id from review_item where document_id = $1 and kind = 'near_duplicate' and status = 'open'",
    )
    .bind(document_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = open {
        return Ok(id);
    }
    create_item(
        conn,
        &ReviewItemIn {
            kind: "near_duplicate".into(),
            household_id: Some(household_id),
            profile_id,
            payload: json!({"document_id": document_id.to_string(), "same_as": near.document_id.to_string(),
                            "pages": near.pages, "max_phash_distance": near.max_phash_distance,
                            "max_simhash_distance": near.max_simhash_distance}),
            reasons: vec!["near_duplicate".into()],
            document_id: Some(document_id),
            ref_table: Some("document".into()),
            ref_id: Some(near.document_id),
            ..ReviewItemIn::default()
        },
    )
    .await
}

// --------------------------------------------- level 3: observations ----

/// The verdict of [`match_observation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "match", rename_all = "snake_case")]
pub enum ObservationMatch {
    /// No stored fact is the same: commit a new observation.
    New,
    /// This observation already holds the fact: link the new source to it, commit nothing.
    Same { observation_id: Uuid },
    /// Maybe the same fact, but a person must say: `reason` is `collection_time_unknown` or
    /// `comparator_differs`.
    Review { observation_id: Uuid, reason: String },
}

fn same_number(a: Option<Decimal>, b: Option<Decimal>) -> Option<bool> {
    Some(a?.normalize() == b?.normalize())
}

fn fold(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Level 3 for one incoming fact. `lab` is the issuing lab of the incoming row when known (the
/// document's provider); `time_known` is false when the source printed only a date.
pub async fn match_observation(
    conn: &mut PgConnection,
    obs: &ObservationIn,
    lab: Option<&str>,
    time_known: bool,
) -> Result<ObservationMatch> {
    let rows = sqlx::query(
        "select o.id, o.value_num, o.value_canonical, o.unit_ucum, o.unit_canonical, o.printed_value, o.value_cmp, \
         o.method, o.effective_at, d.provider_name \
         from observation o left join document d on d.id = o.document_id \
         where o.profile_id = $1 and o.alias_group = $2 and o.status in ('current', 'provisional') \
           and (o.effective_at at time zone 'UTC')::date = ($3 at time zone 'UTC')::date order by o.recorded_at",
    )
    .bind(obs.profile_id)
    .bind(&obs.alias_group)
    .bind(obs.effective_at)
    .fetch_all(conn)
    .await?;
    for r in &rows {
        let their_lab: Option<String> = r.try_get("provider_name")?;
        if let (Some(a), Some(b)) = (lab, their_lab.as_deref())
            && fold(a) != fold(b)
        {
            continue;
        }
        let canon = same_number(obs.value_canonical, r.try_get("value_canonical")?).filter(|_| {
            obs.unit_canonical.as_deref()
                == r.try_get::<Option<String>, _>("unit_canonical")
                    .ok()
                    .flatten()
                    .as_deref()
        });
        let num = same_number(obs.value_num, r.try_get("value_num")?)
            .map(|eq| eq && obs.unit_ucum == r.try_get::<Option<String>, _>("unit_ucum").ok().flatten());
        let same_value = canon.or(num).unwrap_or_else(|| {
            fold(&obs.printed_value) == fold(&r.try_get::<String, _>("printed_value").unwrap_or_default())
        });
        if !same_value {
            continue;
        }
        let id: Uuid = r.try_get("id")?;
        let cmp: Option<String> = r.try_get("value_cmp")?;
        if cmp != obs.value_cmp {
            return Ok(ObservationMatch::Review {
                observation_id: id,
                reason: "comparator_differs".into(),
            });
        }
        let method: Option<String> = r.try_get("method")?;
        if method.as_deref().map(fold) != obs.method.as_deref().map(fold) {
            continue;
        }
        let at: DateTime<Utc> = r.try_get("effective_at")?;
        if !time_known {
            return Ok(ObservationMatch::Review {
                observation_id: id,
                reason: "collection_time_unknown".into(),
            });
        }
        if at == obs.effective_at.with_timezone(&Utc) {
            return Ok(ObservationMatch::Same { observation_id: id });
        }
    }
    Ok(ObservationMatch::New)
}

/// Add one more provenance link to an observation (level 3 "same"). Idempotent.
pub async fn link_observation(
    conn: &mut PgConnection,
    observation_id: Uuid,
    src_id: &str,
    document_id: Option<Uuid>,
) -> Result<()> {
    sqlx::query(
        "insert into observation_link (observation_id, src_id, document_id) values ($1, $2, $3) on conflict do nothing",
    )
    .bind(observation_id)
    .bind(src_id)
    .bind(document_id)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_template_with_other_values_is_not_a_near_duplicate() {
        // A full page (30 rows). On a short page the footer weighs more and a re-download can be missed,
        // which is harmless: level 3 still matches the facts.
        let rows = [
            "Hemoglobina",
            "Hematocrito",
            "Leucocitos",
            "Plaquetas",
            "Glucosa",
            "Creatinina",
            "Urea",
            "Sodio",
            "Potasio",
            "Colesterol total",
            "HDL",
            "LDL",
            "Triglicéridos",
            "TSH",
            "T4 libre",
            "Ferritina",
            "Hierro",
            "Vitamina D",
            "Vitamina B12",
            "Ácido fólico",
            "GOT",
            "GPT",
            "GGT",
            "Fosfatasa alcalina",
            "Bilirrubina",
            "Proteínas totales",
            "Albúmina",
            "Calcio",
            "Fósforo",
            "Magnesio",
        ];
        let page = |seed: u32, printed: &str| {
            let mut s = format!("LabNorte Informe de resultados Paciente RIVERA ALEX Nº de informe LN-{seed} ");
            for (i, n) in rows.iter().enumerate() {
                let v = (i as u32 * 37 + seed * 13) % 200;
                s.push_str(&format!(
                    "{n} {v},{} mg/dL {} - {} ",
                    (i as u32 + seed) % 10,
                    v / 2,
                    v * 2
                ));
            }
            s + printed
        };
        assert_eq!(simhash64("one"), None);
        let a = simhash64(&page(0, "Impreso 14/09/2026 10:31")).expect("hash");
        let again = simhash64(&page(0, "Impreso 02/10/2026 08:02")).expect("hash");
        assert!(
            hamming(a, again) <= SIMHASH_MAX_DISTANCE,
            "re-download {} bits",
            hamming(a, again)
        );
        for seed in 1..20 {
            let next = simhash64(&page(seed, "Impreso 14/09/2026 10:31")).expect("hash");
            assert!(
                hamming(a, next) > SIMHASH_MAX_DISTANCE,
                "report {seed} only {} bits",
                hamming(a, next)
            );
        }
    }

    fn page(seed: u32, noise: u8) -> Vec<u8> {
        (0..200 * 280)
            .map(|i| {
                let (x, y) = (i % 200, i / 200);
                let ink = (x / 20 + y / 14 + seed).is_multiple_of(3) as u8 * 180;
                255 - ink - (i.wrapping_mul(2_654_435_761) >> 28) as u8 % (noise + 1)
            })
            .collect()
    }

    #[test]
    fn phash_is_close_for_a_noisy_copy_and_far_for_another_page() {
        let a = phash64(&page(0, 0), 200, 280).expect("hash");
        let noisy = phash64(&page(0, 12), 200, 280).expect("hash");
        let other = phash64(&page(1, 0), 200, 280).expect("hash");
        assert!(
            hamming(a, noisy) <= PHASH_MAX_DISTANCE,
            "noise moved {}",
            hamming(a, noisy)
        );
        assert!(
            hamming(a, other) > PHASH_MAX_DISTANCE,
            "another page only {}",
            hamming(a, other)
        );
        assert_eq!(phash64(&[], 0, 0), None);
    }

    #[test]
    fn pages_compare_on_every_fingerprint_both_sides_have() {
        let a: Prints = [(1, (Some(0u64), Some(0u64)))].into();
        let near: Prints = [(1, (Some(0b111u64), Some(0b1u64)))].into();
        let far_text: Prints = [(1, (Some(0u64), Some(0xffu64)))].into();
        let none: Prints = [(1, (None, None))].into();
        assert_eq!(compare(&a, &near), Some((Some(3), Some(1))));
        assert_eq!(compare(&a, &far_text), None);
        assert_eq!(compare(&a, &none), None);
        let two: Prints = [(1, (Some(0u64), None)), (2, (Some(0u64), None))].into();
        assert_eq!(compare(&a, &two), None, "page counts differ");
    }
}
