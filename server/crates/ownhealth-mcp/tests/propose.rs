//! `tests/mcp/test_mcp_propose.py`: every kind lands in the review queue, never in the record; failed
//! rows are queued and marked; a person's accept is what writes.

mod common;

use std::collections::BTreeMap;

use common::*;
use ownhealth_core::review;
use serde_json::{Value, json};
use uuid::Uuid;

const FACT_TABLES: [&str; 5] = [
    "observation",
    "note",
    "medication_statement",
    "condition",
    "genome_finding",
];

async fn counts(w: &World, pid: Uuid) -> anyhow::Result<BTreeMap<&'static str, i64>> {
    let mut out = BTreeMap::new();
    for t in FACT_TABLES {
        let sql = format!("select count(*) from {t} where profile_id = $1");
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(pid)
            .fetch_one(&w.db.pool)
            .await?;
        out.insert(t, n);
    }
    Ok(out)
}

async fn item(w: &World, handle: &str) -> anyhow::Result<Value> {
    Ok(
        sqlx::query_scalar("select to_jsonb(r) from review_item r where replace(id::text, '-', '') like $1")
            .bind(format!("{}%", &handle[5..]))
            .fetch_one(&w.db.pool)
            .await?,
    )
}

fn item_id(it: &Value) -> anyhow::Result<Uuid> {
    Ok(Uuid::parse_str(it["id"].as_str().unwrap_or_default())?)
}

#[tokio::test]
async fn observations_are_checked_and_queued() -> R {
    let w = world().await?;
    let before = counts(&w, w.pid("alex")).await?;
    let doc = format!("doc_2026-09-14_{}", &w.doc_id.simple().to_string()[..8]);
    let payload = json!({"doc_id": doc, "rows": [
        {"name": "Colesterol LDL", "value": 97, "unit": "mg/dL", "date": "2026-09-14", "page": 1},
        {"name": "Hierro", "value": "1,62", "unit": "mg/dL", "date": "2026-09-14", "page": 1},
        {"name": "Potasio", "value": 2.1, "unit": "mmol/L", "date": "2025-01-01", "page": 1},
    ]});
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call_tool(
            "propose",
            json!({"profile": "alex", "kind": "observations", "payload": payload, "based_on": [w.s("vitd"), doc]}),
        )
        .await?
        .ok();
    let d = &env["data"];
    assert_eq!(d["status"], "queued_for_review");
    let pid = d["proposal_id"].as_str().unwrap_or_default();
    assert!(pid.starts_with("prop_"));
    let rows = d["rows"].as_array().cloned().unwrap_or_default();
    let grounded: Vec<Value> = rows.iter().map(|r| r["grounded"].clone()).collect();
    assert_eq!(grounded, [json!(true), json!(false), json!(false)]);
    let checks: Vec<Value> = rows.iter().map(|r| r["check"].clone()).collect();
    assert_eq!(checks, [json!("ok"), json!("not_grounded"), json!("not_grounded")]);
    assert!(strs(&rows[2]["failed"]).contains(&"date_in_document".to_string()));
    assert_eq!(
        rows[0]["name"],
        json!({"trust": "untrusted_text", "origin": "agent_note", "text": "Colesterol LDL"})
    );
    let it = item(&w, pid).await?;
    assert!(
        d["review_url"]
            .as_str()
            .unwrap_or_default()
            .ends_with(it["id"].as_str().unwrap_or("?"))
    );
    assert_eq!(counts(&w, w.pid("alex")).await?, before);
    assert_eq!(it["kind"], "agent_proposal");
    assert_eq!(it["status"], "open");
    assert_eq!(it["row_conf"].as_f64(), Some(0.0));
    let reasons: std::collections::BTreeSet<String> = strs(&it["reasons"]).into_iter().collect();
    assert_eq!(
        reasons,
        ["date_invalid", "not_grounded"].iter().map(|s| s.to_string()).collect()
    );
    assert_eq!(it["document_id"], json!(w.doc_id));
    assert!(
        it["context_documents"]
            .as_array()
            .is_some_and(|a| a.contains(&json!(w.doc_id)))
    );
    assert!(strs(&it["context_src_ids"]).contains(&w.s("vitd")));
    assert_eq!(it["payload"]["rows"].as_array().map(Vec::len), Some(3));
    assert_eq!(it["payload"]["token_label"], "Claude Code · test");

    let mut conn = w.conn().await?;
    let created = review::accept(&mut conn, item_id(&it)?, w.user("alex"), None).await?;
    assert_eq!(created.len(), 3);
    let (accepted_by, src): (String, String) =
        sqlx::query_as("select accepted_by, src_id from observation where id = $1")
            .bind(created[0])
            .fetch_one(&mut *conn)
            .await?;
    assert_eq!(accepted_by, "person");
    let kind: String = sqlx::query_scalar("select kind from src where id = $1")
        .bind(&src)
        .fetch_one(&mut *conn)
        .await?;
    assert_eq!(kind, "document_region");
    Ok(())
}

