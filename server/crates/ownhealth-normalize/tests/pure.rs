//! The Python tests of `normalize/` that need no database (tests/normalize/test_norm_values.py,
//! test_norm_units.py, test_norm_tables.py, test_norm_molar.py and the table-only cases of
//! test_norm_resolve.py), with the same cases and expectations. Hypothesis tests are proptest here.

use std::str::FromStr;

use ownhealth_normalize::{
    self as n, RefRange, Resolution, ResolveQuery, convert, interpret, parse_range, parse_value, pick_range, resolve,
    tables, to_ucum, units,
};
use proptest::prelude::*;
use rust_decimal::Decimal;
use serde_json::json;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn ranges(printed: &str, locale: &str, unit: Option<&str>) -> Vec<RefRange> {
    parse_range(printed, locale, unit).unwrap_or_else(|e| panic!("{e}"))
}

fn one(printed: &str, locale: &str, unit: Option<&str>) -> RefRange {
    let mut r = ranges(printed, locale, unit);
    assert_eq!(r.len(), 1, "{printed}: {r:?}");
    r.remove(0)
}

fn conv(v: Decimal, from: &str, to: &str, group: &str) -> Option<Decimal> {
    convert(v, from, to, group).unwrap_or_else(|e| panic!("{e}"))
}

// ------------------------------------------------------------ test_norm_values ----

#[test]
fn test_parse_value() {
    type Case<'a> = (
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );
    let cases: [Case; 25] = [
        ("13,5", "es", None, Some("13.5"), None, None, None),
        ("13.5", "es", None, Some("13.5"), None, None, None),
        ("13.5", "en", None, Some("13.5"), None, None, None),
        ("1.234,5", "de", None, Some("1234.5"), None, None, None),
        ("1,234.5", "en", None, Some("1234.5"), None, None, None),
        ("1,200", "en", Some("mg/dL"), Some("1200"), None, None, None),
        ("6.800", "es", Some("/mm3"), Some("6800"), None, None, None),
        ("6.800", "es", Some("x10^3/µL"), Some("6800"), None, None, None),
        (
            "1.200",
            "es",
            Some("mg/dL"),
            Some("1.200"),
            None,
            None,
            Some("ambiguous_separator"),
        ),
        ("250.000", "fr", Some("/µL"), Some("250000"), None, None, None),
        ("1.234.567", "es", None, Some("1234567"), None, None, None),
        ("1,2,3", "es", None, None, None, None, Some("suspect_separator")),
        ("< 0,5", "es", None, Some("0.5"), Some("<"), None, None),
        ("≤5", "en", None, Some("5"), Some("<="), None, None),
        ("inferior a 5", "es", None, Some("5"), Some("<"), None, None),
        ("kleiner als 0,1", "de", None, Some("0.1"), Some("<"), None, None),
        ("> 90", "es", None, Some("90"), Some(">"), None, None),
        ("163 H", "es", None, Some("163"), None, None, None),
        ("24 *", "es", None, Some("24"), None, None, None),
        ("Negativo", "es", None, None, None, Some("negative"), None),
        ("nicht nachweisbar", "de", None, None, None, Some("not_detected"), None),
        ("Négatif", "fr", None, None, None, Some("negative"), None),
        ("Trazas", "es", None, None, None, Some("trace"), None),
        ("+++", "es", None, None, None, Some("positive"), None),
        ("ver nota", "es", None, None, None, None, None),
    ];
    for (printed, locale, unit, value, cmp, code, reason) in cases {
        let pv = parse_value(printed, locale, unit).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (
                pv.value,
                pv.comparator.as_deref(),
                pv.text_code.as_deref(),
                pv.reason.as_deref()
            ),
            (value.map(d), cmp, code, reason),
            "{printed} {locale}"
        );
        assert_eq!(pv.printed, printed); // the printed string is kept as found
    }
    // "1.200" keeps its printed scale, as Decimal("1.200") does
    let pv = parse_value("1.200", "es", Some("mg/dL")).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(pv.value.map(|v| v.to_string()).as_deref(), Some("1.200"));
}

/// `f"{v:,.2f}"` in the locale's separators.
fn fmt(cents: u64, locale: &str) -> String {
    let int = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let s = format!("{grouped}.{:02}", cents % 100);
    if locale == "en" {
        s
    } else {
        s.replace(',', "_").replace('.', ",").replace('_', ".")
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]
    /// Any number a lab prints with two decimals reads back exactly in its own locale.
    #[test]
    fn test_locale_round_trip(cents in 0u64..=999_999_900, locale in prop::sample::select(vec!["es", "en", "de", "fr"])) {
        let v = Decimal::new(cents as i64, 2);
        let pv = parse_value(&fmt(cents, locale), locale, None).unwrap_or_else(|e| panic!("{e}"));
        prop_assert_eq!(pv.value, Some(v));
        prop_assert_eq!(pv.reason, None);
    }
}

