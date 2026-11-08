//! Which argument error a tool reports, and its words, match 0.1.1's `jsonschema.validate`
//! (`best_match`, Python `repr`). The messages below are python-jsonschema 4.26's output for the
//! Python tool schemas.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn argument_errors_read_as_python_jsonschema_wrote_them() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let cases = [
        (
            "get_trend",
            json!({"profile": "alex", "analyte": "ldl_c"}),
            "Invalid arguments: 'code' is a required property",
        ),
        (
            "search_observations",
            json!({"profile": "alex"}),
            "Invalid arguments: {'profile': 'alex'} is not valid under any of the given schemas",
        ),
        (
            "search_observations",
            json!({"profile": "alex", "query": "ldl", "units": "si", "limit": 5}),
            "Invalid arguments: Additional properties are not allowed ('limit', 'units' were unexpected)",
        ),
        (
            "search_observations",
            json!({"profile": "alex", "category": "vital-signs"}),
            "Invalid arguments: {'profile': 'alex', 'category': 'vital-signs'} is not valid under any of the given schemas",
        ),
        (
            "search_observations",
            json!({"profile": "alex", "query": "ldl", "category": "vital-signs"}),
            "Invalid category: 'vital-signs' is not one of ['lab', 'vital', 'wearable', 'any']",
        ),
        (
            "check_drug",
            json!({"profile": "alex", "drug": ""}),
            "Invalid drug: '' should be non-empty",
        ),
        (
            "lookup_variant",
            json!({"profile": "alex", "rsid": "rs1", "hgvs": "x"}),
            "Invalid arguments: {'profile': 'alex', 'rsid': 'rs1', 'hgvs': 'x'} is valid under each of {'required': ['hgvs']}, {'required': ['rsid']}",
        ),
        (
            "lookup_variant",
            json!({"profile": "alex", "region": "chr1:1"}),
            "Invalid region: 'chr1:1' does not match '^chr[0-9XYM]+:\\\\d+-\\\\d+$'",
        ),
        (
            "get_trend",
            json!({"profile": "alex", "code": "ldl", "window": "P3W", "resolution": "hour"}),
            "Invalid window: 'P3W' does not match '^(P\\\\d+[DMY]|all)$'",
        ),
        (
            "get_trend",
            json!({"profile": 3, "code": 4.5}),
            "Invalid profile: 3 is not of type 'string'",
        ),
    ];
    let mut wrong = Vec::new();
    for (name, args, want) in cases {
        let e = c.call_tool(name, args.clone()).await?.err();
        assert_eq!(e["code"], "bad_input", "{name} {args}");
        if e["message"] != json!(want) {
            wrong.push(format!("{name} {args}\n  want {want}\n  got  {}", e["message"]));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    Ok(())
}

/// Every tool schema uses only keywords the walker knows, so no tool falls back to the crate's words.
#[test]
fn every_tool_schema_is_walked() {
    use ownhealth_mcp::tools::{REGISTRY, pyschema};
    for spec in REGISTRY {
        let schema = (spec.input_schema)();
        let walked = pyschema::iter_errors(&json!({"profile": 1, "unexpected": [1.5, null]}), &schema);
        assert!(walked.is_some_and(|e| !e.is_empty()), "{}", spec.name);
    }
}