#[tokio::test]
async fn note_medication_and_finding_status() -> R {
    let w = world().await?;
    let before = counts(&w, w.pid("alex")).await?;
    let c = w.connect(&w.tok().await?).await?;
    let p = |kind: &str, payload: Value| json!({"profile": "alex", "kind": kind, "payload": payload});
    let note = c
        .call_tool(
            "propose",
            p(
                "note",
                json!({"text": "Ask about vitamin D.", "attach_to": w.s("vitd")}),
            ),
        )
        .await?
        .ok();
    assert_eq!(note["data"]["checks"][0]["ok"], true);
    let bad = c
        .call_tool("propose", p("note", json!({"text": "x", "attach_to": w.s("sam_ldl")})))
        .await?
        .ok();
    assert_eq!(bad["data"]["checks"][0]["ok"], false);
    let med = c
        .call_tool(
            "propose",
            p(
                "medication",
                json!({"action": "stop", "name": "Atorvastatin", "date": "2026-11-01", "reason": "muscle pain"}),
            ),
        )
        .await?
        .ok();
    assert_eq!(med["data"]["checks"][0]["ok"], true);
    assert!(truthy(&med["data"]["drug"]["cpic_drug_id"]));
    let new = c
        .call_tool(
            "propose",
            p(
                "medication",
                json!({"action": "start", "name": "Vitamina D3 1000 UI", "date": "2026-11-01"}),
            ),
        )
        .await?
        .ok();
    assert!(
        strs(&new["limitations"]).iter().any(|x| x.contains("free text")),
        "{}",
        new["limitations"]
    );
    let fid: String = sqlx::query_scalar("select fact_id from genome_finding where src_id = $1")
        .bind(w.s("slco1b1"))
        .fetch_one(&w.db.pool)
        .await?;
    let apoe: String = sqlx::query_scalar("select fact_id from genome_finding where src_id = $1")
        .bind(w.s("apoe"))
        .fetch_one(&w.db.pool)
        .await?;
    let fs = c
        .call_tool(
            "propose",
            p(
                "finding_status",
                json!({"finding_id": fid, "verification": "confirmed", "reason": "clinical lab confirmed"}),
            ),
        )
        .await?
        .ok();
    let oks: Vec<Value> = fs["data"]["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| x["ok"].clone())
        .collect();
    assert_eq!(oks, [json!(true), json!(true)]);
    let hidden = c
        .call_tool(
            "propose",
            p(
                "finding_status",
                json!({"finding_id": apoe, "verification": "confirmed", "reason": "x"}),
            ),
        )
        .await?
        .ok();
    assert_eq!(hidden["data"]["checks"][0]["ok"], false);
    let e = c
        .call_tool("propose", p("note", json!({"text": "x".repeat(4001)})))
        .await?
        .err();
    assert_eq!(e["code"], "bad_input");
    let e = c
        .call_tool(
            "propose",
            p("finding_status", json!({"finding_id": fid, "reason": "x"})),
        )
        .await?
        .err();
    assert_eq!(e["code"], "bad_input");
    assert_eq!(counts(&w, w.pid("alex")).await?, before);

    let mut conn = w.conn().await?;
    let note_item = item(&w, note["data"]["proposal_id"].as_str().unwrap_or_default()).await?;
    let ids = review::accept(&mut conn, item_id(&note_item)?, w.user("alex"), None).await?;
    assert_eq!(ids.len(), 1);
    let author: String = sqlx::query_scalar("select author_kind from note where id = $1")
        .bind(ids[0])
        .fetch_one(&mut *conn)
        .await?;
    assert_eq!(author, "agent");
    let stop_item = item(&w, med["data"]["proposal_id"].as_str().unwrap_or_default()).await?;
    review::accept(&mut conn, item_id(&stop_item)?, w.user("alex"), None).await?;
    let status: String =
        sqlx::query_scalar("select med_status from medication_statement where profile_id = $1 and status = 'current'")
            .bind(w.pid("alex"))
            .fetch_one(&mut *conn)
            .await?;
    assert_eq!(status, "stopped");
    Ok(())
}

#[tokio::test]
async fn propose_needs_write_scope_and_row_cap() -> R {
    let w = world().await?;
    let note = json!({"profile": "alex", "kind": "note", "payload": {"text": "x"}});
    let c = w
        .connect(&w.tok_scopes(scopes(&["read:summary", "read:labs"])).await?)
        .await?;
    assert!(!c.tool_names().await?.contains("propose"));
    assert_eq!(
        c.call_tool("propose", note.clone()).await?.err()["code"],
        "scope_missing"
    );
    let c = w.connect(&w.tok_scopes(scopes(&["write:propose"])).await?).await?;
    assert_eq!(c.call_tool("propose", note).await?.err()["code"], "scope_missing");
    let rows: Vec<Value> = (0..201)
        .map(|_| json!({"name": "A", "value": 1, "unit": "g", "date": "2026-09-14", "page": 1}))
        .collect();
    let c = w.connect(&w.tok().await?).await?;
    let e = c
        .call_tool(
            "propose",
            json!({"profile": "alex", "kind": "observations",
                   "payload": {"doc_id": "doc_2026-09-14_00000000", "rows": rows}}),
        )
        .await?
        .err();
    assert_eq!(e["code"], "propose_cap");
    Ok(())
}