#[test]
fn test_ranges() {
    let r = one("13.5 - 17.5", "es", None);
    assert_eq!(
        (r.low, r.high, r.text.as_str()),
        (Some(d("13.5")), Some(d("17.5")), "13.5 - 17.5")
    );
    let r = one("0,70 - 1,20", "es", None);
    assert_eq!((r.low, r.high), (Some(d("0.70")), Some(d("1.20"))));
    let r = one("< 130", "es", None);
    assert_eq!((r.low, r.high, r.high_inclusive), (None, Some(d("130")), false));
    let r = one("Hasta 40", "es", None);
    assert_eq!((r.low, r.high, r.high_inclusive), (None, Some(d("40")), true));
    let r = one("> 90", "es", None);
    assert_eq!((r.low, r.high, r.low_inclusive), (Some(d("90")), None, false));
    let r = one("Negativo", "es", None);
    assert_eq!(r.text_code.as_deref(), Some("negative"));
    let r = one("150.000 - 450.000", "es", Some("/mm3"));
    assert_eq!((r.low, r.high), (Some(d("150000")), Some(d("450000"))));
    assert!(ranges("ver nota", "es", None).is_empty());
}

fn pick<'r>(r: &'r [RefRange], sex: Option<&str>, age: Option<i32>) -> Option<&'r RefRange> {
    pick_range(r, sex, age)
}

#[test]
fn test_conditioned_ranges_and_pick() {
    let rs = ranges("Hombres: 13.5-17.5 Mujeres: 12,0-16,0", "es", None);
    let got: Vec<_> = rs.iter().map(|r| (r.applies_to.clone(), r.low, r.high)).collect();
    assert_eq!(
        got,
        vec![
            (json!({"sex": "male"}), Some(d("13.5")), Some(d("17.5"))),
            (json!({"sex": "female"}), Some(d("12.0")), Some(d("16.0"))),
        ]
    );
    assert_eq!(pick(&rs, Some("female"), Some(40)).and_then(|r| r.low), Some(d("12.0")));
    assert_eq!(pick(&rs, Some("male"), Some(40)).and_then(|r| r.low), Some(d("13.5")));
    assert!(pick(&rs, None, Some(40)).is_none()); // never guess the sex

    let de = ranges("Männer: 0,7-1,2; Frauen: 0,5-0,9", "de", None);
    assert_eq!(pick(&de, Some("female"), Some(30)).and_then(|r| r.high), Some(d("0.9")));

    let ages = ranges("18-49 años: 1.0-2.0; 50-99 años: 2.0-3.0", "es", None);
    assert_eq!(pick(&ages, Some("male"), Some(47)).and_then(|r| r.high), Some(d("2.0")));
    assert_eq!(pick(&ages, Some("male"), Some(71)).and_then(|r| r.high), Some(d("3.0")));
    assert!(pick(&ages, Some("male"), None).is_none());

    let preg = ranges("Mujeres: 12-16; 3er trimestre: 11-14", "es", None);
    // a trimester range is never picked
    assert_eq!(
        pick(&preg, Some("female"), Some(30)).and_then(|r| r.high),
        Some(d("16"))
    );
}

#[test]
fn test_two_equal_ranges_that_disagree_pick_none() {
    let rs = ranges("13-17; 12-16", "es", None);
    assert!(pick(&rs, Some("male"), Some(40)).is_none());
}

#[test]
fn test_interpret_from_the_printed_range_only() {
    let r = one("13.5 - 17.5", "es", None);
    assert_eq!(interpret(Some(d("12")), None, Some(&r), None), Some("L"));
    assert_eq!(interpret(Some(d("13.5")), None, Some(&r), None), Some("N"));
    assert_eq!(interpret(Some(d("18")), None, Some(&r), None), Some("H"));
    assert_eq!(interpret(Some(d("18")), None, None, None), None); // no printed range, no flag
    let lt = one("< 130", "es", None);
    assert_eq!(interpret(Some(d("129")), None, Some(&lt), None), Some("N"));
    assert_eq!(interpret(Some(d("130")), None, Some(&lt), None), Some("H")); // "< 130" excludes 130
    let gt = one("> 90", "es", None);
    assert_eq!(interpret(Some(d("90")), None, Some(&gt), None), Some("L"));
    assert_eq!(interpret(Some(d("0.5")), None, Some(&lt), Some("<")), Some("N"));
    let rr = one("1 - 3", "es", None);
    assert_eq!(interpret(Some(d("0.5")), None, Some(&rr), Some("<")), Some("L"));
    assert_eq!(interpret(Some(d("2")), None, Some(&rr), Some("<")), None); // "<2" could be below 1
    let neg = one("Negativo", "es", None);
    assert_eq!(interpret(None, Some("negative"), Some(&neg), None), Some("N"));
    assert_eq!(interpret(None, Some("not_detected"), Some(&neg), None), Some("N"));
    assert_eq!(interpret(None, Some("positive"), Some(&neg), None), Some("A"));
}

