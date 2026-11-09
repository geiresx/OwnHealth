//! Which pages need OCR (lane C, ingestion.md 6). The text layer comes first. A page goes to OCR when
//! it has no usable text layer: fewer than 150 characters, under half of its 3+ letter tokens
//! word-like, `(cid:NN)` runs, or 5% or more Private Use Area or replacement characters. Every image
//! goes to OCR too. A page with a good text layer never does.

use ownhealth_ingest_pdf::textlayer::{MAX_PUA, MIN_CHARS, MIN_WORDLIKE, wordlike_share};

/// True when a page with this text (and this text-layer verdict from `import.pdf`) must be read by OCR.
pub fn needs_ocr(text_layer_ok: bool, text: &str) -> bool {
    !text_layer_ok || garbage(text)
}

/// The text a text layer gave cannot be read as words.
fn garbage(text: &str) -> bool {
    let joined: String = text.split_whitespace().collect();
    let n = joined.chars().count();
    if n < MIN_CHARS || joined.contains("(cid:") {
        return true;
    }
    let odd = joined
        .chars()
        .filter(|c| ('\u{E000}'..='\u{F8FF}').contains(c) || *c == '\u{FFFD}')
        .count();
    if odd as f64 / n as f64 >= MAX_PUA {
        return true;
    }
    let tokens: Vec<&str> = text.split_whitespace().collect();
    wordlike_share(&tokens) < MIN_WORDLIKE
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLIP: &str = "LabNorte Informe de resultados de laboratorio Paciente: RIVERA, ALEX \
        Fecha de toma: 14/09/2026 08:15 Prueba Resultado Unidades Valores de referencia \
        Hemoglobina 14,2 g/dL 13,5 - 17,5 Leucocitos 5,8 x10^3/µL 4,0 - 11,0 Plaquetas 262 \
        Glucosa 102 mg/dL 70 - 110 Creatinina 0,62 mg/dL";

    #[test]
    fn a_good_text_layer_stays_first() {
        assert!(!needs_ocr(true, SLIP));
    }

    #[test]
    fn no_text_layer_or_an_image_goes_to_ocr() {
        assert!(needs_ocr(false, SLIP));
        assert!(needs_ocr(false, ""));
    }

    #[test]
    fn garbage_glyphs_go_to_ocr_even_when_the_layer_claims_to_be_fine() {
        // too short: a scan with a one-line stamp as its only text
        assert!(needs_ocr(true, "Escaneado 14/09/2026"));
        // (cid:NN) runs: a font without a ToUnicode map
        let cid = SLIP.replace("Hemoglobina", "(cid:41)(cid:72)(cid:80)");
        assert!(needs_ocr(true, &cid));
        // Private Use Area glyphs
        let pua: String = SLIP
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() && c < 'h' {
                    '\u{E012}'
                } else {
                    c
                }
            })
            .collect();
        assert!(needs_ocr(true, &pua));
        // glyph ids read as letters: no vowels, long consonant runs
        let ids = "Xqzt Brkkt Fghjkl Mnbvc Qwrtp Zxcvb Lkjhg Pqrst Vwxyz Bcdfg ".repeat(4);
        assert!(needs_ocr(true, &ids));
    }
}
