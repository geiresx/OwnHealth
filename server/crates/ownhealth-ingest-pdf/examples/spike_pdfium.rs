//! Spike (b): words with bboxes and font attributes from pdfium, in pdfplumber's coordinates.
//! `cargo run -p ownhealth-ingest-pdf --example spike_pdfium -- <lib dir> <file.pdf> <plumber.json>`
//! prints how many words match pdfplumber's text and the largest bbox difference.

use pdfium_render::prelude::*;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct W {
    text: String,
    x0: f64,
    top: f64,
    x1: f64,
    bottom: f64,
    font: String,
    size: f64,
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (lib, pdf_path, plumber) = (&args[1], &args[2], &args[3]);
    let pdfium = Pdfium::new(Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(
        lib,
    ))?);
    let doc = pdfium.load_pdf_from_file(pdf_path, None)?;
    let page = doc.pages().get(0)?;
    let h = page.height().value as f64;
    let text = page.text()?;
    let mut words: Vec<W> = Vec::new();
    let mut cur: Option<W> = None;
    for ch in text.chars().iter() {
        let Some(c) = ch.unicode_char() else { continue };
        if ch.is_generated().unwrap_or(false) || c.is_whitespace() {
            if let Some(w) = cur.take() {
                words.push(w);
            }
            continue;
        }
        let loose = ch.loose_bounds()?;
        let size = ch.scaled_font_size().value as f64;
        // pdfminer: the box runs from baseline + descent up by one font size, origin top-left.
        // The font's own descent (FPDFFont_GetDescent) at the origin matches pdfminer's AFM metrics.
        let descent = ch
            .text_object()
            .ok()
            .and_then(|o| o.font().descent(ch.scaled_font_size()).ok())
            .map(|d| d.value as f64);
        let bottom = match (ch.origin_y(), descent) {
            (Ok(y), Some(d)) => h - (y.value as f64 + d),
            _ => h - loose.bottom().value as f64,
        };
        let top = bottom - size;
        let (x0, x1) = (loose.left().value as f64, loose.right().value as f64);
        let font = ch.font_name();
        let gap_ok = cur
            .as_ref()
            .is_some_and(|w| (x0 - w.x1).abs() <= 1.5 && (top - w.top).abs() <= 3.0);
        if gap_ok {
            let w = cur.as_mut().expect("checked");
            w.text.push(c);
            w.x1 = x1;
        } else {
            if let Some(w) = cur.take() {
                words.push(w);
            }
            cur = Some(W {
                text: c.to_string(),
                x0,
                top,
                x1,
                bottom,
                font,
                size,
            });
        }
    }
    if let Some(w) = cur.take() {
        words.push(w);
    }
    let reference: Value = serde_json::from_str(&std::fs::read_to_string(plumber)?)?;
    let theirs = reference["words"].as_array().cloned().unwrap_or_default();
    let mut matched = 0;
    let mut max_d = 0.0f64;
    let mut font_mismatch = 0;
    let mut size_mismatch = 0;
    let mut text_mismatch = Vec::new();
    for (i, t) in theirs.iter().enumerate() {
        let Some(m) = words.get(i) else { break };
        if t["text"].as_str() != Some(m.text.as_str()) {
            text_mismatch.push(json!({"i": i, "plumber": t["text"], "pdfium": m.text}));
            continue;
        }
        matched += 1;
        for (k, v) in [("x0", m.x0), ("top", m.top), ("x1", m.x1), ("bottom", m.bottom)] {
            max_d = max_d.max((t[k].as_f64().unwrap_or(0.0) - r2(v)).abs());
        }
        if t["font"].as_str() != Some(m.font.as_str()) {
            font_mismatch += 1;
        }
        if (t["size"].as_f64().unwrap_or(0.0) - r2(m.size)).abs() > 0.01 {
            size_mismatch += 1;
        }
    }
    let sample: Vec<Value> = words
        .iter()
        .take(4)
        .map(|w| json!({"text": w.text, "x0": r2(w.x0), "top": r2(w.top), "x1": r2(w.x1), "bottom": r2(w.bottom), "font": w.font, "size": r2(w.size)}))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "pdfium_words": words.len(), "plumber_words": theirs.len(), "same_text_in_order": matched,
            "max_bbox_delta_pt": r2(max_d), "font_mismatch": font_mismatch, "size_mismatch": size_mismatch,
            "text_mismatch_first": text_mismatch.iter().take(5).collect::<Vec<_>>(), "sample": sample
        }))?
    );
    Ok(())
}
