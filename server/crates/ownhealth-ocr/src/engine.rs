//! The OCR engine (lane C). `load` reads the models once per process from `OWNHEALTH_OCR_MODELS_DIR`
//! (the files `scripts/fetch-ocr-models.sh` wrote, sha256-checked there); `read_page` is CPU work: call
//! it from `spawn_blocking`.
//!
//! The detector finds text lines (on a lab slip, one per table cell); the recognizer reads each line
//! and, with `return_word_box`, places every character. `words_from_lines` turns that into words with
//! boxes in PDF points, in the same shape as a text layer. A scan or a photo is rarely straight, so it
//! also measures the page's skew from the line boxes and turns the word positions back by it:
//! without that, a 1-degree tilt moves the right end of a row 10 pt up or down and the table reader
//! mixes rows.

use std::path::Path;

use crate::{OcrError, OcrPage, OcrWord};

/// The model files, as `scripts/fetch-ocr-models.sh` names them.
pub const DET_MODEL: &str = "pp-ocrv5_mobile_det.onnx";
pub const REC_MODEL: &str = "latin_pp-ocrv5_mobile_rec.onnx";
pub const REC_DICT: &str = "ppocrv5_latin_dict.txt";

/// The detector grows every text box by a margin around the ink (DB "unclip"): about 40% of the box's
/// thickness is margin. Measured on the eval set at 200 dpi: 9 pt text, 15.5 pt boxes. Without taking
/// it off, the boxes of neighbouring table rows overlap and the table reader merges the rows.
const INK_SHARE: f64 = 0.6;

/// A line this many times longer than thick says something about the page's skew.
const MIN_SKEW_ASPECT: f64 = 5.0;

/// ONNX Runtime threads per page. The box is shared with the web server and Postgres, and pages are
/// read one at a time on the `ocr` queue.
#[cfg(feature = "engine")]
const THREADS: usize = 4;

/// A loaded engine. Cheap to share behind an `Arc`.
pub struct Engine {
    #[cfg(feature = "engine")]
    ocr: oar_ocr::oarocr::OAROCR,
    /// sha256 over the three model files (`ocr_run.models_sha256`): a model change is a new run.
    pub models_sha256: String,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("models_sha256", &self.models_sha256)
            .finish()
    }
}

/// Load the models from `dir`. Without the `engine` feature: `OcrError::EngineMissing`; a missing
/// model file: `OcrError::Model` naming it.
pub fn load(dir: &Path) -> Result<Engine, OcrError> {
    #[cfg(not(feature = "engine"))]
    {
        let _ = dir;
        Err(OcrError::EngineMissing)
    }
    #[cfg(feature = "engine")]
    {
        use oar_ocr::core::config::OrtSessionConfig;
        use sha2::{Digest, Sha256};

        let mut all = Sha256::new();
        for name in [DET_MODEL, REC_MODEL, REC_DICT] {
            let path = dir.join(name);
            let bytes = std::fs::read(&path)
                .map_err(|e| OcrError::Model(format!("{} ({e}); run scripts/fetch-ocr-models.sh", path.display())))?;
            all.update(Sha256::digest(&bytes));
        }
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(THREADS));
        let ocr = oar_ocr::oarocr::OAROCRBuilder::new(dir.join(DET_MODEL), dir.join(REC_MODEL), dir.join(REC_DICT))
            .return_word_box(true)
            .ort_session(OrtSessionConfig::new().with_intra_threads(threads))
            .build()
            .map_err(|e| OcrError::Model(e.to_string()))?;
        Ok(Engine {
            ocr,
            models_sha256: hex::encode(all.finalize()),
        })
    }
}

/// Read one page image (RGB8, row-major, `width * height * 3` bytes) rendered at `dpi`; word boxes come
/// back in PDF points, origin top-left.
pub fn read_page(
    engine: &Engine,
    rgb: &[u8],
    width: u32,
    height: u32,
    dpi: f64,
    page_no: i32,
) -> Result<OcrPage, OcrError> {
    #[cfg(not(feature = "engine"))]
    {
        let _ = (engine, rgb, width, height, dpi, page_no);
        Err(OcrError::EngineMissing)
    }
    #[cfg(feature = "engine")]
    {
        let started = std::time::Instant::now();
        let img = image::RgbImage::from_raw(width, height, rgb.to_vec())
            .ok_or_else(|| OcrError::Engine(format!("{width}x{height} image with {} bytes", rgb.len())))?;
        let results = engine
            .ocr
            .predict(vec![img])
            .map_err(|e| OcrError::Engine(e.to_string()))?;
        let lines: Vec<Line> = results
            .first()
            .map(|r| r.text_regions.iter().filter_map(line_of).collect())
            .unwrap_or_default();
        Ok(OcrPage {
            page_no,
            width_pt: f64::from(width) * 72.0 / dpi,
            height_pt: f64::from(height) * 72.0 / dpi,
            words: words_from_lines(&lines, width, height, dpi),
            engine: crate::ENGINE.into(),
            engine_version: crate::ENGINE_VERSION.into(),
            seconds: started.elapsed().as_secs_f64(),
        })
    }
}

