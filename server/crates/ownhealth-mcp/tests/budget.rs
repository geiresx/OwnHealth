//! `tests/mcp/test_mcp_budget.py`: response budgets (~4k tokens default, 15k max, status under 2 KB),
//! cursor pagination that walks every row exactly once, and cursors that only work for their own
//! query and token.

mod common;

use common::*;
use serde_json::{Value, json};

const N_GROUPS: usize = 100;
const PER_GROUP: usize = 6;

async fn big(w: &World) -> R {
    let carmen = w.pid("carmen");
    let mut c = w.conn().await?;
    for g in 0..N_GROUPS {
        for i in 0..PER_GROUP {
            let o = obs(
                &format!("Analito número {g} de prueba"),
                &(50 + i).to_string(),
                Some("mg/dL"),
                &format!("analyte_{g:03}"),
                Some(&format!("{}-{}", 10000 + g, g % 10)),
                &format!("20{}-0{}-1{}", 18 + i, 1 + g % 9, i % 9),
            )
            .low("40")
            .high("60")
            .flag(if i > 6 { "H" } else { "N" });
            add_obs(&mut c, carmen, o, None).await?;
        }
    }
    Ok(())
}

fn measured(r: &ToolRes) -> usize {
    ownhealth_mcp::envelope::tokens_of(&dumps(&r.sc)) + ownhealth_mcp::envelope::tokens_of(&r.text)
}

fn size(v: &Value) -> usize {
    dumps(v).len()
}

async fn walk(c: &Client, args: Value) -> anyhow::Result<Vec<(ToolRes, Value)>> {
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut a = args.clone();
        if let Some(cur) = &cursor {
            a["cursor"] = json!(cur);
        }
        let r = c.call_tool("search_observations", a).await?;
        let env = r.ok();
        cursor = env["next_cursor"].as_str().map(str::to_string);
        pages.push((r, env));
        if cursor.is_none() {
            return Ok(pages);
        }
        assert!(pages.len() < 100);
    }
}

fn carmen_tok() -> Tok {
    Tok {
        profiles: vec!["carmen"],
        user: "alex",
        ..Tok::default()
    }
}

#[tokio::test]
async fn full_detail_pages_stay_under_15k_and_cover_everything() -> R {
    let w = world().await?;
    big(&w).await?;
    let c = w.connect(&w.token(carmen_tok()).await?).await?;
    let pages = walk(&c, json!({"profile": "carmen", "query": "Analito", "detail": "full"})).await?;
    assert!(pages.len() > 1);
    let mut seen = Vec::new();
    for (r, env) in &pages {
        assert!(measured(r) <= 15000, "{}", measured(r));
        assert!(env["budget"]["tokens_est"].as_u64().unwrap_or(u64::MAX) <= 15000);
        seen.extend(facts(&env["data"]).into_iter().map(|f| f["src"].to_string()));
    }
    let uniq: std::collections::BTreeSet<_> = seen.iter().collect();
    assert_eq!(seen.len(), N_GROUPS * PER_GROUP);
    assert_eq!(uniq.len(), seen.len());
    let truncated: Vec<Value> = pages.iter().map(|(_, e)| e["budget"]["truncated"].clone()).collect();
    let mut want = vec![json!(true); pages.len() - 1];
    want.push(json!(false));
    assert_eq!(truncated, want);
    Ok(())
}

#[tokio::test]
async fn summary_default_stays_under_4k() -> R {
    let w = world().await?;
    big(&w).await?;
    let c = w.connect(&w.token(carmen_tok()).await?).await?;
    let pages = walk(&c, json!({"profile": "carmen", "query": "Analito"})).await?;
    let total: usize = pages
        .iter()
        .map(|(_, e)| e["data"]["results"].as_array().map_or(0, Vec::len))
        .sum();
    assert_eq!(total, N_GROUPS);
    for (r, _) in &pages {
        assert!(measured(r) <= 4000, "{}", measured(r));
    }
    Ok(())
}

