//! Table extraction against the synthetic ground truth, and vendor templates (ingestion.md 7, 13).
mod support;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use ownhealth_ingest_pdf::extract::{Extraction, extract};
use ownhealth_ingest_pdf::tables::GENERIC;
use ownhealth_ingest_pdf::templates::{load_templates, match_template, parse_template};
use serde_json::Value;
use support::generate::{self, Layout, Writer};
use support::matching::{Extracted, TruthRow, score_rows};

fn slips() -> HashMap<String, (Vec<u8>, Value)> {
    generate::all_slips()
        .into_iter()
        .map(|(n, pdf, t)| (n, (pdf, t)))
        .collect()
}

fn special(name: &str) -> (Vec<u8>, Value) {
    generate::no_text_layer()
        .into_iter()
        .chain(generate::injection_fixtures())
        .find(|(n, _, _)| n == name)
        .map(|(_, pdf, t)| (pdf, t))
        .expect("a special slip")
}

fn layout(id: &str) -> Layout {
    generate::layouts()
        .into_iter()
        .chain([generate::labnorte_3p()])
        .find(|l| l.id == id)
        .expect("a layout")
}

fn ext(pdf: &[u8]) -> Extraction {
    extract(pdf).expect("an extraction")
}

fn rows(e: &Extraction) -> Vec<Extracted> {
    e.results()
        .iter()
        .map(|(_, r)| Extracted {
            page: i64::from(r.page_no),
            name: r.text("name").into(),
            value: r.text("value").into(),
            unit: r.text("unit").into(),
            bbox: r.bbox().to_vec(),
        })
        .collect()
}