#[test]
fn test_line_breaks_separate_range_entries() {
    let by_age = ranges("0-17 años: 5-10\n18-99 años: 3-8", "es", None);
    let got: Vec<_> = by_age.iter().map(|r| (r.low, r.high)).collect();
    assert_eq!(got, vec![(Some(d("5")), Some(d("10"))), (Some(d("3")), Some(d("8")))]);
    // an adult gets the adult range
    assert_eq!(pick(&by_age, Some("male"), Some(40)).and_then(|r| r.high), Some(d("8")));
    let two = ranges("10-20\n30-40", "es", None);
    assert!(two.len() == 2 && pick(&two, Some("male"), Some(40)).is_none()); // disagreement seen
    let wrapped = ranges("Hombres:\n13-17\nMujeres:\n12-16", "es", None); // a label continues below
    let got: Vec<_> = wrapped.iter().map(|r| (r.applies_to.clone(), r.high)).collect();
    assert_eq!(
        got,
        vec![
            (json!({"sex": "male"}), Some(d("17"))),
            (json!({"sex": "female"}), Some(d("16")))
        ]
    );
    let age = one("0-17 años:\n5-10", "es", None);
    assert!(age.applies_to == json!({"age_min": 0, "age_max": 17}) && age.high == Some(d("10")));
}

#[test]
fn test_a_censored_value_at_an_exclusive_bound_is_inside() {
    let lt = one("< 5", "es", None);
    assert_eq!(interpret(Some(d("5")), None, Some(&lt), Some("<")), Some("N")); // "<5" against "<5": below 5
    assert_eq!(interpret(Some(d("5")), None, Some(&lt), Some("<=")), None); // "<=5" may be 5, which "<5" excludes
    let gt = one("> 60", "es", None);
    assert_eq!(interpret(Some(d("60")), None, Some(&gt), Some(">")), Some("N"));
    assert_eq!(interpret(Some(d("60")), None, Some(&gt), Some(">=")), None);
    let le = one("<= 5", "es", None);
    assert_eq!(interpret(Some(d("5")), None, Some(&le), Some("<=")), Some("N"));
}

// ------------------------------------------------------------- test_norm_units ----

const PRINTED: [(&str, &str); 37] = [
    ("mg/dl", "mg/dL"),
    ("mg%", "mg/dL"),
    ("MG/DL", "mg/dL"),
    ("x10^3/µL", "10*3/uL"),
    ("x10E3/uL", "10*3/uL"),
    ("10^9/L", "10*9/L"),
    ("x 10⁹/l", "10*9/L"),
    ("miles/µL", "10*3/uL"),
    ("/mm3", "/mm3"),
    ("UI/L", "[IU]/L"),
    ("IE/l", "[IU]/L"),
    ("U/l", "U/L"),
    ("mUI/mL", "m[IU]/mL"),
    ("µUI/mL", "u[IU]/mL"),
    ("μIU/mL", "u[IU]/mL"),
    ("mU/L", "mU/L"),
    ("mL/min/1.73m2", "mL/min/{1.73_m2}"),
    ("mL/min/1,73 m²", "mL/min/{1.73_m2}"),
    ("mmol/l", "mmol/L"),
    ("µmol/L", "umol/L"),
    ("mcg/dL", "ug/dL"),
    ("fl", "fL"),
    ("pg", "pg"),
    ("%", "%"),
    ("mmol/mol", "mmol/mol"),
    ("mEq/L", "meq/L"),
    ("mOsm/kg", "mosm/kg"),
    ("seg", "s"),
    ("mm/1ªh", "mm/h"),
    ("mmHg", "mm[Hg]"),
    ("mg/g creat", "mg/g"),
    ("mg/mmol", "mg/mmol"),
    ("g/24h", "g/(24.h)"),
    ("cel/campo", "/[HPF]"),
    ("ng/ml", "ng/mL"),
    ("10*3/uL", "10*3/uL"),
    ("m[IU]/L", "m[IU]/L"),
];

#[test]
fn test_printed_units_map_to_ucum() {
    for (printed, ucum) in PRINTED {
        assert_eq!(to_ucum(printed).as_deref(), Some(ucum), "{printed}");
        assert!(units::parse_ucum(ucum).is_ok(), "{ucum}"); // and the result is valid in our UCUM subset
    }
}

#[test]
fn test_unreadable_units_are_none() {
    for printed in ["", "-", "xyz", "mg/xyz", "ver nota"] {
        assert_eq!(to_ucum(printed), None, "{printed}");
    }
}

#[test]
fn test_parse_rejects_unknown_atoms() {
    assert!(units::parse_ucum("mg/foo").is_err());
}

#[test]
fn test_iu_and_enzyme_units_never_convert() {
    assert_eq!(conv(d("1"), "[IU]/L", "U/L", "alt"), None);
    assert_eq!(conv(d("1"), "m[IU]/L", "mU/L", "tsh"), None);
    assert_ne!(units::dims_of(Some("[IU]/L")), units::dims_of(Some("U/L")));
}

#[test]
fn test_known_factors() {
    assert_eq!(conv(d("1"), "U/L", "ukat/L", "alt"), Some(d("0.0166666666667")));
    assert_eq!(conv(d("6.8"), "10*3/uL", "10*9/L", "wbc"), Some(d("6.8")));
    assert_eq!(conv(d("6800"), "/uL", "10*3/uL", "wbc"), Some(d("6.8")));
    assert_eq!(
        conv(d("1"), "mL/s/{1.73_m2}", "mL/min/{1.73_m2}", "egfr"),
        Some(d("60"))
    );
    assert_eq!(conv(d("30"), "ng/mL", "ug/L", "ferritin"), Some(d("30")));
    assert_eq!(conv(d("2.5"), "m[IU]/L", "u[IU]/mL", "tsh"), Some(d("2.5")));
}

