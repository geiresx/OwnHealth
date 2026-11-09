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

/// The most pixels one page render may hold (A3 at scale 4 is about 16 M): a page's own `MediaBox`
/// can ask for a 57600 x 57600 render, which is gigabytes.
pub const MAX_RENDER_PIXELS: f64 = 25_000_000.0;

/// The pixel size of a `width` x `height` pt page at `scale` (clamped to 0.5..4; NaN is 0.5, as
/// Python's `max(0.5, min(scale, 4.0))`), scaled down to fit `MAX_RENDER_PIXELS`.
fn render_size(width: f64, height: f64, scale: f64) -> (i32, i32) {
    let scale = if scale.is_nan() { 0.5 } else { scale.clamp(0.5, 4.0) };
    // pypdfium2 rounded the pixel size up: ceil(points * scale)
    let (w, h) = ((width * scale).ceil(), (height * scale).ceil());
    if w * h <= MAX_RENDER_PIXELS {
        return (w as i32, h as i32);
    }
    // rounded down, so the product stays under the cap
    let k = scale * (MAX_RENDER_PIXELS / (w * h)).sqrt();
    (
        ((width * k).floor() as i32).max(1),
        ((height * k).floor() as i32).max(1),
    )
}

/// A PNG of one page, scale clamped to 0.5..4 and the size to `MAX_RENDER_PIXELS` (CPU work: call it
/// from `spawn_blocking`). A page out of range is `Error::NotFound`.
pub fn render_page_png(blobs: &BlobStore, blob_sha256: &str, page_no: i32, scale: f64) -> Result<Vec<u8>> {
    let data = blobs.get_bytes(blob_sha256)?;
    let doc = pdfium()?.load_pdf_from_byte_slice(&data, None).map_err(pdf_error)?;
    let n = doc.pages().len() as i32;
    if !(1..=n).contains(&page_no) {
        return Err(Error::NotFound(format!("page {page_no} out of range")));
    }
    let page = doc.pages().get((page_no - 1) as _).map_err(pdf_error)?;
    let (w, h) = render_size(page.width().value as f64, page.height().value as f64, scale);
    let cfg = PdfRenderConfig::new().set_target_width(w).set_target_height(h);
    let bitmap = page.render_with_config(&cfg).map_err(pdf_error)?;
    let rgba = bitmap.as_rgba_bytes();
    let (bw, bh) = (bitmap.width() as u32, bitmap.height() as u32);
    png_rgb(&rgba, bw, bh)
}

/// The pixel width of the render a page's pHash is taken from (the hash itself reads 32x32).
const PHASH_RENDER_WIDTH: i32 = 256;

/// Dedup level 2: the 64-bit pHash of every page of a PDF, from a small grayscale render (CPU work:
/// call it from `spawn_blocking`). A page that will not render is `None`.
pub fn page_phashes(data: &[u8]) -> Result<Vec<Option<u64>>> {
    let doc = pdfium()?.load_pdf_from_byte_slice(data, None).map_err(pdf_error)?;
    let mut out = Vec::new();
    for page in doc.pages().iter() {
        let (w, h) = (page.width().value as f64, page.height().value as f64);
        if !(w > 0.0 && h > 0.0) {
            out.push(None);
            continue;
        }
        let height = ((PHASH_RENDER_WIDTH as f64 * h / w).round() as i32).clamp(1, PHASH_RENDER_WIDTH * 4);
        let cfg = PdfRenderConfig::new()
            .set_target_width(PHASH_RENDER_WIDTH)
            .set_target_height(height);
        let Ok(bitmap) = page.render_with_config(&cfg) else {
            out.push(None);
            continue;
        };
        let rgba = bitmap.as_rgba_bytes();
        let (bw, bh) = (bitmap.width() as usize, bitmap.height() as usize);
        let gray: Vec<u8> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| ((u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114) / 1000) as u8)
            .collect();
        out.push(ownhealth_core::dedup::phash64(&gray, bw, bh));
    }
    Ok(out)
}

/// The largest side of a stored image we decode, in pixels (a 12000 x 12000 photo is 144 Mpx).
pub const MAX_IMAGE_SIDE: u32 = 12_000;
/// The most memory one image decode may allocate.
pub const MAX_IMAGE_ALLOC: u64 = 256 * 1024 * 1024;