fn truth_rows(t: &Value) -> Vec<TruthRow> {
    t["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(TruthRow::from_json)
        .collect()
}

#[test]
fn eval_set_covers_30_layouts_in_four_languages() {
    let s = slips();
    let layouts: Vec<&Value> = s
        .values()
        .map(|(_, t)| t)
        .filter(|t| {
            let n = t["slip"].as_str().unwrap_or("");
            !n.contains("-amend-") && !n.contains("labnorte-3p")
        })
        .map(|t| &t["layout"])
        .collect();
    assert!(layouts.len() >= 30);
    let mut per_lang: BTreeMap<&str, usize> = BTreeMap::new();
    for l in &layouts {
        *per_lang.entry(l["lang"].as_str().unwrap_or("")).or_default() += 1;
    }
    assert_eq!(per_lang.keys().copied().collect::<Vec<_>>(), ["de", "en", "es", "fr"]);
    assert!(per_lang.values().all(|n| *n >= 6));
    let p3 = &s["es-labnorte-3p"].1;
    assert!(p3["pages"] == 3 && p3["lab"] == "LabNorte" && p3["lang"] == "es");
}

#[test]
fn every_truth_row_is_read_with_its_page_and_bbox() {
    let s = slips();
    let mut names: Vec<String> = ["es", "en", "de", "fr"]
        .iter()
        .flat_map(|lang| {
            [
                "01-basic",
                "02-asterisk",
                "03-bold-sections",
                "04-wrapped",
                "05-two-column",
                "06-multipage",
                "07-unit-in-value",
                "08-range-first",
            ]
            .map(|n| format!("{lang}-{n}"))
        })
        .collect();
    names.push("es-labnorte-3p".into());
    for name in names {
        let (pdf, truth) = &s[&name];
        let e = ext(pdf);
        let t = truth_rows(truth);
        let sc = score_rows(&t, &rows(&e));
        assert!(
            sc.missing.is_empty() && sc.extra.is_empty(),
            "{name}: missing {:?} extra {:?}",
            sc.missing,
            sc.extra
        );
        assert!(
            sc.tp == t.len() && sc.unit_ok == t.len() && sc.bbox_ok == t.len(),
            "{name}: {sc:?}"
        );
        let pending: Vec<&str> = e.pending().iter().map(|r| r.text("name")).collect();
        let want: Vec<&str> = truth["pending"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert_eq!(pending, want, "{name}");
    }
}

#[test]
fn labnorte_reads_through_its_template_with_flags_split_from_ranges() {
    let s = slips();
    let (pdf, truth) = &s["es-labnorte-3p"];
    let e = ext(pdf);
    assert_eq!(e.template.as_ref().map(|t| t.id.as_str()), Some("es/labnorte"));
    let flags: HashMap<(i64, String), String> = truth_rows(truth)
        .iter()
        .zip(truth["rows"].as_array().into_iter().flatten())
        .map(|(t, v)| ((t.page, t.name.clone()), v["flag"].as_str().unwrap_or("").to_string()))
        .collect();
    let mut flagged = 0;
    for (spec, r) in e.results() {
        assert!(spec.prior == 1.0 && r.conf(spec.prior) == 1.0, "{}", r.text("name"));
        assert_eq!(
            r.text("flag"),
            flags[&(i64::from(r.page_no), r.text("name").to_string())]
        );
        assert!(!r.text("ref_range").split_whitespace().any(|w| w == r.text("flag")));
        flagged += usize::from(!r.text("flag").is_empty());
    }
    assert!(flagged >= 3);
    let urine: BTreeSet<&str> = e
        .results()
        .iter()
        .filter(|(_, r)| r.specimen.as_deref() == Some("urine"))
        .map(|(_, r)| r.text("name"))
        .collect();
    assert_eq!(urine, BTreeSet::from(["Glucosa en orina", "Proteínas en orina"]));
}

#[test]
fn wrapped_names_are_joined_and_cost_confidence() {
    let e = ext(&slips()["en-04-wrapped"].0);
    let results = e.results();
    let wrapped: Vec<_> = results
        .iter()
        .filter(|(_, r)| r.cells.get("name").is_some_and(|c| c.conf < 1.0))
        .collect();
    assert!(!wrapped.is_empty());
    assert!(
        wrapped
            .iter()
            .all(|(_, r)| r.text("name").split_whitespace().count() >= 2)
    );
    // generic rules + a join: review, not auto
    assert!(wrapped.iter().all(|(_, r)| r.conf(GENERIC.prior) < 0.95));
}

#[test]
fn space_thousands_stay_one_value_and_go_to_review() {
    let e = ext(&slips()["fr-07-unit-in-value"].0);
    let results = e.results();
    let plt = results
        .iter()
        .find(|(_, r)| r.text("name") == "Plaquettes")
        .expect("platelets")
        .1;
    assert_eq!((plt.text("value"), plt.text("unit")), ("353 000", "/µL"));
    assert!(plt.cells.get("value").is_some_and(|c| c.conf < 0.95));
}

#[test]
fn glued_asterisk_flag_is_split_from_value_and_unit() {
    let e = ext(&slips()["fr-07-unit-in-value"].0);
    let results = e.results();
    let hb = results
        .iter()
        .find(|(_, r)| r.text("name") == "Hémoglobine")
        .expect("haemoglobin")
        .1;
    assert_eq!(
        (hb.text("value"), hb.text("unit"), hb.text("flag")),
        ("18,8", "g/dL", "*")
    );
}

#[test]
fn injected_url_row_is_kept_but_never_confident() {
    let e = ext(&special("inj-url-in-table").0);
    let results = e.results();
    let row = results
        .iter()
        .find(|(_, r)| r.text("value").contains("http"))
        .expect("the url row")
        .1;
    assert!(row.conf(1.0) <= 0.5);
}

#[test]
fn hidden_rows_are_never_extracted() {
    let (pdf, truth) = special("inj-white-on-white");
    let e = ext(&pdf);
    assert!(!e.results().iter().any(|(_, r)| r.text("value") == "45"));
    assert_eq!(e.results().len(), truth["rows"].as_array().map_or(0, Vec::len));
}

#[test]
fn no_text_layer_pages_are_pending() {
    for name in ["es-scan", "es-broken-tounicode"] {
        let e = ext(&special(name).0);
        assert_eq!(e.pending_pages, vec![1], "{name}");
        assert!(e.results().is_empty(), "{name}");
    }
}

#[test]
fn template_fingerprint_needs_the_provider_and_its_headers() {
    let tpl = load_templates().expect("templates");
    assert_eq!(tpl.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["es/labnorte"]);
    assert!(
        match_template(
            "LabNorte\nPrueba Resultado Unidades Valores de referencia\nFecha de toma: 1/1/2026",
            tpl
        )
        .0
        .is_some()
    );
    assert!(
        match_template("Example Lab\nTest Result Units Reference range", tpl)
            .0
            .is_none()
    );
    assert!(match_template("LabNorte newsletter", tpl).0.is_none()); // provider named, no table headers
}

#[test]
fn template_without_name_or_value_headers_is_refused() {
    let raw = "id: x\nversion: '1'\nprovider_name: X\nlocale: es\nfingerprint: {all: [x]}\ncolumns: {unit: [u]}";
    assert!(matches!(parse_template(raw), Err(ownhealth_core::Error::Invalid(_))));
}

#[test]
fn a_row_after_a_wide_gap_is_kept_and_a_footer_is_not() {
    let lay = Layout {
        id: "gap".into(),
        ..layout("en-01-basic")
    };
    let late = |w: &mut Writer, _page: usize, y: f64| {
        w.plain(50.0, y + 200.0, "Ferritin");
        w.plain(250.0, y + 200.0, "88");
        w.plain(320.0, y + 200.0, "ng/mL");
        w.plain(400.0, y + 200.0, "30 - 400");
        w.plain(
            50.0,
            y + 260.0,
            "Laboratory director: Dr. Example. Accredited laboratory 0000.",
        );
    };
    let (pdf, truth) = generate::render(&lay, Some(&late));
    let e = ext(&pdf);
    let got: Vec<(String, String)> = e
        .results()
        .iter()
        .map(|(_, r)| (r.text("name").to_string(), r.text("value").to_string()))
        .collect();
    assert_eq!(got.last(), Some(&("Ferritin".to_string(), "88".to_string())));
    assert_eq!(got.len(), truth["rows"].as_array().map_or(0, Vec::len) + 1);
}
