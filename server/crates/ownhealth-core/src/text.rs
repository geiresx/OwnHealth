//! Free text that did not come from our own templates (`mcp/envelope.py` `untrusted`). Every lane
//! wraps such text before it goes out over MCP.

use serde_json::{Value, json};

use crate::error::{Error, Result};

pub const ORIGINS: [&str; 7] = [
    "pdf_text_layer",
    "ocr",
    "email_subject",
    "fhir_note",
    "agent_note",
    "person_note",
    // Free text from an imported OwnGenome facts file (lane D).
    "genome_facts",
];

pub const NOTICE: &str = "Information from your records, not a diagnosis.";

/// `{"trust": "untrusted_text", "origin": ..., "text": ...}`; None stays None (JSON null).
pub fn untrusted(text: Option<&str>, origin: &str) -> Result<Value> {
    let Some(text) = text else {
        return Ok(Value::Null);
    };
    if !ORIGINS.contains(&origin) {
        return Err(Error::Invalid(format!("unknown untrusted_text origin {origin:?}")));
    }
    Ok(json!({"trust": "untrusted_text", "origin": origin, "text": text}))
}

pub fn is_untrusted(value: &Value) -> bool {
    value.get("trust").and_then(Value::as_str) == Some("untrusted_text") && value.get("text").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_refuses_unknown_origins() {
        let v = untrusted(Some("hola"), "ocr").ok();
        assert_eq!(
            v,
            Some(json!({"trust": "untrusted_text", "origin": "ocr", "text": "hola"}))
        );
        assert!(v.as_ref().is_some_and(is_untrusted));
        assert_eq!(untrusted(None, "nope").ok(), Some(Value::Null));
        assert!(untrusted(Some("x"), "nope").is_err());
    }
}