#[test]
fn test_mass_and_molar_need_the_molar_mass() {
    assert_eq!(conv(d("100"), "mg/dL", "mmol/L", "glucose"), Some(d("5.55074490997"))); // 1000/180.156
    // Lp(a): mass and molar measure different things, never converted
    assert_eq!(conv(d("100"), "mg/dL", "nmol/L", "lpa"), None);
    // a group with no molar entry does not borrow one
    assert_eq!(conv(d("100"), "mg/dL", "mmol/L", "ferritin"), None);
}

#[test]
fn test_eq_converts_only_with_valence() {
    assert_eq!(conv(d("140"), "meq/L", "mmol/L", "sodium"), Some(d("140")));
    assert_eq!(conv(d("5"), "meq/L", "mmol/L", "calcium"), Some(d("2.5")));
    assert_eq!(conv(d("5"), "meq/L", "mmol/L", "glucose"), None);
}

#[test]
fn test_hba1c_master_equation_not_a_factor_of_ten() {
    assert_eq!(conv(d("6.5"), "%", "mmol/mol", "hba1c"), Some(d("47.54115"))); // 10.929 x 4.35
    let back = conv(d("48"), "mmol/mol", "%", "hba1c").map(|v| v.round_dp(2));
    assert_eq!(back, Some(d("6.54")));
    // outside hba1c the same pair is a plain scale (and nothing in the tables uses it)
    assert_eq!(conv(d("6.5"), "%", "mmol/mol", "hematocrit"), Some(d("65")));
}

#[test]
fn test_urine_acr_converts_through_the_creatinine_in_the_denominator() {
    // 3 mg/mmol is about 26.5 mg/g (KDIGO: A2 starts at 3 mg/mmol = 30 mg/g, rounded)
    let v = conv(d("3"), "mg/mmol", "mg/g", "urine_acr").map(|v| v.round_dp(1));
    assert_eq!(v, Some(d("26.5")));
}

const SAME_DIMENSION: [(&str, &[&str]); 6] = [
    ("mg/dL", &["g/L", "mg/L", "ug/mL", "ng/mL", "ug/dL", "g/dL", "kg/m3"]),
    ("mmol/L", &["umol/L", "nmol/L", "pmol/L", "mol/m3"]),
    ("10*3/uL", &["10*9/L", "/uL", "/mm3", "10*6/mL"]),
    ("U/L", &["ukat/L", "nkat/L", "kU/L", "mU/mL"]),
    ("m[IU]/mL", &["[IU]/L", "u[IU]/mL", "m[IU]/L"]),
    ("s", &["min", "h", "ms"]),
];

fn pairs() -> Vec<(&'static str, &'static str)> {
    SAME_DIMENSION
        .iter()
        .flat_map(|(a, bs)| bs.iter().map(move |b| (*a, *b)))
        .collect()
}

fn close(a: Decimal, b: Decimal) -> bool {
    (a - b).abs() <= b.abs() * Decimal::new(1, 9) + Decimal::new(1, 12)
}

/// st.decimals(min_value=0.001, max_value=100000, places=3)
fn values() -> impl Strategy<Value = Decimal> {
    (1i64..=100_000_000).prop_map(|m| Decimal::new(m, 3))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn test_round_trip_within_a_dimension(v in values(), pair in prop::sample::select(pairs())) {
        let (a, b) = pair;
        let there = conv(v, a, b, "any");
        prop_assert!(there.is_some());
        let back = there.and_then(|t| conv(t, b, a, "any"));
        prop_assert!(back.is_some_and(|x| close(x, v)), "{v} {a}->{b}: {back:?}");
    }

    #[test]
    fn test_conversion_is_linear(v in values(), k in 2i64..=50, pair in prop::sample::select(pairs())) {
        let (a, b) = pair;
        let k = Decimal::from(k);
        let lhs = conv(v * k, a, b, "any");
        let rhs = conv(v, a, b, "any").map(|x| x * k);
        prop_assert!(matches!((lhs, rhs), (Some(l), Some(r)) if close(l, r)), "{lhs:?} {rhs:?}");
    }

    #[test]
    fn test_hba1c_round_trip(cents in 220i64..=2000) {
        let v = Decimal::new(cents, 2);
        let ifcc = conv(v, "%", "mmol/mol", "hba1c");
        let back = ifcc.and_then(|i| conv(i, "mmol/mol", "%", "hba1c"));
        prop_assert!(back.is_some_and(|x| close(x, v)), "{v}: {back:?}");
    }

    #[test]
    fn test_molar_round_trip_cholesterol(
        a in prop::sample::select(vec!["mg/dL", "g/L", "ug/dL"]),
        b in prop::sample::select(vec!["mmol/L", "umol/L"]),
        v in values(),
    ) {
        let there = conv(v, a, b, "ldl_c");
        prop_assert!(there.is_some());
        let back = there.and_then(|t| conv(t, b, a, "ldl_c"));
        prop_assert!(back.is_some_and(|x| close(x, v)), "{v} {a}->{b}: {back:?}");
    }
}