#[cfg(feature = "engine")]
fn line_of(r: &oar_ocr::oarocr::TextRegion) -> Option<Line> {
    let text = r.text.as_deref()?.trim().to_string();
    let pts = &r.bounding_box.points;
    if text.is_empty() || pts.len() < 4 {
        return None;
    }
    let quad = [0, 1, 2, 3].map(|i| [pts[i].x, pts[i].y]);
    let chars = r.word_boxes.as_ref().map(|boxes| {
        boxes
            .iter()
            .map(|b| [b.x_min(), b.y_min(), b.x_max(), b.y_max()])
            .collect()
    });
    Some(Line {
        quad,
        text,
        conf: r.confidence.unwrap_or(0.0),
        chars,
    })
}

/// One text line the detector found, in pixels: its four corners, the text, the recognizer's
/// confidence, and one box per character when the recognizer placed them.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub quad: [[f32; 2]; 4],
    pub text: String,
    pub conf: f32,
    pub chars: Option<Vec<[f32; 4]>>,
}

impl Line {
    /// The direction of the line's long edge, in radians, folded into (-45°, 45°].
    fn angle(&self) -> f64 {
        let [a, b, c, _] = self.quad.map(|p| [f64::from(p[0]), f64::from(p[1])]);
        let (e1, e2) = ([b[0] - a[0], b[1] - a[1]], [c[0] - b[0], c[1] - b[1]]);
        let long = if e1[0].hypot(e1[1]) >= e2[0].hypot(e2[1]) {
            e1
        } else {
            e2
        };
        let mut t = long[1].atan2(long[0]);
        let quarter = std::f64::consts::FRAC_PI_2;
        while t > quarter / 2.0 {
            t -= quarter;
        }
        while t <= -quarter / 2.0 {
            t += quarter;
        }
        t
    }

    /// Long edge over short edge: a line of text is much wider than tall.
    fn aspect(&self) -> f64 {
        let [a, b, c, _] = self.quad.map(|p| [f64::from(p[0]), f64::from(p[1])]);
        let (l1, l2) = ((b[0] - a[0]).hypot(b[1] - a[1]), (c[0] - b[0]).hypot(c[1] - b[1]));
        l1.max(l2) / l1.min(l2).max(1e-6)
    }

    fn length(&self) -> f64 {
        let [a, b, c, _] = self.quad.map(|p| [f64::from(p[0]), f64::from(p[1])]);
        (b[0] - a[0]).hypot(b[1] - a[1]).max((c[0] - b[0]).hypot(c[1] - b[1]))
    }

    fn thickness(&self) -> f64 {
        let [a, b, c, _] = self.quad.map(|p| [f64::from(p[0]), f64::from(p[1])]);
        (b[0] - a[0]).hypot(b[1] - a[1]).min((c[0] - b[0]).hypot(c[1] - b[1]))
    }

    fn centre(&self) -> [f64; 2] {
        let n = self.quad.len() as f64;
        let sx: f64 = self.quad.iter().map(|p| f64::from(p[0])).sum();
        let sy: f64 = self.quad.iter().map(|p| f64::from(p[1])).sum();
        [sx / n, sy / n]
    }

    fn x_range(&self) -> (f64, f64) {
        let xs = self.quad.map(|p| f64::from(p[0]));
        (
            xs.iter().copied().fold(f64::INFINITY, f64::min),
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        )
    }
}

/// The page's skew: the median angle of its long lines, each weighted by its length (0 when none
/// qualifies). Only long lines count: the detector's box around a short word ("Hemoglobina", "14,2")
/// is usually axis-aligned at these small angles and would pull the median to 0.
pub fn skew(lines: &[Line]) -> f64 {
    let mut angles: Vec<(f64, f64)> = lines
        .iter()
        .filter(|l| l.aspect() >= MIN_SKEW_ASPECT)
        .map(|l| (l.angle(), l.length()))
        .collect();
    angles.sort_by(|a, b| a.0.total_cmp(&b.0));
    let half = angles.iter().map(|a| a.1).sum::<f64>() / 2.0;
    let mut seen = 0.0;
    for (angle, len) in angles {
        seen += len;
        if seen >= half {
            return angle;
        }
    }
    0.0
}

