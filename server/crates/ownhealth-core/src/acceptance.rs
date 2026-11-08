//! The auto-accept rule (docs/design/ingestion.md section 9.1), as one function (`core/acceptance.py`).
//!
//! A row is committed without a click only when all of these hold:
//! * every check passed;
//! * row_conf >= threshold (instance setting, default 0.95, never below 0.90);
//! * the source allows it: a vendor template or generic rules on a native text layer, OCR with a
//!   matching vendor template at row_conf >= 0.98, or a structured import with a valid code and unit;
//! * the identity on the page matched the profile (not just hinted).
//!
//! Agent proposals, VLM rows and manual rows never auto-accept.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Reason codes, one list for every importer (architecture.md 6.2, ingestion.md 9).
pub const REASON_CODES: [&str; 20] = [
    "loinc_unknown",
    "loinc_deprecated",
    "unmapped",
    "unit_dimension_mismatch",
    "unit_unconvertible",
    "implausible",
    "suspect_separator",
    "range_parse",
    "value_is_range",
    "not_grounded",
    "name_not_on_line",
    "identity_mismatch",
    "identity_missing",
    "date_invalid",
    "duplicate",
    "conflict",
    "amended_value",
    "possible_injection",
    "flag_mismatch",
    "ambiguous_separator",
];

/// template_text | rules_text | ocr_template | ocr_rules | structured_valid | structured_partial |
/// vlm | agent | manual
pub const SOURCE_ALLOWANCES: [&str; 9] = [
    "template_text",
    "rules_text",
    "ocr_template",
    "ocr_rules",
    "structured_valid",
    "structured_partial",
    "vlm",
    "agent",
    "manual",
];

/// matched | hinted | mismatch | missing
pub const IDENTITIES: [&str; 4] = ["matched", "hinted", "mismatch", "missing"];

pub const MIN_THRESHOLD: f64 = 0.90;
pub const DEFAULT_THRESHOLD: f64 = 0.95;
pub const OCR_TEMPLATE_THRESHOLD: f64 = 0.98;

/// One validator result. `reason` is a code from `REASON_CODES` when `ok` is false.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

impl Check {
    pub fn pass(name: &str) -> Check {
        Check {
            name: name.into(),
            ok: true,
            reason: None,
            detail: None,
        }
    }

    pub fn fail(name: &str, reason: &str) -> Check {
        Check {
            name: name.into(),
            ok: false,
            reason: Some(reason.into()),
            detail: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decision {
    pub auto: bool,
    /// Reason codes from failed checks, sorted.
    pub reasons: Vec<String>,
    /// Human-readable, for the review payload.
    pub why_not: Vec<String>,
}

/// The reason codes of the failed checks, sorted and unique.
pub fn failed_reasons(checks: &[Check]) -> Vec<String> {
    checks
        .iter()
        .filter(|c| !c.ok)
        .filter_map(|c| c.reason.clone().filter(|r| !r.is_empty()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn auto_accept(row_conf: f64, checks: &[Check], source: &str, identity: &str, threshold: f64) -> Decision {
    let threshold = threshold.max(MIN_THRESHOLD);
    let mut reasons = failed_reasons(checks);
    let mut why_not = Vec::new();
    if checks.iter().any(|c| !c.ok) {
        why_not.push("a check failed".to_string());
    }
    let needed = match source {
        "template_text" | "rules_text" | "structured_valid" => Some(threshold),
        "ocr_template" => Some(threshold.max(OCR_TEMPLATE_THRESHOLD)),
        _ => {
            why_not.push(format!("source {source} never auto-accepts"));
            None
        }
    };
    // A positive range check, so a NaN or an out-of-range confidence fails closed.
    if let Some(needed) = needed
        && !(needed <= row_conf && row_conf <= 1.0)
    {
        why_not.push(format!("row confidence {} below {needed:.2}", py_2f(row_conf)));
    }
    if identity != "matched" {
        why_not.push(format!("identity {identity}"));
        let code = match identity {
            "mismatch" => Some("identity_mismatch"),
            "missing" => Some("identity_missing"),
            _ => None,
        };
        if let Some(code) = code
            && !reasons.iter().any(|r| r == code)
        {
            reasons.push(code.into());
            reasons.sort();
        }
    }
    Decision {
        auto: why_not.is_empty(),
        reasons,
        why_not,
    }
}

/// Python's `f"{x:.2f}"`, which prints `nan` and `inf` in lowercase.
fn py_2f(x: f64) -> String {
    if x.is_nan() {
        "nan".into()
    } else if x.is_infinite() {
        if x > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        format!("{x:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Vec<Check> {
        vec![Check::pass("loinc"), Check::pass("units")]
    }

    #[test]
    fn auto_accept_matrix() {
        let bad = vec![Check::fail("units", "unit_dimension_mismatch")];
        let cases: Vec<(f64, Vec<Check>, &str, &str, bool)> = vec![
            (0.95, ok(), "template_text", "matched", true),
            (0.95, ok(), "rules_text", "matched", true),
            (0.99, ok(), "structured_valid", "matched", true),
            (0.949, ok(), "rules_text", "matched", false),
            (0.99, bad, "template_text", "matched", false),
            (0.99, ok(), "template_text", "hinted", false),
            (0.97, ok(), "ocr_template", "matched", false),
            (0.98, ok(), "ocr_template", "matched", true),
            (0.99, ok(), "ocr_rules", "matched", false),
            (1.0, ok(), "agent", "matched", false),
            (1.0, ok(), "vlm", "matched", false),
            (1.0, ok(), "structured_partial", "matched", false),
        ];
        for (conf, checks, source, identity, want) in cases {
            assert_eq!(
                auto_accept(conf, &checks, source, identity, DEFAULT_THRESHOLD).auto,
                want,
                "{conf} {source} {identity}"
            );
        }
    }

    #[test]
    fn threshold_never_below_090_and_reasons_collected() {
        assert!(!auto_accept(0.85, &ok(), "rules_text", "matched", 0.5).auto);
        assert!(auto_accept(0.91, &ok(), "rules_text", "matched", 0.9).auto);
        let d = auto_accept(
            0.99,
            &[Check::fail("g", "not_grounded")],
            "rules_text",
            "mismatch",
            DEFAULT_THRESHOLD,
        );
        assert_eq!(d.reasons, vec!["identity_mismatch", "not_grounded"]);
    }

    #[test]
    fn a_confidence_outside_0_1_fails_closed() {
        for conf in [f64::NAN, 1.5, f64::INFINITY] {
            let d = auto_accept(conf, &ok(), "rules_text", "matched", DEFAULT_THRESHOLD);
            assert!(!d.auto && !d.why_not.is_empty());
        }
        let d = auto_accept(0.5, &ok(), "rules_text", "matched", DEFAULT_THRESHOLD);
        assert_eq!(d.why_not, vec!["row confidence 0.50 below 0.95"]);
    }
}