// ------------------------------------------------------------ test_norm_tables ----

#[test]
fn test_loinc_subset_size_and_notice() {
    let codes = tables::loinc_codes();
    assert!(codes.len() >= 200);
    assert!(codes.values().all(|c| !c.status.is_empty()));
    let notice = include_str!("../../../../data/terminology/loinc/NOTICE.md");
    assert!(notice.contains("Regenstrief") && notice.contains("loinc.org/license"));
}

#[test]
fn test_every_alias_and_code_points_at_a_group() {
    let groups = tables::groups();
    assert!(
        tables::aliases()
            .values()
            .flatten()
            .all(|a| groups.contains_key(&a.alias_group))
    );
    let mut by_group: Vec<_> = tables::codes_by_group().keys().collect();
    let mut all: Vec<_> = groups.keys().collect();
    by_group.sort();
    all.sort();
    assert_eq!(by_group, all);
    assert!(tables::molar().keys().all(|g| groups.contains_key(g)));
    assert!(tables::specimen_twins().values().all(|t| groups.contains_key(t)));
}

#[test]
fn test_every_quantitative_property_has_a_dimension_rule() {
    for c in tables::loinc_codes().values() {
        if matches!(c.scale.as_str(), "Qn" | "SemiQn") {
            assert!(resolve::property_dims(&c.property).is_some(), "{c:?}");
        }
    }
}

#[test]
fn test_group_units_parse_fit_a_code_and_convert() {
    for (group, g) in tables::groups().iter().filter(|(_, g)| !g.canonical.is_empty()) {
        for u in [&g.canonical, &g.si, &g.conventional] {
            assert!(units::parse_ucum(u).is_ok(), "{group} {u}");
            assert!(conv(d("1"), u, &g.canonical, group).is_some(), "{group} {u}");
        }
        assert!(
            tables::codes_by_group()[group.as_str()]
                .iter()
                .filter(|c| c.scale != "Ord")
                .any(|c| resolve::unit_fits(&c.property, Some(&g.canonical))),
            "{group}"
        );
    }
}

#[test]
fn test_every_language_has_aliases_for_the_core_panel() {
    let core = [
        "hemoglobin",
        "glucose",
        "creatinine",
        "ldl_c",
        "hdl_c",
        "triglycerides",
        "tsh",
        "hba1c",
        "ferritin",
    ];
    for lang in tables::LANGS {
        let have: Vec<&str> = tables::aliases()
            .values()
            .flatten()
            .filter(|a| a.lang == lang)
            .map(|a| a.alias_group.as_str())
            .collect();
        for g in core {
            assert!(have.contains(&g), "{lang} misses {g}");
        }
    }
}

#[test]
fn test_derivation_version_names_code_and_tables() {
    let v = n::derivation_version();
    assert!(
        v.starts_with("norm-0.1.0+t") && v.split("+t").nth(1).map(str::len) == Some(8),
        "{v}"
    );
    // a table edit changes the version, so rederive runs
    let before = tables::hash_files(tables::FILES.iter().map(|(p, c)| (*p, c.as_bytes())));
    assert_eq!(before, tables::tables_hash());
    let edited: Vec<(&str, String)> = tables::FILES
        .iter()
        .map(|(p, c)| {
            let c = if *p == "aliases/es.tsv" {
                format!("{c}Ferritina sérica\tferritin\t\tcurated\t2026-11-08\n")
            } else {
                c.to_string()
            };
            (*p, c)
        })
        .collect();
    let after = tables::hash_files(edited.iter().map(|(p, c)| (*p, c.as_bytes())));
    assert_ne!(after, before);
}

/// The embedded list is every `*.tsv` under terminology/, in Python's sorted(rglob) order, and the
/// embedded bytes are the files' bytes: a new or moved table cannot be left out of the hash.
#[test]
fn test_embedded_tables_are_the_shipped_files() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/terminology");
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<Vec<String>>) {
        for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{e}")).flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().is_some_and(|x| x == "tsv") {
                let rel = p.strip_prefix(root).unwrap_or_else(|e| panic!("{e}"));
                out.push(rel.iter().map(|c| c.to_string_lossy().into_owned()).collect());
            }
        }
    }
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    found.sort(); // component by component, as PurePath compares
    let found: Vec<String> = found.iter().map(|parts| parts.join("/")).collect();
    let embedded: Vec<&str> = tables::FILES.iter().map(|(p, _)| *p).collect();
    assert_eq!(found, embedded);
    for (p, c) in tables::FILES {
        assert_eq!(std::fs::read_to_string(root.join(p)).unwrap_or_default(), c, "{p}");
    }
}

// ------------------------------------------------------------- test_norm_molar ----