/// Lines -> words with boxes in PDF points, the page's skew turned back around its centre. A word's
/// confidence is its line's.
pub fn words_from_lines(lines: &[Line], width: u32, height: u32, dpi: f64) -> Vec<OcrWord> {
    let theta = skew(lines);
    let (sin, cos) = theta.sin_cos();
    let (cx0, cy0) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let k = 72.0 / dpi;
    let mut out = Vec::new();
    for line in lines {
        let [lcx, lcy] = line.centre();
        let half = line.thickness() * INK_SHARE / 2.0;
        for (text, x0, x1) in split_words(line) {
            // the word's centre on the line's centre line, then turned back by the page's skew
            let xc = (x0 + x1) / 2.0;
            let yc = lcy + theta.tan() * (xc - lcx);
            let (dx, dy) = (xc - cx0, yc - cy0);
            let (x, y) = (cx0 + dx * cos + dy * sin, cy0 - dx * sin + dy * cos);
            let half_w = (x1 - x0) / cos.max(0.5) / 2.0;
            out.push(OcrWord {
                text,
                bbox: [(x - half_w) * k, (y - half) * k, (x + half_w) * k, (y + half) * k],
                conf: line.conf,
            });
        }
    }
    out.sort_by(|a, b| a.bbox[1].total_cmp(&b.bbox[1]).then(a.bbox[0].total_cmp(&b.bbox[0])));
    out
}

