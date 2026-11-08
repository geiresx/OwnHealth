//! `tests/mcp/test_mcp_tools.py`: contract tests for the eight tools, through an MCP client session.

mod common;

use std::collections::BTreeSet;

use common::*;
use ownhealth_core::observations::retract_observation;
use ownhealth_mcp::envelope::ToolError;
use serde_json::{Value, json};
use uuid::Uuid;

fn arr(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn col(v: &Value, key: &str) -> Vec<Value> {
    arr(v).iter().map(|x| x[key].clone()).collect()
}

fn py_round_pct(a: f64, b: f64) -> i64 {
    ((a - b) / b * 100.0).round_ties_even() as i64
}

#[tokio::test]
async fn search_summary_full_draw_and_flags() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call(
            "search_observations",
            json!({"profile": "alex", "query": "Colesterol LDL"}),
        )
        .await?
        .ok();
    let rows = arr(&env["data"]["results"]);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["src"], json!(w.s("ldl_latest")));
    assert_eq!(row["value"], json!(97));
    assert_eq!(row["unit"], "mg/dL");
    assert_eq!(
        row["name"],
        json!({"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "Colesterol LDL"})
    );
    assert_eq!(row["range"], json!({"high": 130}));
    assert_eq!(row["code"], "LOINC:13457-7");
    assert_eq!(row["lab"]["trust"], "untrusted_text");
    assert!(
        row["draw_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("doc_2026-09-14_")
    );
    assert_eq!(
        env["data"]["resolved"]["codes"],
        json!(["LOINC:13457-7", "LOINC:18262-6"])
    );
    let full = c
        .call(
            "search_observations",
            json!({"profile": "alex", "loinc": "13457-7", "detail": "full"}),
        )
        .await?
        .ok();
    assert_eq!(arr(&full["data"]["results"]).len(), 8);
    let flagged = c
        .call(
            "search_observations",
            json!({"profile": "alex", "loinc": "LOINC:13457-7", "detail": "full", "flagged_only": true}),
        )
        .await?
        .ok();
    assert_eq!(col(&flagged["data"]["results"], "value"), vec![json!(163)]);
    let window = c
        .call(
            "search_observations",
            json!({"profile": "alex", "query": "ldl", "detail": "full", "from": "2025-01-01", "to": "2025-12-31"}),
        )
        .await?
        .ok();
    let dates: BTreeSet<String> = strs(&json!(col(&window["data"]["results"], "date")))
        .into_iter()
        .collect();
    assert_eq!(
        dates,
        ["2025-01-20", "2025-09-08"].iter().map(|s| s.to_string()).collect()
    );
    let draws = c
        .call(
            "search_observations",
            json!({"profile": "alex", "query": "LabNorte", "group_by": "draw"}),
        )
        .await?
        .ok();
    assert_eq!(draws["data"]["results"], json!([]));
    assert_eq!(draws["data"]["not_measured"], true);
    let by_draw = c
        .call(
            "search_observations",
            json!({"profile": "alex", "loinc": "13457-7", "group_by": "draw", "detail": "full", "from": "2026-09-01"}),
        )
        .await?
        .ok();
    let draws = arr(&by_draw["data"]["results"]);
    assert_eq!(draws.len(), 1);
    assert!(draws[0]["draw_id"].as_str().unwrap_or_default().starts_with("doc_"));
    assert_eq!(draws[0]["results"][0]["src"], json!(w.s("ldl_latest")));
    let e = c.call("search_observations", json!({"profile": "alex"})).await?.err();
    assert_eq!(e["code"], "bad_input");
    let e = c
        .call(
            "search_observations",
            json!({"profile": "alex", "query": "x", "category": "genome"}),
        )
        .await?
        .err();
    assert_eq!(e["code"], "bad_input");
    Ok(())
}

#[tokio::test]
async fn trend_keeps_both_codes_stats_and_annotations() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call(
            "get_trend",
            json!({"profile": "alex", "code": "LOINC:13457-7", "window": "P6Y"}),
        )
        .await?
        .ok();
    let d = &env["data"];
    assert_eq!(d["codes"], json!(["LOINC:13457-7", "LOINC:18262-6"]), "{d}");
    assert_eq!(d["canonical_unit"], "mmol/L");
    let dates = strs(&json!(col(&d["points"], "date")));
    let mut sorted = dates.clone();
    sorted.sort();
    assert_eq!(dates, sorted);
    assert_eq!(dates.len(), 8);
    assert_eq!(d["resolution"], "raw");
    assert_eq!(d["stats"]["n"], 8);
    assert!(["increasing", "decreasing", "no significant trend"].contains(&d["stats"]["trend"].as_str().unwrap_or("")));
    assert_eq!(d["stats"]["last_vs_previous_pct"], json!(py_round_pct(97.0, 118.0)));
    assert_eq!(col(&d["annotations"], "event"), vec![json!("medication_started")]);
    assert_eq!(d["annotations"][0]["name"]["trust"], "untrusted_text");
    assert!(
        strs(&env["limitations"])
            .iter()
            .any(|x| x.contains("mixes LOINC codes"))
    );
    let c = w.connect(&w.tok_scopes(scopes(&["read:labs"])).await?).await?;
    let env = c
        .call(
            "get_trend",
            json!({"profile": "alex", "code": "13457-7", "window": "all"}),
        )
        .await?
        .ok();
    assert_eq!(env["data"]["annotations"], json!([]));
    let nothing = c
        .call(
            "get_trend",
            json!({"profile": "alex", "code": "13457-7", "window": "P30D"}),
        )
        .await?
        .ok();
    assert_eq!(nothing["data"]["points"], json!([]));
    assert_eq!(nothing["data"]["not_measured"], true);
    Ok(())
}

#[tokio::test]
async fn get_source_document_region() -> R {
    let w = world().await?;
    let c = w
        .connect(&w.tok_scopes(without(&["read:documents_text"])).await?)
        .await?;
    let env = c.call("get_source", json!({"src_id": w.s("vitd")})).await?.ok();
    let d = &env["data"];
    assert_eq!(env["profile"], "alex");
    assert_eq!(d["status"], "current");
    assert_eq!(d["location"]["page"], 1);
    assert_eq!(arr(&d["location"]["bbox"]).len(), 4);
    assert_eq!(d["location"]["units"], "pt");
    assert_eq!(
        d["printed"]["value"],
        json!({"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "24"})
    );
    assert_eq!(d["derived"]["loinc"], "62292-8");
    assert!(
        d["document"]["id"]
            .as_str()
            .unwrap_or_default()
            .starts_with("doc_2026-09-14_")
    );
    let prefix = format!("http://127.0.0.1:8080/import-review?doc={}&page=1", w.doc_id);
    assert!(
        d["crop_url"].as_str().unwrap_or_default().starts_with(&prefix),
        "{}",
        d["crop_url"]
    );
    assert!(d["text"].is_null());
    let with_text = c
        .call("get_source", json!({"src_id": w.s("vitd"), "include_text": true}))
        .await?
        .ok();
    assert!(with_text["data"]["text"].is_null());
    assert_eq!(with_text["withheld"]["by_scope"], json!(["documents_text"]));
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call("get_source", json!({"src_id": w.s("vitd"), "include_text": true}))
        .await?
        .ok();
    assert_eq!(
        env["data"]["text"],
        json!({"trust": "untrusted_text", "origin": "pdf_text_layer", "text": "Colesterol LDL"})
    );
    assert_eq!(
        c.call("get_source", json!({"src_id": "bad id"})).await?.err()["code"],
        "bad_input"
    );
    Ok(())
}

#[tokio::test]
async fn get_source_lifecycle() -> R {
    let w = world().await?;
    let src = w.s("ldl_2025-01-20");
    let oid: Uuid = sqlx::query_scalar("select id from observation where src_id = $1")
        .bind(&src)
        .fetch_one(&w.db.pool)
        .await?;
    retract_observation(&mut *w.conn().await?, oid, "OCR misread, confirmed by the lab").await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c.call("get_source", json!({"src_id": src})).await?.ok();
    assert_eq!(env["data"]["status"], "retracted");
    assert_eq!(env["data"]["reason"]["trust"], "untrusted_text");
    let trend = c
        .call(
            "get_trend",
            json!({"profile": "alex", "code": "13457-7", "window": "all"}),
        )
        .await?
        .ok();
    assert!(!col(&trend["data"]["points"], "src").contains(&json!(src)));
    Ok(())
}

#[tokio::test]
async fn genome_findings_and_lookup() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c.call("get_genome_findings", json!({"profile": "alex"})).await?.ok();
    let genes: BTreeSet<String> = strs(&json!(col(&env["data"]["findings"], "gene")))
        .into_iter()
        .collect();
    assert_eq!(genes, ["HFE", "SLCO1B1"].iter().map(|s| s.to_string()).collect());
    let states = ["ref", "alt", "no_call", "not_on_array", "not_covered"];
    assert!(
        arr(&env["data"]["findings"])
            .iter()
            .all(|f| states.contains(&f["call_state"].as_str().unwrap_or("")))
    );
    assert!(truthy(&env["not_assessed"]));
    assert_eq!(env["db_versions"], json!({"clinvar": "2026-10-28"}));
    assert!(strs(&env["limitations"]).iter().any(|x| x.contains("Research-grade")));
    let pgx = c
        .call("get_genome_findings", json!({"profile": "alex", "category": "pgx"}))
        .await?
        .ok();
    assert_eq!(col(&pgx["data"]["findings"], "gene"), vec![json!("SLCO1B1")]);
    let hit = c
        .call("lookup_variant", json!({"profile": "alex", "rsid": "rs1800562"}))
        .await?
        .ok();
    assert_eq!(hit["data"]["calls"][0]["call_state"], "alt");
    assert_eq!(hit["data"]["quota"], json!({"used_today": 1, "daily_cap": 200}));
    let e = c
        .call(
            "lookup_variant",
            json!({"profile": "alex", "rsid": "rs1", "region": "chr1:1-2"}),
        )
        .await?
        .err();
    assert_eq!(e["code"], "bad_input");
    let e = c
        .call("lookup_variant", json!({"profile": "alex", "region": "chr1:1-2000000"}))
        .await?
        .err();
    assert_eq!(e["code"], "bad_input");
    let page1 = c
        .call("lookup_variant", json!({"profile": "alex", "region": "chr6:1000-2000"}))
        .await?
        .ok();
    assert!(truthy(&page1["next_cursor"]));
    let page2 = c
        .call(
            "lookup_variant",
            json!({"profile": "alex", "region": "chr6:1000-2000", "cursor": page1["next_cursor"]}),
        )
        .await?
        .ok();
    assert_eq!(col(&page2["data"]["calls"], "pos"), vec![json!(1002), json!(1003)]);
    assert!(page2["next_cursor"].is_null());
    Ok(())
}

#[tokio::test]
async fn lookup_daily_cap() -> R {
    let w = world().await?;
    let tok = w.tok().await?;
    w.set_limit(&tok, "genome_raw_lookups_per_day", 2).await?;
    let c = w.connect(&tok).await?;
    for _ in 0..2 {
        c.call("lookup_variant", json!({"profile": "alex", "rsid": "rs1800562"}))
            .await?
            .ok();
    }
    let e = c
        .call("lookup_variant", json!({"profile": "alex", "rsid": "rs1800562"}))
        .await?
        .err();
    assert_eq!(e["code"], "genome_lookup_cap");
    assert_eq!(e["daily_cap"], 2);
    c.call("get_genome_findings", json!({"profile": "alex"})).await?.ok();
    Ok(())
}

#[tokio::test]
async fn check_drug() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call("check_drug", json!({"profile": "alex", "drug": "atorvastatin"}))
        .await?
        .ok();
    let d = &env["data"];
    assert_eq!(d["pgx"][0]["gene"], "SLCO1B1");
    assert_eq!(d["current_medications_relevant"][0]["src"], json!(w.s("statin")));
    assert_eq!(d["current_medications_relevant"][0]["name"]["trust"], "untrusted_text");
    assert!(
        env["notice"]
            .as_str()
            .unwrap_or_default()
            .ends_with("Discuss with your prescriber.")
    );
    assert!(
        arr(&env["not_assessed"])
            .iter()
            .any(|x| x.to_string().contains("drug-drug"))
    );
    let c = w.connect(&w.tok_scopes(scopes(&["read:labs"])).await?).await?;
    let env = c
        .call("check_drug", json!({"profile": "alex", "drug": "atorvastatin"}))
        .await?
        .ok();
    let keys: BTreeSet<&str> = env["data"]
        .as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();
    assert_eq!(keys, ["drug", "relevant_labs"].into_iter().collect());
    assert_eq!(env["withheld"]["by_scope"], json!(["genome", "records"]));
    Ok(())
}

#[test]
fn tool_error_shape() {
    assert_eq!(
        ToolError::new("x", "m", Some("f")).as_dict(),
        json!({"isError": true, "code": "x", "message": "m", "fix": "f"})
    );
}

#[tokio::test]
async fn wearable_trend_uses_the_day_total_for_summed_metrics() -> R {
    let w = world().await?;
    let pid = w.pid("alex");
    let sid: i16 = sqlx::query_scalar(
        "insert into wearable_source (profile_id, provider, device) values ($1, 'apple_health', 'Watch') returning id",
    )
    .bind(pid)
    .fetch_one(&w.db.pool)
    .await?;
    sqlx::query(
        "insert into wearable_rollup (profile_id, metric, source_id, bucket, t, n, v_min, v_max, v_avg, v_sum, \
         v_p10, v_p50, v_p90, coverage) values ($1, 'sleep_asleep', $2, 'day', '2026-11-01', 3, 60, 240, \
         140, 420, 60, 120, 240, 1)",
    )
    .bind(pid)
    .bind(sid)
    .execute(&w.db.pool)
    .await?;
    let c = w.connect_real(&w.tok().await?).await?;
    let env = c
        .call(
            "get_trend",
            json!({"profile": "alex", "code": "sleep_asleep", "window": "all"}),
        )
        .await?
        .ok();
    // Python's `== [420]` holds for 420.0 too; both sides serialize the float column as 420.0
    let values: Vec<Option<f64>> = col(&env["data"]["points"], "value").iter().map(Value::as_f64).collect();
    assert_eq!(values, vec![Some(420.0)]);
    Ok(())
}

#[tokio::test]
async fn canonical_follows_the_token_unit_preference() -> R {
    let w = world().await?;
    let args = json!({"profile": "alex", "loinc": "13457-7", "detail": "full"});
    for (units, unit) in [("si", "mmol/L"), ("conventional", "mg/dL")] {
        let c = w
            .connect(
                &w.token(Tok {
                    units,
                    ..Tok::default()
                })
                .await?,
            )
            .await?;
        let found = arr(&c.call("search_observations", args.clone()).await?.ok()["data"]["results"]);
        let trend = c
            .call(
                "get_trend",
                json!({"profile": "alex", "code": "LOINC:13457-7", "window": "all"}),
            )
            .await?
            .ok();
        let units_seen: BTreeSet<String> = found
            .iter()
            .filter(|r| truthy(&r["canonical"]))
            .filter_map(|r| r["canonical"]["unit"].as_str().map(str::to_string))
            .collect();
        assert!(!found.is_empty());
        assert_eq!(units_seen, [unit.to_string()].into_iter().collect(), "{units}");
        assert_eq!(trend["data"]["canonical_unit"], unit);
        assert!(
            arr(&trend["data"]["points"])
                .iter()
                .all(|p| p.get("canonical").is_some())
        );
    }
    let c = w
        .connect(
            &w.token(Tok {
                units: "as_reported",
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let found = arr(&c.call("search_observations", args).await?.ok()["data"]["results"]);
    assert!(!found.is_empty() && found.iter().all(|r| r.get("canonical").is_none_or(Value::is_null)));
    Ok(())
}

#[tokio::test]
async fn get_source_withholds_page_text_of_a_sensitive_document() -> R {
    let w = world().await?;
    let o = obs("VIH Ac", "1", Some("{index}"), "hiv_ab", None, "2026-09-14")
        .sensitive("sexual_health")
        .doc(w.doc_id);
    add_obs(&mut *w.conn().await?, w.pid("alex"), o, None).await?;
    let args = json!({"src_id": w.s("vitd"), "include_text": true});
    let c = w.connect(&w.tok_scopes(without(&["read:sensitive"])).await?).await?;
    let env = c.call("get_source", args.clone()).await?.ok();
    assert!(env["data"]["text"].is_null());
    assert!(strs(&env["withheld"]["by_scope"]).contains(&"sensitive".to_string()));
    assert!(env["data"].get("crop_url").is_none());
    let c = w.connect(&w.tok().await?).await?;
    let env = c.call("get_source", args).await?.ok();
    assert_eq!(env["data"]["text"]["text"], "Colesterol LDL");
    Ok(())
}

#[tokio::test]
async fn get_source_page_text_needs_every_category_on_the_page() -> R {
    let w = world().await?;
    let o = obs("Peso", "80", Some("kg"), "body_weight", Some("29463-7"), "2026-09-14")
        .category("vital-signs")
        .doc(w.doc_id);
    let src = region(w.pid("alex"), w.doc_id, w.boxes[0]);
    add_obs(&mut *w.conn().await?, w.pid("alex"), o, Some(src)).await?;
    let labs_only = ["read:summary", "read:labs", "read:documents_text"];
    let args = json!({"src_id": w.s("vitd"), "include_text": true});
    let c = w.connect(&w.tok_scopes(scopes(&labs_only)).await?).await?;
    let env = c.call("get_source", args.clone()).await?.ok();
    assert!(env["data"]["text"].is_null());
    assert_eq!(env["withheld"]["by_scope"], json!(["vitals"]));
    let mut more = scopes(&labs_only);
    more.insert("read:vitals".into());
    let c = w.connect(&w.tok_scopes(more).await?).await?;
    let env = c.call("get_source", args).await?.ok();
    assert_eq!(env["data"]["text"]["text"], "Colesterol LDL");
    Ok(())
}

#[tokio::test]
async fn get_source_text_of_a_pending_page_needs_every_category_and_the_genome_switch() -> R {
    let w = world().await?;
    sqlx::query(
        "insert into review_item (household_id, profile_id, kind, payload, document_id) \
         values ($1, $2, 'pending_document', '{\"pages\": [1]}'::jsonb, $3)",
    )
    .bind(w.family.household_id)
    .bind(w.pid("alex"))
    .bind(w.doc_id)
    .execute(&w.db.pool)
    .await?;
    let args = json!({"src_id": w.s("vitd"), "include_text": true});
    let c = w.connect(&w.tok_scopes(without(&["read:genome"])).await?).await?;
    let env = c.call("get_source", args.clone()).await?.ok();
    assert!(env["data"]["text"].is_null());
    assert!(strs(&env["withheld"]["by_scope"]).contains(&"genome".to_string()));
    let c = w.connect(&w.tok().await?).await?;
    let env = c.call("get_source", args.clone()).await?.ok();
    assert!(env["data"]["text"].is_null());
    assert_eq!(env["withheld"]["sensitive"], 1);
    sqlx::query("update profile set genome_sensitive_agents = true where id = $1")
        .bind(w.pid("alex"))
        .execute(&w.db.pool)
        .await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c.call("get_source", args).await?.ok();
    assert_eq!(env["data"]["text"]["text"], "Colesterol LDL");
    Ok(())
}

#[tokio::test]
async fn get_source_text_of_a_minors_pending_page_is_withheld() -> R {
    let w = world().await?;
    let lucia = w.pid("lucia");
    w.exec(&format!(
        "update document set profile_id = '{lucia}' where id = '{}'",
        w.doc_id
    ))
    .await?;
    w.exec(&format!(
        "update profile set genome_sensitive_agents = true where id = '{lucia}'"
    ))
    .await?;
    let o = obs(
        "Hemoglobina",
        "13",
        Some("g/dL"),
        "hemoglobin",
        Some("718-7"),
        "2026-09-14",
    )
    .doc(w.doc_id);
    let sid = add_obs(
        &mut *w.conn().await?,
        lucia,
        o,
        Some(region(lucia, w.doc_id, w.boxes[0])),
    )
    .await?;
    sqlx::query(
        "insert into review_item (household_id, profile_id, kind, payload, document_id) \
         values ($1, $2, 'pending_document', '{}'::jsonb, $3)",
    )
    .bind(w.family.household_id)
    .bind(lucia)
    .bind(w.doc_id)
    .execute(&w.db.pool)
    .await?;
    let c = w
        .connect(
            &w.token(Tok {
                profiles: vec!["lucia"],
                ..Tok::default()
            })
            .await?,
        )
        .await?;
    let env = c
        .call("get_source", json!({"src_id": sid, "include_text": true}))
        .await?
        .ok();
    assert!(env["data"]["text"].is_null());
    assert_eq!(env["withheld"]["minor_rule"], 1);
    Ok(())
}

#[tokio::test]
async fn trend_stats_never_mix_a_point_without_a_canonical_value() -> R {
    let w = world().await?;
    let o = obs(
        "Colesterol LDL",
        "9999",
        Some("mg/dL"),
        "ldl_c",
        Some("13457-7"),
        "2026-10-20",
    )
    .printed("value_canonical", Value::Null)
    .printed("unit_canonical", Value::Null);
    add_obs(&mut *w.conn().await?, w.pid("alex"), o, None).await?;
    let c = w.connect(&w.tok().await?).await?;
    let env = c
        .call(
            "get_trend",
            json!({"profile": "alex", "code": "LOINC:13457-7", "window": "P6Y"}),
        )
        .await?
        .ok();
    let d = &env["data"];
    assert_eq!(d["resolution"], "raw");
    assert_eq!(arr(&d["points"]).len(), 9);
    assert_eq!(d["stats"]["n"], 8, "{}", d["stats"]);
    assert_eq!(d["stats"]["last_vs_previous_pct"], json!(py_round_pct(97.0, 118.0)));
    Ok(())
}