/// A photo or scan stored as an image (PNG or JPEG), decoded upright: the EXIF orientation a phone
/// camera writes (pixels in sensor order, `Orientation = 6` for a portrait shot) is applied. Sizes past
/// `MAX_IMAGE_SIDE` or `MAX_IMAGE_ALLOC` are refused before decoding. CPU work: call it off the async
/// workers.
pub fn decode_image(data: &[u8]) -> Result<image::DynamicImage> {
    use image::ImageDecoder as _;
    let bad = |e: image::ImageError| match e {
        image::ImageError::Limits(_) => Error::Invalid("image too large".into()),
        e => Error::Invalid(format!("unreadable image: {e}")),
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| Error::Invalid(format!("unreadable image: {e}")))?;
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(bad)?;
    let orientation = decoder.orientation().map_err(bad)?;
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(bad)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// The pHash of a photo or scan stored as an image (PNG or JPEG), as one page: the same 256-pixel-wide
/// grayscale the PDF pages are hashed from, so a photo of a report we hold matches its PDF page.
pub fn image_phash(data: &[u8]) -> Result<Vec<Option<u64>>> {
    let img = decode_image(data)?;
    let max_h = (PHASH_RENDER_WIDTH * 4) as u32;
    let gray = img
        .resize(PHASH_RENDER_WIDTH as u32, max_h, image::imageops::FilterType::Triangle)
        .to_luma8();
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    Ok(vec![ownhealth_core::dedup::phash64(gray.as_raw(), w, h)])
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

    #[test]
    fn render_size_is_capped_and_nan_is_the_smallest_scale() {
        assert_eq!(render_size(595.0, 842.0, 1.0), (595, 842));
        assert_eq!(render_size(595.3, 841.9, 2.0), (1191, 1684));
        assert_eq!(render_size(595.0, 842.0, 9.0), (2380, 3368));
        assert_eq!(render_size(595.0, 842.0, f64::NAN), (298, 421));
        assert_eq!(render_size(595.0, 842.0, f64::INFINITY), (2380, 3368));
        let (w, h) = render_size(14400.0, 14400.0, 4.0);
        assert!(f64::from(w) * f64::from(h) <= MAX_RENDER_PIXELS, "{w}x{h}");
        assert!(w > 4900 && w == h, "{w}x{h}");
        let (w, h) = render_size(14400.0, 3.0, 4.0);
        assert!(f64::from(w) * f64::from(h) <= MAX_RENDER_PIXELS && h >= 1, "{w}x{h}");
    }

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

    /// A JPEG of `img` with an EXIF APP1 segment carrying `Orientation = orientation`, as a phone writes.
    fn jpeg_with_orientation(img: &image::RgbImage, orientation: u16) -> Vec<u8> {
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95);
        img.write_with_encoder(enc).unwrap_or_else(|e| panic!("encode: {e}"));
        // TIFF, little endian: header, one IFD with one SHORT entry (0x0112), no next IFD
        let mut tiff = b"II*\0".to_vec();
        tiff.extend(8u32.to_le_bytes());
        tiff.extend(1u16.to_le_bytes());
        tiff.extend(0x0112u16.to_le_bytes());
        tiff.extend(3u16.to_le_bytes());
        tiff.extend(1u32.to_le_bytes());
        tiff.extend(orientation.to_le_bytes());
        tiff.extend([0, 0]);
        tiff.extend(0u32.to_le_bytes());
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend(tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend([0xFF, 0xE1]);
        out.extend(((app1.len() + 2) as u16).to_be_bytes());
        out.extend(app1);
        out.extend(&jpeg[2..]);
        out
    }

    #[test]
    fn a_portrait_phone_photo_is_decoded_upright_and_hashes_like_the_upright_page() -> Result<()> {
        // the upright page: 200 wide, 300 tall, a dark band across the top third
        let upright = image::RgbImage::from_fn(200, 300, |_, y| {
            if y < 100 {
                image::Rgb([20, 20, 20])
            } else {
                image::Rgb([235, 235, 235])
            }
        });
        // the camera stores it in sensor order (rotated 90 degrees counter-clockwise) with Orientation 6
        let sensor = image::imageops::rotate270(&upright);
        let photo = jpeg_with_orientation(&sensor, 6);
        let img = decode_image(&photo)?;
        assert_eq!((img.width(), img.height()), (200, 300));
        let rgb = img.to_rgb8();
        assert!(rgb.get_pixel(100, 20)[0] < 80 && rgb.get_pixel(100, 280)[0] > 180);
        let straight = jpeg_with_orientation(&upright, 1);
        assert_eq!(image_phash(&photo)?, image_phash(&straight)?);
        Ok(())
    }

    #[test]
    fn an_image_that_declares_a_huge_size_is_refused_before_decoding() {
        // a valid PNG header for 40000 x 40000 RGB, no pixel data
        let mut ihdr = Vec::new();
        ihdr.extend(40_000u32.to_be_bytes());
        ihdr.extend(40_000u32.to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IEND", &[]);
        let e = decode_image(&png);
        assert!(
            matches!(e, Err(Error::Invalid(ref m)) if m == "image too large"),
            "{e:?}"
        );
        assert!(image_phash(&png).is_err());
    }
}