const PUBLISHED: [(&str, &str, &str, &str); 24] = [
    ("glucose", "mg/dL", "mmol/L", "0.05551"),
    ("total_cholesterol", "mg/dL", "mmol/L", "0.02586"),
    ("ldl_c", "mg/dL", "mmol/L", "0.02586"),
    ("hdl_c", "mg/dL", "mmol/L", "0.02586"),
    ("triglycerides", "mg/dL", "mmol/L", "0.01129"),
    ("creatinine", "mg/dL", "umol/L", "88.40"),
    ("urea", "mg/dL", "mmol/L", "0.1665"),
    ("bun", "mg/dL", "mmol/L", "0.357"),
    ("uric_acid", "mg/dL", "umol/L", "59.48"),
    ("bilirubin_total", "mg/dL", "umol/L", "17.10"),
    ("calcium", "mg/dL", "mmol/L", "0.2495"),
    ("phosphate", "mg/dL", "mmol/L", "0.3229"),
    ("magnesium", "mg/dL", "mmol/L", "0.4114"),
    ("iron", "ug/dL", "umol/L", "0.1791"),
    ("vitamin_d_25oh", "ng/mL", "nmol/L", "2.496"),
    ("vitamin_b12", "pg/mL", "pmol/L", "0.7378"),
    ("folate", "ng/mL", "nmol/L", "2.266"),
    ("testosterone", "ng/dL", "nmol/L", "0.03467"),
    ("cortisol", "ug/dL", "nmol/L", "27.59"),
    ("ft4", "ng/dL", "pmol/L", "12.87"),
    ("ft3", "pg/mL", "pmol/L", "1.536"),
    ("estradiol", "pg/mL", "pmol/L", "3.671"),
    ("progesterone", "ng/mL", "nmol/L", "3.180"),
    ("dhea_s", "ug/dL", "umol/L", "0.02714"),
];

#[test]
fn test_published_factor() {
    for (group, src, dst, factor) in PUBLISHED {
        let got = conv(d("1"), src, dst, group).unwrap_or_else(|| panic!("{group}"));
        let f = d(factor);
        assert!((got - f).abs() / f < d("0.001"), "{group} {got} {factor}");
    }
}

#[test]
fn test_every_row_is_cited_and_converts() {
    for (group, m) in tables::molar() {
        assert!(m.citation.starts_with("https://"), "{group}");
        assert!(m.g_per_mol.is_some() || m.valence.is_some(), "{group}");
        if let Some(mm) = &m.g_per_mol {
            let got = conv(d("1"), "g/L", "mol/L", group).unwrap_or_else(|| panic!("{group}"));
            let mm = Decimal::from_str(&mm.numer().to_string()).unwrap_or_default()
                / Decimal::from_str(&mm.denom().to_string()).unwrap_or_default();
            assert!((got * mm - Decimal::ONE).abs() < Decimal::new(1, 9), "{group}");
        }
    }
}

#[test]
fn test_bun_is_not_urea() {
    // Urea and urea nitrogen differ by 60.06 / 28.01 = 2.14: the trap the table names.
    let urea = conv(d("1"), "mg/dL", "mmol/L", "urea").unwrap_or_default();
    let bun = conv(d("1"), "mg/dL", "mmol/L", "bun").unwrap_or_default();
    assert_eq!((bun / urea).round_dp(2), d("2.14"));
}

#[test]
fn test_phosphate_is_phosphorus_not_po4() {
    let v = conv(d("1"), "mg/dL", "mmol/L", "phosphate").map(|v| v.round_dp(4));
    assert_eq!(v, Some(d("0.3229")));
}

// ---------------------------------------------- test_norm_resolve (no database) ----

async fn res(name: &str, lang: &str, unit: Option<&str>, specimen: Option<&str>) -> Resolution {
    let q = ResolveQuery {
        lang,
        unit,
        specimen,
        ..Default::default()
    };
    n::resolve_analyte(None, name, &q)
        .await
        .unwrap_or_else(|e| panic!("{e}"))
}