/// The words of a line with their x range in pixels: from the character boxes when there is one per
/// character, otherwise in proportion to the character count.
fn split_words(line: &Line) -> Vec<(String, f64, f64)> {
    let chars: Vec<char> = line.text.chars().collect();
    let (lx0, lx1) = line.x_range();
    let margin = (line.thickness() * (1.0 - INK_SHARE) / 2.0).min((lx1 - lx0) / 4.0);
    let (lx0, lx1) = (lx0 + margin, lx1 - margin);
    let boxes: Vec<(f64, f64)> = match &line.chars {
        Some(b) if b.len() == chars.len() => b
            .iter()
            .map(|c| (f64::from(c[0]).clamp(lx0, lx1), f64::from(c[2]).clamp(lx0, lx1)))
            .collect(),
        _ => {
            let step = (lx1 - lx0) / chars.len().max(1) as f64;
            (0..chars.len())
                .map(|i| (lx0 + step * i as f64, lx0 + step * (i + 1) as f64))
                .collect()
        }
    };
    let mut out = Vec::new();
    let mut cur: Option<(String, f64, f64)> = None;
    for (c, (x0, x1)) in chars.iter().zip(boxes) {
        if c.is_whitespace() {
            out.extend(cur.take());
            continue;
        }
        match cur.as_mut() {
            Some((s, _, e)) => {
                s.push(*c);
                *e = x1;
            }
            None => cur = Some((c.to_string(), x0, x1)),
        }
    }
    out.extend(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line of `text` whose left end is at (x, y), `len` px long and 25 px thick, turned by `deg`
    /// around the page centre (1000 x 1400 px).
    fn line(text: &str, x: f32, y: f32, len: f32, deg: f32) -> Line {
        let (s, c) = deg.to_radians().sin_cos();
        let turn = |p: [f32; 2]| {
            let (dx, dy) = (p[0] - 500.0, p[1] - 700.0);
            [500.0 + dx * c - dy * s, 700.0 + dx * s + dy * c]
        };
        Line {
            quad: [[x, y], [x + len, y], [x + len, y + 25.0], [x, y + 25.0]].map(turn),
            text: text.into(),
            conf: 0.97,
            chars: None,
        }
    }

    #[test]
    fn straight_lines_become_words_in_points() {
        let words = words_from_lines(&[line("14,2 g/dL", 100.0, 200.0, 180.0, 0.0)], 1000, 1400, 200.0);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "14,2");
        assert_eq!(words[1].text, "g/dL");
        let k = 72.0 / 200.0;
        // the detector's margin (5 px a side of a 25 px box) comes off; 9 characters share the 170 px
        // left, and "14,2" holds the first 4
        let b = words[0].bbox;
        let x1 = 105.0 + 170.0 * 4.0 / 9.0;
        assert!((b[0] - 105.0 * k).abs() < 1e-6 && (b[2] - x1 * k).abs() < 1e-6, "{b:?}");
        assert!(
            (b[1] - 205.0 * k).abs() < 1e-6 && (b[3] - 220.0 * k).abs() < 1e-6,
            "{b:?}"
        );
        assert!((words[0].conf - 0.97).abs() < 1e-6);
    }

    #[test]
    fn character_boxes_place_the_words() {
        let mut l = line("ab cd", 100.0, 200.0, 100.0, 0.0);
        l.chars = Some(vec![
            [100.0, 200.0, 110.0, 225.0],
            [110.0, 200.0, 120.0, 225.0],
            [120.0, 200.0, 160.0, 225.0],
            [160.0, 200.0, 170.0, 225.0],
            [170.0, 200.0, 200.0, 225.0],
        ]);
        let words = words_from_lines(&[l], 1000, 1400, 72.0);
        // the outer character boxes lose the detector's margin
        assert_eq!(words[0].bbox[0], 105.0);
        assert_eq!(words[0].bbox[2], 120.0);
        assert_eq!(words[1].bbox[0], 160.0);
        assert_eq!(words[1].bbox[2], 195.0);
    }

    #[test]
    fn a_tilted_page_comes_back_straight() {
        // one table row: name at the left, value far to the right, tilted 2 degrees
        for deg in [2.0f32, -3.5] {
            let lines = [
                line("Hemoglobina", 100.0, 600.0, 200.0, deg),
                line("14,2", 700.0, 600.0, 70.0, deg),
                line("Leucocitos", 100.0, 640.0, 180.0, deg),
                line("5,8", 700.0, 640.0, 50.0, deg),
            ];
            assert!((skew(&lines).to_degrees() - f64::from(deg)).abs() < 0.01);
            let words = words_from_lines(&lines, 1000, 1400, 72.0);
            let top = |t: &str| {
                words
                    .iter()
                    .find(|w| w.text == t)
                    .map(|w| w.bbox[1])
                    .unwrap_or(f64::NAN)
            };
            // on the tilted image the two ends of a row are 21 px apart; back on the page they line up
            assert!((top("Hemoglobina") - top("14,2")).abs() < 0.5, "{deg}: {words:?}");
            assert!((top("Leucocitos") - top("5,8")).abs() < 0.5, "{deg}: {words:?}");
            assert!((top("Leucocitos") - top("Hemoglobina") - 40.0).abs() < 0.5);
        }
    }

    #[test]
    fn the_skew_ignores_short_boxes_and_folds_vertical_edges() {
        assert_eq!(skew(&[]), 0.0);
        // a square box (a lone "H" flag) says nothing about the skew
        let square = Line {
            quad: [[0.0, 0.0], [25.0, 5.0], [20.0, 30.0], [-5.0, 25.0]],
            text: "H".into(),
            conf: 0.9,
            chars: None,
        };
        assert_eq!(skew(std::slice::from_ref(&square)), 0.0);
        // corners listed from the bottom-left: the same 2-degree line
        let mut l = line("Hemoglobina", 100.0, 600.0, 200.0, 2.0);
        l.quad.rotate_left(3);
        assert!((skew(&[l]).to_degrees() - 2.0).abs() < 0.01);
        // a scan tilted 0.7 degrees: the detector boxes its short cells axis-aligned; the long header
        // lines carry the real skew
        let mut lines = vec![
            line("Informe de resultados de laboratorio", 100.0, 160.0, 460.0, -0.7),
            line("Resultado Unidades Valores de referencia", 460.0, 380.0, 500.0, -0.7),
        ];
        for i in 0..12 {
            lines.push(line("Hemoglobina", 100.0, 430.0 + 40.0 * i as f32, 90.0, 0.0));
        }
        assert!(
            (skew(&lines).to_degrees() + 0.7).abs() < 0.01,
            "{}",
            skew(&lines).to_degrees()
        );
    }

    #[test]
    fn load_without_models_names_what_is_missing() {
        let dir = std::env::temp_dir().join("ownhealth-ocr-no-models");
        let err = load(&dir).expect_err("no models there");
        if cfg!(feature = "engine") {
            assert!(matches!(&err, OcrError::Model(m) if m.contains(DET_MODEL)), "{err}");
        } else {
            assert!(matches!(err, OcrError::EngineMissing), "{err}");
        }
    }
}
