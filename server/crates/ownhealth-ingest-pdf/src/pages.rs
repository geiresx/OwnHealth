//! Page access for the review split view and `propose`: the stored word layer, page renders, and
//! grounding a printed value on a page (`ingest/pdf/__init__.py`, page access).

use std::io::Write as _;
use std::sync::LazyLock;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::{Error, Result};
use pdfium_render::prelude::*;
use regex::Regex;
use serde_json::Value;
use sqlx::{PgConnection, Row as _};
use uuid::Uuid;

use crate::Word;
use crate::classify::fold;
use crate::pdfium::{pdf_error, pdfium};
use crate::textlayer::read_pdf;

fn word_from(w: &Value) -> Option<Word> {
    let f = |k: &str| w.get(k).and_then(Value::as_f64);
    Some(Word {
        text: w.get("text")?.as_str()?.to_string(),
        bbox: [f("x0")?, f("top")?, f("x1")?, f("bottom")?],
        font: w.get("font").and_then(Value::as_str).unwrap_or("").to_string(),
        size: f("size").unwrap_or(0.0),
        bold: w.get("bold").and_then(Value::as_bool).unwrap_or(false),
        hidden: w.get("hidden").and_then(Value::as_str).map(String::from),
    })
}

/// The stored word layer of one page (hidden words included and marked). [] for an unknown page.
pub async fn page_words(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    page_no: i32,
) -> Result<Vec<Word>> {
    let row = sqlx::query(
        "select p.words_blob, d.blob_sha256, d.mime from document d left join document_page p \
         on p.document_id = d.id and p.page_no = $2 where d.id = $1",
    )
    .bind(document_id)
    .bind(page_no)
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else { return Ok(Vec::new()) };
    let words_blob: Option<Vec<u8>> = row.try_get("words_blob")?;
    let mime: String = row.try_get("mime")?;
    let words: Vec<Value> = if let Some(wb) = words_blob {
        let layer: Value = serde_json::from_slice(&blobs.get_bytes(&hex::encode(wb))?)?;
        layer
            .get("words")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else if mime == "application/pdf" {
        let sha: Vec<u8> = row.try_get("blob_sha256")?;
        let data = blobs.get_bytes(&hex::encode(sha))?;
        let model = tokio::task::spawn_blocking(move || read_pdf(&data))
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("PDF read panicked: {e}")))??;
        let Some(page) = usize::try_from(page_no - 1).ok().and_then(|i| model.pages.get(i)) else {
            return Ok(Vec::new());
        };
        page.words.iter().map(|w| w.as_dict()).collect()
    } else {
        return Ok(Vec::new());
    };
    Ok(words.iter().filter_map(word_from).collect())
}

/// A PNG of one page, scale clamped to 0.5..4 (CPU work: call it from `spawn_blocking`). A page out
/// of range is `Error::NotFound`.
pub fn render_page_png(blobs: &BlobStore, blob_sha256: &str, page_no: i32, scale: f64) -> Result<Vec<u8>> {
    let data = blobs.get_bytes(blob_sha256)?;
    let doc = pdfium()?.load_pdf_from_byte_slice(&data, None).map_err(pdf_error)?;
    let n = doc.pages().len() as i32;
    if !(1..=n).contains(&page_no) {
        return Err(Error::NotFound(format!("page {page_no} out of range")));
    }
    let page = doc.pages().get((page_no - 1) as _).map_err(pdf_error)?;
    let scale = scale.clamp(0.5, 4.0);
    // pypdfium2 rounded the pixel size up: ceil(points * scale)
    let w = (page.width().value as f64 * scale).ceil() as i32;
    let h = (page.height().value as f64 * scale).ceil() as i32;
    let cfg = PdfRenderConfig::new().set_target_width(w).set_target_height(h);
    let bitmap = page.render_with_config(&cfg).map_err(pdf_error)?;
    let rgba = bitmap.as_rgba_bytes();
    let (bw, bh) = (bitmap.width() as u32, bitmap.height() as u32);
    png_rgb(&rgba, bw, bh)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend(&body);
    out.extend(crc32(&body).to_be_bytes());
}