#[tokio::test]
async fn test_resolve() {
    type Case<'a> = (&'a str, &'a str, Option<&'a str>, Option<&'a str>, &'a str, &'a str);
    let cases: [Case; 32] = [
        ("Colesterol LDL", "es", Some("mg/dL"), None, "ldl_c", "13457-7"),
        ("Colesterol LDL", "es", Some("mmol/L"), None, "ldl_c", "39469-2"),
        ("Colesterol LDL directo", "es", Some("mg/dL"), None, "ldl_c", "18262-6"),
        ("LDL-Cholesterin", "de", Some("mg/dL"), None, "ldl_c", "2089-1"),
        ("Cholestérol LDL", "fr", Some("mmol/L"), None, "ldl_c", "39469-2"),
        (
            "Colesterol total",
            "es",
            Some("mg/dL"),
            None,
            "total_cholesterol",
            "2093-3",
        ),
        (
            "Colesterol total",
            "es",
            Some("mmol/L"),
            None,
            "total_cholesterol",
            "14647-2",
        ),
        (
            "Hemoglobina glicada (HbA1c)",
            "es",
            Some("%"),
            Some("blood"),
            "hba1c",
            "4548-4",
        ),
        ("HbA1c", "de", Some("mmol/mol"), None, "hba1c", "59261-8"),
        ("Hémoglobine glyquée", "fr", Some("%"), None, "hba1c", "4548-4"),
        ("TSH", "es", Some("µUI/mL"), None, "tsh", "3016-3"),
        ("Thyreotropin", "de", Some("mIU/L"), None, "tsh", "3016-3"),
        ("Ferritina", "es", Some("ng/mL"), None, "ferritin", "2276-4"),
        ("Creatinina", "es", Some("mg/dL"), None, "creatinine", "2160-0"),
        ("Creatinine", "en", Some("µmol/L"), None, "creatinine", "14682-9"),
        ("Créatinine", "fr", Some("µmol/L"), None, "creatinine", "14682-9"),
        (
            "Creatinina",
            "es",
            Some("mg/dL"),
            Some("orina"),
            "urine_creatinine",
            "2161-8",
        ),
        (
            "Creatinina en orina",
            "es",
            Some("mg/dL"),
            None,
            "urine_creatinine",
            "2161-8",
        ),
        ("Neutrófilos", "es", Some("%"), None, "neutrophils_pct", "770-8"),
        ("Neutrófilos", "es", Some("x10^3/µL"), None, "neutrophils_abs", "751-8"),
        ("Plaquetas", "es", Some("x10^9/L"), Some("blood"), "platelets", "777-3"),
        ("S-Glucosa", "es", Some("mg/dl"), None, "glucose", "2345-7"),
        ("Glucosa basal", "es", Some("mg/dL"), None, "glucose", "1558-6"),
        ("Glucosa", "es", Some("mmol/L"), None, "glucose", "14749-6"),
        (
            "Cociente albúmina/creatinina",
            "es",
            Some("mg/g"),
            Some("urine"),
            "urine_acr",
            "9318-7",
        ),
        (
            "Albumin/creatinine ratio",
            "en",
            Some("mg/mmol"),
            None,
            "urine_acr",
            "32294-1",
        ),
        ("CA 125", "es", Some("U/mL"), None, "ca125", "10334-1"),
        ("INR", "es", None, None, "inr", "6301-6"),
        ("ALT (GPT)", "es", Some("U/L"), Some("serum"), "alt", "1742-6"),
        ("GPT (ALAT)", "de", Some("U/l"), None, "alt", "1742-6"),
        (
            "Vitamina D (25-OH)",
            "es",
            Some("ng/mL"),
            None,
            "vitamin_d_25oh",
            "62292-8",
        ),
        (
            "25-OH-Vitamin D3",
            "de",
            Some("nmol/L"),
            None,
            "vitamin_d_25oh",
            "14635-7",
        ),
    ];
    for (name, lang, unit, specimen, group, loinc) in cases {
        let r = res(name, lang, unit, specimen).await;
        assert_eq!(
            (r.alias_group.as_str(), r.loinc.as_deref()),
            (group, Some(loinc)),
            "{name}: {r:?}"
        );
        assert!(
            r.reasons.is_empty() || r.reasons == ["method_uncertain"],
            "{name}: {r:?}"
        );
        assert!((0.8..=1.0).contains(&r.confidence), "{name}: {r:?}");
    }
}

#[tokio::test]
async fn test_same_name_different_unit_is_a_different_code() {
    // Never name alone: total cholesterol in mg/dL and in mmol/L are different LOINC codes.
    let mass = res("Colesterol total", "es", Some("mg/dL"), None).await;
    let molar = res("Colesterol total", "es", Some("mmol/L"), None).await;
    assert_eq!(mass.alias_group, molar.alias_group);
    assert_eq!(
        (mass.property.as_deref(), molar.property.as_deref()),
        (Some("MCnc"), Some("SCnc"))
    );
}

#[tokio::test]
async fn test_panel_names_are_refused() {
    for (name, lang) in [
        ("Perfil lipídico", "es"),
        ("Hemograma completo", "es"),
        ("Blutbild", "de"),
        ("Lipid panel", "en"),
        ("Bilan lipidique", "fr"),
    ] {
        let r = res(name, lang, None, None).await;
        assert!(r.refused && r.loinc.is_none(), "{name}");
    }
}

#[tokio::test]
async fn test_unknown_name_is_unmapped_and_scoped_to_the_provider() {
    let q = ResolveQuery {
        lang: "es",
        provider: Some("LabNorte"),
        ..Default::default()
    };
    let r = n::resolve_analyte(None, "Índice aterogénico", &q)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(r.loinc.is_none() && r.reasons == ["unmapped"]);
    assert_eq!(r.alias_group, "indice_aterogenico@labnorte");
}

#[tokio::test]
async fn test_wrong_unit_keeps_the_group_and_flags_the_dimension() {
    let r = res("Hemoglobina glicada (HbA1c)", "es", Some("mg/dL"), None).await;
    assert!(r.alias_group == "hba1c" && r.reasons.iter().any(|x| x == "unit_dimension_mismatch"));
    assert!(r.confidence < 0.6);
}

#[tokio::test]
async fn test_a_printed_u_is_read_by_the_code_property() {
    let tsh = res("TSH", "de", Some("mU/l"), None).await; // an ACnc code in [IU]: mU/l is mIU/L
    assert_eq!((tsh.loinc.as_deref(), tsh.reasons.len()), (Some("3016-3"), 0));
    let alt = res("ALAT", "fr", Some("UI/L"), None).await; // a CCnc enzyme: UI/L is U/L
    assert!(alt.alias_group == "alt" && alt.reasons.is_empty());
    let gram = res("TSH", "es", Some("mg/L"), None).await; // a mass unit is still wrong
    assert!(gram.reasons.iter().any(|x| x == "unit_dimension_mismatch"));
}

