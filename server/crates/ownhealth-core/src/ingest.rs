//! What every importer returns (`ingest/__init__.py`).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

/// Stored in `job.result` and shown in the import screen. Serializes as Python's `as_dict()`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ImportSummary {
    pub document_id: String,
    pub accepted: i64,
    /// Review items created.
    pub queued: i64,
    /// Duplicates.
    pub skipped: i64,
    /// Reason code -> count.
    pub reasons: BTreeMap<String, i64>,
}

impl ImportSummary {
    pub fn new(document_id: impl Into<String>) -> Self {
        ImportSummary {
            document_id: document_id.into(),
            ..Default::default()
        }
    }

    /// Count one routed fact.
    pub fn count(&mut self, result: &crate::review::RouteResult) {
        if result.state == "auto_accepted" {
            self.accepted += 1;
        } else {
            self.queued += 1;
        }
        for r in &result.reasons {
            *self.reasons.entry(r.clone()).or_default() += 1;
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}