/// An 8-bit RGB PNG from RGBA pixels (alpha dropped: pages render on white).
fn png_rgb(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for row in rgba.chunks_exact(w as usize * 4).take(h as usize) {
        raw.push(0); // filter: none
        for px in row.as_chunks::<4>().0 {
            raw.extend_from_slice(&px[..3]);
        }
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    z.write_all(&raw)?;
    let idat = z.finish()?;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

static NUM_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-+]?\d+(?:[.,' ]\d+)*$").expect("pattern"));
static NUM_SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.,' ]").expect("pattern"));
static ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9]+").expect("pattern"));

/// '2,41' / '2.41' / '250.000' -> a separator-free key; None when it is not a number.
fn num_key(s: &str) -> Option<String> {
    let s = s.trim().trim_start_matches(['<', '>', '≤', '≥', '=']).trim();
    if !NUM_KEY.is_match(s) {
        return None;
    }
    let k = NUM_SEP.replace_all(s, "").trim_start_matches('0').to_string();
    Some(if k.is_empty() { "0".into() } else { k })
}

fn tokens(s: &str) -> Vec<String> {
    ALNUM.find_iter(&fold(s)).map(|m| m.as_str().to_string()).collect()
}

fn same_line(a: &Word, b: &Word) -> bool {
    a.bbox[3].min(b.bbox[3]) - a.bbox[1].max(b.bbox[1]) > 0.5 * (a.bbox[3] - a.bbox[1]).min(b.bbox[3] - b.bbox[1])
}

/// Find a printed value on the page word layer (decimal-separator tolerant). Used by `propose`.
///
/// Returns the bbox of the value word, widened to the name words when they sit on the same line;
/// None when the value is not on the page. Hidden words never ground a value.
pub async fn ground_value(
    conn: &mut PgConnection,
    blobs: &BlobStore,
    document_id: Uuid,
    page_no: i32,
    name: &str,
    value: &str,
) -> Result<Option<[f64; 4]>> {
    let words: Vec<Word> = page_words(conn, blobs, document_id, page_no)
        .await?
        .into_iter()
        .filter(|w| w.hidden.is_none())
        .collect();
    Ok(ground_in(&words, name, value))
}

fn ground_in(words: &[Word], name: &str, value: &str) -> Option<[f64; 4]> {
    let want = num_key(value);
    let folded_value = fold(value).trim().to_string();
    let hits: Vec<&Word> = words
        .iter()
        .filter(|w| match &want {
            Some(k) => num_key(&w.text).as_ref() == Some(k),
            None => fold(&w.text).trim() == folded_value,
        })
        .collect();
    let first = *hits.first()?;
    let name_toks: Vec<String> = tokens(name).into_iter().filter(|t| t.len() > 1).collect();
    let (mut best, mut best_score, mut best_names) = (first, -1i64, Vec::new());
    for h in hits {
        let on_line: Vec<&Word> = words
            .iter()
            .filter(|w| {
                same_line(w, h) && w.bbox[2] <= h.bbox[0] && tokens(&w.text).iter().any(|t| name_toks.contains(t))
            })
            .collect();
        let mut seen: Vec<String> = on_line
            .iter()
            .flat_map(|w| tokens(&w.text))
            .filter(|t| name_toks.contains(t))
            .collect();
        seen.sort();
        seen.dedup();
        let score = seen.len() as i64;
        if score > best_score {
            (best, best_score, best_names) = (h, score, on_line);
        }
    }
    let mut b = best.bbox;
    for w in best_names {
        b = [
            b[0].min(w.bbox[0]),
            b[1].min(w.bbox[1]),
            b[2].max(w.bbox[2]),
            b[3].max(w.bbox[3]),
        ];
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, x0: f64, x1: f64) -> Word {
        Word {
            text: text.into(),
            bbox: [x0, 100.0, x1, 109.0],
            font: String::new(),
            size: 9.0,
            bold: false,
            hidden: None,
        }
    }

    #[test]
    fn num_key_ignores_separators() {
        assert_eq!(num_key("2,41"), Some("241".into()));
        assert_eq!(num_key("< 0.5"), Some("5".into()));
        assert_eq!(num_key("250 000"), Some("250000".into()));
        assert_eq!(num_key("negativo"), None);
    }

    #[test]
    fn grounding_widens_to_the_name_on_the_line() {
        let words = [
            w("Hemoglobina", 56.0, 110.0),
            w("14,2", 300.0, 316.0),
            w("g/dL", 352.0, 370.0),
        ];
        assert_eq!(
            ground_in(&words, "Hemoglobina", "14.2"),
            Some([56.0, 100.0, 316.0, 109.0])
        );
        assert_eq!(ground_in(&words, "Hemoglobina", "9999"), None);
    }
}
