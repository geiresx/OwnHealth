//! `fixtures/labslips/injection/`: the registry the inbox lanes (A email, B Paperless, C OCR) and the MCP
//! lane (F) share. Every fixture in the registry has its files, its marker and its truth; every file in
//! the folder belongs to a fixture; and the instruction each one plants is one the injection detector
//! flags with exactly the codes its truth expects, while the clean parts stay clean.

use std::collections::BTreeSet;
use std::path::PathBuf;

use ownhealth_ingest_pdf::textlayer::injection_hits;
use serde_json::Value;

type R = anyhow::Result<()>;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/labslips/injection")
}

fn read(name: &str) -> anyhow::Result<String> {
    Ok(std::fs::read_to_string(dir().join(name))?)
}

fn json(name: &str) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&read(name)?)?)
}

fn sorted(v: &[String]) -> Vec<String> {
    v.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect()
}

fn expected(truth: &Value) -> Vec<String> {
    let v: Vec<String> = truth["expect"]["possible_injection"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_string))
        .collect();
    sorted(&v)
}

#[test]
fn every_fixture_has_its_files_and_every_file_a_fixture() -> R {
    let registry = json("registry.json")?;
    let marker = registry["marker"].as_str().unwrap_or_default();
    assert_eq!(marker, "CANARY");
    let mut listed: BTreeSet<String> = ["registry.json", "README.md"].iter().map(|s| s.to_string()).collect();
    let fixtures = registry["fixtures"].as_array().cloned().unwrap_or_default();
    assert_eq!(fixtures.len(), 7);
    for f in &fixtures {
        let name = f["name"].as_str().unwrap_or_default();
        assert!(
            !f["lanes"].as_array().is_none_or(Vec::is_empty),
            "{name}: no lane reads it"
        );
        let files: Vec<String> = f["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .chain([format!("{name}.marker"), format!("{name}.truth")])
            .collect();
        for file in files {
            let meta = std::fs::metadata(dir().join(&file));
            assert!(meta.is_ok_and(|m| m.len() > 0), "{name}: {file} missing or empty");
            listed.insert(file);
        }
        assert_eq!(read(&format!("{name}.marker"))?.trim(), marker, "{name}");
        let truth = json(&format!("{name}.truth"))?;
        assert_eq!(truth["slip"], name);
        assert_eq!(truth["lab"], "LabNorte", "{name}: synthetic lab only");
        assert!(truth["expect"].is_object(), "{name}: no expectation");
    }
    let on_disk: BTreeSet<String> = std::fs::read_dir(dir())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    assert_eq!(on_disk, listed, "files without a fixture, or fixtures without files");
    Ok(())
}

/// The plain-text and HTML parts of a MIME message, by their content type line.
fn mime_part<'a>(eml: &'a str, content_type: &str) -> &'a str {
    eml.split_once(&format!("Content-Type: {content_type}"))
        .and_then(|(_, rest)| rest.split_once("\r\n\r\n"))
        .and_then(|(_, body)| body.split_once("\r\n--oh-"))
        .map_or("", |(body, _)| body)
}

#[test]
fn email_body_plants_the_instruction_and_the_attachment_is_clean() -> R {
    let eml = read("inj-email-body.eml")?;
    assert!(eml.contains("\r\n"), "an RFC 5322 message ends its lines with CRLF");
    for header in [
        "From: LabNorte Resultados <resultados@labnorte.example>\r\n",
        "Message-ID: <inj-email-body-1@labnorte.example>\r\n",
        "Authentication-Results: mx.rivera.example;\r\n dkim=pass",
        "filename=\"resultados-INJ-EML-1.pdf\"",
    ] {
        assert!(eml.contains(header), "{header}");
    }
    let plain = mime_part(&eml, "text/plain");
    let html = mime_part(&eml, "text/html");
    assert!(plain.contains("CANARY") && html.contains("CANARY"), "{plain}\n{html}");
    let mut hits = injection_hits(plain);
    hits.extend(injection_hits(html));
    let truth = json("inj-email-body.truth")?;
    assert_eq!(sorted(&hits), expected(&truth));
    assert_eq!(truth["expect"]["sender"], "resultados@labnorte.example");
    // the attachment is the last part, base64; the marker is only in the body
    let attachment = eml
        .rsplit_once("Content-Transfer-Encoding: base64\r\n\r\n")
        .map_or("", |(_, a)| a);
    assert!(
        attachment.starts_with("JVBERi0"),
        "a PDF: {}",
        &attachment[..attachment.len().min(20)]
    );
    Ok(())
}

#[test]
fn paperless_note_plants_the_instruction_and_the_document_is_clean() -> R {
    let doc = json("inj-paperless-note.document.json")?;
    assert_eq!(doc["id"], 4127);
    let notes: Vec<&str> = doc["notes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["note"].as_str())
        .collect();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("CANARY"));
    let truth = json("inj-paperless-note.truth")?;
    assert_eq!(sorted(&injection_hits(notes[0])), expected(&truth));
    for field in ["title", "content", "original_file_name"] {
        let v = doc[field].as_str().unwrap_or_default();
        assert!(!v.contains("CANARY") && injection_hits(v).is_empty(), "{field}: {v}");
    }
    let pdf = std::fs::read(dir().join("inj-paperless-note.pdf"))?;
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(!pdf.windows(6).any(|w| w == b"CANARY"), "the original is clean");
    assert!(pdf.windows(9).any(|w| w == b"INJ-PPL-1"));
    Ok(())
}

/// Width and height from a baseline JPEG's SOF0 marker.
fn jpeg_size(b: &[u8]) -> Option<(u16, u16)> {
    let at = b.windows(2).position(|w| w == [0xFF, 0xC0])?;
    let h = u16::from_be_bytes([*b.get(at + 5)?, *b.get(at + 6)?]);
    let w = u16::from_be_bytes([*b.get(at + 7)?, *b.get(at + 8)?]);
    Some((w, h))
}

#[test]
fn ocr_only_page_is_an_image_whose_printed_instruction_is_flagged() -> R {
    let jpg = std::fs::read(dir().join("inj-ocr-only.jpg"))?;
    assert!(jpg.starts_with(&[0xFF, 0xD8, 0xFF]), "a JPEG");
    assert!(
        !jpg.windows(6).any(|w| w == b"CANARY"),
        "no text layer: the marker is pixels only"
    );
    let truth = json("inj-ocr-only.truth")?;
    let image = &truth["expect"]["image"];
    assert_eq!(
        jpeg_size(&jpg).map(|(w, h)| (i64::from(w), i64::from(h))),
        image["width"].as_i64().zip(image["height"].as_i64())
    );
    let text = truth["expect"]["injection_text"].as_str().unwrap_or_default();
    assert!(text.ends_with("CANARY"));
    assert_eq!(sorted(&injection_hits(text)), expected(&truth));
    assert_eq!(truth["rows"].as_array().map(Vec::len), Some(4));
    Ok(())
}
