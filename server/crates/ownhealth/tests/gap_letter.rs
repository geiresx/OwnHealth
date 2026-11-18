//! The access-request letter of a document gap (`ownhealth_ops::letter_pdf`), read back the way a
//! reader would (PDFium): the Spanish letter in es and en is byte for byte the committed golden, its
//! text cites each article with its URL and is information, not advice, and the identity number and
//! the address are labels over blank lines, never values.

use std::path::PathBuf;

use ownhealth_ingest_pdf::textcheck::assert_text_layer;
use ownhealth_ops::letter_pdf::{LetterIn, render};
use ownhealth_rules::gaps;
use ownhealth_rules::wording::imperative_phrases;

type R = anyhow::Result<()>;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn letter(id: &str, lang: &str) -> anyhow::Result<Vec<u8>> {
    letter_as(id, lang, "Alex Rivera", false)
}

fn letter_as(id: &str, lang: &str, name: &str, on_behalf: bool) -> anyhow::Result<Vec<u8>> {
    let g = gaps::load()?;
    let l = g.letter(id).ok_or_else(|| anyhow::anyhow!("no letter {id}"))?;
    Ok(render(
        g,
        l,
        &LetterIn {
            lang,
            name,
            provider: "Traumatología · Hospital Norte",
            date: "2025-11-10",
            on_behalf,
        },
    ))
}

fn lines(pdf: &[u8]) -> Vec<String> {
    assert_text_layer(pdf, |line| match imperative_phrases(line).first() {
        Some(hit) => Err(format!("reads as advice: {hit}")),
        None => Ok(()),
    })
}

/// `OWNHEALTH_RECORD_LETTERS=1` writes the goldens; re-record only on purpose.
#[test]
fn the_spanish_letter_is_byte_for_byte_its_golden_in_es_and_en() -> R {
    for lang in gaps::LANGS {
        let pdf = letter("spain", lang)?;
        assert_eq!(pdf, letter("spain", lang)?, "{lang}: two renders differ");
        let golden = PathBuf::from(FIXTURES).join(format!("gap_letter_spain_{lang}.pdf"));
        if std::env::var_os("OWNHEALTH_RECORD_LETTERS").is_some() {
            std::fs::create_dir_all(FIXTURES)?;
            std::fs::write(&golden, &pdf)?;
        }
        assert!(
            std::fs::read(&golden)? == pdf,
            "{lang}: the letter moved from {}; re-record only on purpose",
            golden.display()
        );
    }
    Ok(())
}

#[test]
fn the_letter_names_the_patient_provider_and_day_and_cites_each_article() -> R {
    for (lang, title, id_label, address, gdpr, ley) in [
        (
            "es",
            "Solicitud de copia de mi historia clínica",
            "Número del documento de identidad:",
            "Domicilio:",
            "artículo 15",
            "artículo 18",
        ),
        (
            "en",
            "Request for a copy of my health records",
            "Identity document number:",
            "Postal address:",
            "Article 15",
            "Article 18",
        ),
    ] {
        let text = lines(&letter("spain", lang)?);
        let all = text.join("\n");
        assert_eq!(text[0], title, "{lang}: {text:#?}");
        for want in ["Alex Rivera", "Traumatología · Hospital Norte", "2025-11-10", gdpr, ley] {
            assert!(all.contains(want), "{lang}: no {want:?} in {text:#?}");
        }
        // each law with its URL, on its own line
        for url in [
            "https://eur-lex.europa.eu/eli/reg/2016/679/oj#art_15",
            "https://eur-lex.europa.eu/eli/reg/2016/679/oj#art_12",
            "https://www.boe.es/buscar/act.php?id=BOE-A-2002-22188#a18",
        ] {
            assert!(text.iter().any(|l| l.ends_with(url)), "{lang}: {url} in {text:#?}");
        }
        // the identity number and the address are written by hand: their lines hold the label only
        for label in [id_label, address] {
            assert!(
                text.iter().any(|l| l.trim() == label),
                "{lang}: {label:?} is not a blank line: {text:#?}"
            );
        }
    }
    // the letter for the rest of the EU and EEA cites the GDPR only
    let eu = lines(&letter("eu", "en")?).join("\n");
    assert!(
        eu.contains("Article 15") && eu.contains("oj#art_12") && !eu.contains("41/2002") && !eu.contains("boe.es"),
        "{eu}"
    );
    Ok(())
}

/// A minor's letter is their legal representative's request: the requester's name is a blank line,
/// the child is the person it is made for, and nothing reads as the child asking about "my visit".
#[test]
fn a_minors_letter_is_the_legal_representatives_request() -> R {
    for (lang, title, requester, behalf, own) in [
        (
            "es",
            "Solicitud de copia de la historia clínica",
            "Solicitante (representante legal):",
            "En nombre de: Lucía Rivera",
            "mi visita",
        ),
        (
            "en",
            "Request for a copy of health records",
            "From (legal representative):",
            "On behalf of: Lucía Rivera",
            "my visit",
        ),
    ] {
        for id in ["spain", "eu"] {
            let text = lines(&letter_as(id, lang, "Lucía Rivera", true)?);
            let all = text.join("\n");
            assert_eq!(text[0], title, "{id} {lang}: {text:#?}");
            assert!(text.iter().any(|l| l.trim() == requester), "{id} {lang}: {text:#?}");
            assert!(text.iter().any(|l| l.trim() == behalf), "{id} {lang}: {text:#?}");
            assert!(!all.contains(own), "{id} {lang}: {all}");
            assert!(
                all.contains("Lucía Rivera") && all.contains("2025-11-10"),
                "{id} {lang}: {all}"
            );
        }
    }
    Ok(())
}

/// The check above is not one that cannot fail: a letter whose text gives an instruction is refused.
#[test]
fn a_letter_that_reads_as_advice_fails_the_check() {
    let g = gaps::load().ok().cloned();
    let Some(mut g) = g else {
        panic!("gaps.yaml did not load");
    };
    g.letter_text["format"]["en"] = "Contact the provider by email.".into();
    let Some(l) = g.letter("spain").cloned() else {
        panic!("no spain letter");
    };
    let pdf = render(
        &g,
        &l,
        &LetterIn {
            lang: "en",
            name: "Alex Rivera",
            provider: "Hospital Norte",
            date: "2025-11-10",
            on_behalf: false,
        },
    );
    let refused = std::panic::catch_unwind(|| lines(&pdf));
    assert!(refused.is_err(), "an instruction in the letter passed");
}