#[tokio::test]
async fn trend_and_status_budgets() -> R {
    let w = world().await?;
    big(&w).await?;
    w.fakes.set_state(StateMode::ManyFlags);
    let c = w.connect(&w.token(carmen_tok()).await?).await?;
    let first = c
        .call_tool("get_health_status", json!({"profile": "carmen"}))
        .await?
        .ok();
    assert!(size(&first) < 2048, "{}", size(&first));
    assert_eq!(first["budget"]["truncated"], true);
    assert!(truthy(&first["next_cursor"]));
    let ranks: Vec<i64> = first["data"]["flags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["rank"].as_i64())
        .collect();
    let second = c
        .call_tool(
            "get_health_status",
            json!({"profile": "carmen", "cursor": first["next_cursor"]}),
        )
        .await?
        .ok();
    assert_eq!(second["data"]["flags"][0]["rank"], json!(ranks[ranks.len() - 1] + 1));
    assert_eq!(second["data"]["state_hash"], first["data"]["state_hash"]);
    let trend = c
        .call_tool(
            "get_trend",
            json!({"profile": "carmen", "code": "10000-0", "window": "all"}),
        )
        .await?;
    assert!(!trend.is_error, "{}", trend.sc);
    assert!(measured(&trend) <= 2000, "{}", measured(&trend));
    Ok(())
}

#[tokio::test]
async fn cursor_is_bound_to_query_and_token() -> R {
    let w = world().await?;
    big(&w).await?;
    let t1 = w
        .token(Tok {
            profiles: vec!["carmen"],
            ..Tok::default()
        })
        .await?;
    let t2 = w
        .token(Tok {
            profiles: vec!["carmen"],
            ..Tok::default()
        })
        .await?;
    let args = json!({"profile": "carmen", "query": "Analito", "detail": "full"});
    let c = w.connect(&t1).await?;
    let cursor = c.call_tool("search_observations", args.clone()).await?.ok()["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_string();
    let mut a = args.clone();
    a["query"] = json!("otro");
    a["cursor"] = json!(cursor);
    assert_eq!(c.call_tool("search_observations", a).await?.err()["code"], "bad_input");
    let (body, mac) = cursor.split_once('.').expect("body.mac");
    let tail = if &body[body.len() - 2..] != "AA" { "AA" } else { "BB" };
    let forged = format!("{}{tail}.{mac}", &body[..body.len() - 2]);
    let mut a = args.clone();
    a["cursor"] = json!(forged);
    assert_eq!(c.call_tool("search_observations", a).await?.err()["code"], "bad_input");
    let c = w.connect(&t2).await?;
    let mut a = args.clone();
    a["cursor"] = json!(cursor);
    assert_eq!(c.call_tool("search_observations", a).await?.err()["code"], "bad_input");
    Ok(())
}

#[tokio::test]
async fn as_of_answers_what_the_store_knew() -> R {
    let w = world().await?;
    let c = w.connect(&w.tok().await?).await?;
    let args = |d: &str| json!({"profile": "alex", "loinc": "13457-7", "detail": "full", "as_of": d});
    let then = c.call_tool("search_observations", args("2024-01-01")).await?.ok();
    let later = c.call_tool("search_observations", args("2099-01-01")).await?.ok();
    assert_eq!(then["data"]["results"], json!([]));
    assert_eq!(then["data"]["not_measured"], true);
    assert_eq!(then["as_of"], "2024-01-01");
    assert_eq!(later["data"]["results"].as_array().map(Vec::len), Some(8));
    Ok(())
}

#[tokio::test]
async fn demo_alex_status_pages_stay_under_2kb_whole() -> R {
    let w = world().await?;
    ownhealth_ops::register();
    ownhealth_genome::register();
    assert!(ownhealth_core::demo::seed_demo(&w.db.pool, &w.rt.settings, &w.rt.blobs, false).await?);
    let (u, p): (uuid::Uuid, uuid::Uuid) = sqlx::query_as(
        "select u.id, p.id from app_user u join household h on h.id = u.household_id \
         join profile p on p.household_id = h.id and p.slug = 'alex' where h.name = $1 order by u.created_at limit 1",
    )
    .bind(ownhealth_core::demo::DEMO_HOUSEHOLD)
    .fetch_one(&w.db.pool)
    .await?;
    let tok = ownhealth_auth::tokens::create_token(
        &mut *w.conn().await?,
        &ownhealth_auth::tokens::TokenIn {
            user_id: u,
            label: "t",
            profile_ids: vec![p],
            scopes: all().into_iter().collect(),
            default_profile: None,
            days: 90,
            identity_mode: "full",
            lang: "en",
            units: "si",
        },
    )
    .await?
    .plaintext;
    let c = w.connect_real(&tok).await?;
    let (mut flags, mut cursor, mut pages) = (Vec::new(), None::<String>, 0);
    loop {
        let mut args = json!({"profile": "alex"});
        if let Some(cur) = &cursor {
            args["cursor"] = json!(cur);
        }
        let env = c.call_tool("get_health_status", args).await?.ok();
        assert!(size(&env) < 2048, "page {pages}: {}", size(&env));
        flags.extend(env["data"]["flags"].as_array().cloned().unwrap_or_default());
        pages += 1;
        cursor = env["next_cursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    let ranks: Vec<i64> = flags.iter().filter_map(|f| f["rank"].as_i64()).collect();
    assert_eq!(ranks, (1..=flags.len() as i64).collect::<Vec<_>>());
    assert!(flags.len() >= 5, "{}", flags.len());
    let allowed = ["rank", "kind", "rule", "code", "text", "src"];
    for f in &flags {
        assert!(
            f.as_object()
                .is_some_and(|m| m.keys().all(|k| allowed.contains(&k.as_str()))),
            "{f}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn long_notes_do_not_push_the_status_past_2kb() -> R {
    let w = world().await?;
    w.fakes.set_state(StateMode::Noted);
    let c = w.connect(&w.token(carmen_tok()).await?).await?;
    let env = c
        .call_tool("get_health_status", json!({"profile": "carmen"}))
        .await?
        .ok();
    assert!(size(&env) < 2048, "{}", size(&env));
    let notes = env["data"]["notes"].as_array().cloned().unwrap_or_default();
    let srcs: Vec<Value> = notes.iter().map(|n| n["src"].clone()).collect();
    assert_eq!(srcs, vec![json!("src_n1"), json!("src_n2"), json!("src_n3")]);
    assert!(
        notes
            .iter()
            .all(|n| n["text"]["truncated"] == true && n["text"]["trust"] == "untrusted_text")
    );
    assert_eq!(env["data"]["state_hash"], "sha256:abc");
    Ok(())
}

#[tokio::test]
async fn check_drug_takes_back_its_own_cursor() -> R {
    let w = world().await?;
    w.fakes.set_drug(DrugMode::ManyLabs);
    let c = w.connect(&w.tok().await?).await?;
    let first = c
        .call_tool("check_drug", json!({"profile": "alex", "drug": "atorvastatin"}))
        .await?
        .ok();
    assert!(truthy(&first["next_cursor"]));
    let second = c
        .call_tool(
            "check_drug",
            json!({"profile": "alex", "drug": "atorvastatin", "cursor": first["next_cursor"]}),
        )
        .await?
        .ok();
    let seen = first["data"]["relevant_labs"].as_array().map_or(0, Vec::len);
    assert_eq!(second["data"]["relevant_labs"][0]["src"], format!("src_l{seen}"));
    Ok(())
}
