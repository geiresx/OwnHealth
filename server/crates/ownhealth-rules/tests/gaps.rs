//! The document-gap rules file (`data/timeline/gaps.yaml`) loads through the cited loader: every row
//! and every law a letter names carries an https cite, every template has es and en text, and a
//! broken file is refused with the reason.

use std::path::Path;

use ownhealth_rules::gaps::{self, RULES, build};

type R = anyhow::Result<()>;

const FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data/timeline/gaps.yaml");

#[test]
fn the_shipped_file_loads_with_a_cite_for_every_row_and_law() -> R {
    let g = gaps::load_path(Path::new(FILE))?;
    assert_eq!(g.version, "gaps 0.1.0");
    assert_eq!((g.window_days, g.min_days), (30, 365));
    assert_eq!(g.rows.iter().map(|(id, _, _)| id.as_str()).collect::<Vec<_>>(), RULES);
    for (id, _, cite) in &g.rows {
        assert!(
            cite.starts_with("https://eur-lex.europa.eu/") || cite.starts_with("https://www.boe.es/"),
            "{id}"
        );
    }
    let spain = g.letter_for(Some("ES")).map(|l| l.id.as_str());
    assert_eq!(spain, Some("spain"));
    assert_eq!(g.letter_for(Some("DE")).map(|l| l.id.as_str()), Some("eu"));
    assert!(g.letter_for(Some("US")).is_none() && g.letter_for(None).is_none());
    let laws: Vec<&str> = g
        .letter("spain")
        .into_iter()
        .flat_map(|l| &l.laws)
        .map(|l| l.cite.as_str())
        .collect();
    assert_eq!(
        laws,
        [
            "https://eur-lex.europa.eu/eli/reg/2016/679/oj#art_15",
            "https://www.boe.es/buscar/act.php?id=BOE-A-2002-22188#a18"
        ]
    );
    // the compiled-in copy is the same file
    assert_eq!(gaps::load()?.version, g.version);
    Ok(())
}

#[test]
fn a_broken_file_is_refused_with_its_reason() -> R {
    let good = std::fs::read_to_string(FILE)?;
    let hole_cite =
        "    min_days: 365\n    source: GDPR Art. 15\n    cite: https://eur-lex.europa.eu/eli/reg/2016/679/oj#art_15\n";
    let cases: [(&str, String, &str); 8] = [
        (
            "a row without a cite",
            good.replace(hole_cite, "    min_days: 365\n    source: GDPR Art. 15\n"),
            "no https `cite`",
        ),
        (
            "an http cite",
            good.replacen("    cite: https://eur-lex", "    cite: http://eur-lex", 1),
            "no https `cite`",
        ),
        (
            "a law without a cite",
            good.replacen(
                "        cite: https://www.boe.es/buscar/act.php?id=BOE-A-2002-22188#a18\n",
                "",
                1,
            ),
            "without an https `cite`",
        ),
        (
            "a template without es",
            good.replacen("      es: \"Sin registros entre {from} y {to}\"\n", "", 1),
            "no `es` text",
        ),
        (
            "a template that reads as advice",
            good.replacen(
                "Sin registros entre {from} y {to}",
                "Pida los informes entre {from} y {to}",
                1,
            ),
            "reads as advice",
        ),
        (
            "a rule the code evaluates is missing",
            good.replacen("  - id: records_hole\n", "  - id: records_gap\n", 1),
            "no row `records_hole`",
        ),
        (
            "no window",
            good.replacen("    window_days: 30\n", "", 1),
            "`window_days` is not a positive number",
        ),
        (
            "a letter without its basis",
            good.replacen("    basis:\n", "    basis_text:\n", 1),
            "no `basis`",
        ),
    ];
    for (what, body, reason) in cases {
        assert_ne!(body, good, "{what}: the mutation did not apply");
        match build(body.as_bytes()) {
            Ok(_) => panic!("{what}: loaded"),
            Err(e) => assert!(e.to_string().contains(reason), "{what}: {e}"),
        }
    }
    Ok(())
}