#[tokio::test]
async fn test_giga_and_tera_per_litre_are_counts_not_grams() {
    let wbc = res("Leucocytes", "fr", Some("G/L"), None).await;
    assert!(wbc.alias_group == "wbc" && wbc.reasons.is_empty());
    let rbc = res("Hématies", "fr", Some("T/L"), None).await;
    assert!(rbc.alias_group == "rbc" && rbc.reasons.is_empty());
    let grams = res("Leucocytes", "fr", Some("g/L"), None).await;
    assert!(grams.reasons.iter().any(|x| x == "unit_dimension_mismatch"));
}

#[tokio::test]
async fn test_a_method_word_is_kept_when_the_name_is_reduced() {
    for (name, lang, loinc) in [
        ("LDL-Cholesterin (berechnet)", "de", "13457-7"),
        ("Colesterol LDL (calculado)", "es", "13457-7"),
        ("LDL-Cholesterin (direkt)", "de", "18262-6"),
        ("LDL (berechnet)", "de", "13457-7"), // the alias "LDL" names no method: the name does
        ("LDL direkt", "de", "18262-6"),
    ] {
        let r = res(name, lang, Some("mg/dL"), None).await;
        assert_eq!((r.loinc.as_deref(), r.reasons.len()), (Some(loinc), 0), "{name}: {r:?}");
    }
}

/// Direct bilirubin is its own analyte, not a method of total bilirubin: a name read as total only
/// after dropping "direkt" is a partial match, never auto-accepted.
#[tokio::test]
async fn test_a_method_word_that_names_another_analyte_is_not_stripped_silently() {
    for (name, lang) in [("Bilirubin, direkt", "de"), ("Bilirubin direct", "en")] {
        let r = res(name, lang, Some("mg/dL"), None).await;
        assert!(
            r.alias_group != "bilirubin_total" || r.reasons.iter().any(|x| x == "name_partial"),
            "{name}"
        );
        let exact = res("Bilirubin direkt", "de", Some("mg/dL"), None).await;
        assert_eq!(
            (exact.alias_group.as_str(), exact.reasons.len(), exact.method),
            ("bilirubin_direct", 0, None)
        );
    }
}

#[tokio::test]
async fn test_a_name_matched_only_in_part_is_marked() {
    let r = res("Cortisol (tarde)", "es", Some("ug/dL"), None).await;
    assert!(r.alias_group == "cortisol" && r.reasons.iter().any(|x| x == "name_partial"));
    let gloss = res("GPT (ALT)", "de", Some("U/l"), None).await; // both halves name ALT
    assert!(gloss.alias_group == "alt" && gloss.reasons.is_empty());
}

#[tokio::test]
async fn test_a_text_result_picks_the_ordinal_code() {
    let text = |unit: Option<&'static str>| ResolveQuery {
        lang: "es",
        unit,
        text_result: true,
        ..Default::default()
    };
    let r = n::resolve_analyte(None, "Proteínas en orina", &text(None))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        (r.alias_group.as_str(), r.loinc.as_deref(), r.reasons.len()),
        ("urine_protein_strip", Some("20454-5"), 0)
    );
    let q = res("Proteínas en orina", "es", Some("mg/dL"), None).await;
    assert_ne!(q.loinc.as_deref(), Some("20454-5"));
    let g = n::resolve_analyte(None, "Glucosa en orina", &text(Some("mg/dL")))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(g.loinc.as_deref(), Some("25428-4")); // "Negativo mg/dL": the unit beside a text result names no quantity
}

#[tokio::test]
async fn test_egfr_without_a_year_is_the_older_code_and_marked() {
    let r = res("Filtrado glomerular estimado", "es", Some("mL/min/1.73m2"), None).await;
    assert_eq!(
        (r.loinc.as_deref(), r.reasons),
        (Some("62238-1"), vec!["method_uncertain".to_string()])
    );
    let r = res("Filtrado glomerular (CKD-EPI 2021)", "es", Some("mL/min/1.73m2"), None).await;
    assert_eq!((r.loinc.as_deref(), r.reasons.len()), (Some("98979-8"), 0));
}

#[tokio::test]
async fn test_candidates_are_scored() {
    let r = res("Creatinina", "es", Some("mg/dL"), None).await;
    assert!(!r.candidates.is_empty() && r.candidates[0].alias_group == "creatinine");
    assert!(r.candidates.windows(2).all(|w| w[0].confidence >= w[1].confidence));
}

/// An ignored table never reaches the image (the root .gitignore ignores data/).
#[test]
fn test_tables_are_not_git_ignored() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/terminology");
    let files: Vec<String> = tables::FILES
        .iter()
        .map(|(p, _)| p.to_string())
        .chain(["loinc/NOTICE.md".into()])
        .collect();
    let Ok(out) = std::process::Command::new("git")
        .arg("check-ignore")
        .args(&files)
        .current_dir(&root)
        .output()
    else {
        return; // git not installed
    };
    if out.status.code() == Some(128) {
        return; // not a git checkout
    }
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}
