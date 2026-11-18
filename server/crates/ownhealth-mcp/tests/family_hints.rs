//! Cross-profile hints in the `family` section of get_health_status (data/family/hints.yaml, consent in
//! `family_share`): by rule id and relationship code only, behind the same gates as the pedigree. A
//! token without read:genome sees no genome hint and so no gene; a token with read:genome sees a
//! sensitive gene only through the profile's agents switch and read:sensitive. The page stays under
//! 2 KB with hints, patterns and due items at their maximum. Synthetic data only.

mod common;

use common::*;
use ownhealth_core::access::set_grant;
use ownhealth_core::family::{self, FamilyCondition, MemberIn};
use ownhealth_core::provenance::SrcRef;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

async fn relative(c: &mut PgConnection, of: Uuid, code: &str, link: Option<Uuid>, kind: Option<(&str, i32)>) -> R {
    let mut m = MemberIn::new(of, code);
    if let Some((kind, onset)) = kind {
        m.conditions = vec![FamilyCondition {
            kind: kind.into(),
            text: None,
            code_system: None,
            code: None,
            onset_age: Some(onset),
            onset_approx: false,
        }];
    }
    family::add(&mut *c, &m, &SrcRef::New(manual(of)), "person", None, link).await?;
    Ok(())
}

async fn share(c: &mut PgConnection, source: Uuid, target: Uuid, categories: &[&str]) -> R {
    sqlx::query(
        "insert into family_share (source_profile_id, target_profile_id, categories) values ($1, $2, $3) \
         on conflict (source_profile_id, target_profile_id) do update set categories = excluded.categories",
    )
    .bind(source)
    .bind(target)
    .bind(categories.iter().map(|c| c.to_string()).collect::<Vec<_>>())
    .execute(c)
    .await?;
    Ok(())
}

async fn ldl(c: &mut PgConnection, profile_id: Uuid, value: &str) -> anyhow::Result<String> {
    let o = obs(
        "Colesterol LDL",
        value,
        Some("mg/dL"),
        "ldl_c",
        Some("13457-7"),
        "2026-06-10",
    )
    .high("130");
    add_obs(c, profile_id, o, None).await
}

async fn sf(c: &mut PgConnection, profile_id: Uuid, genes: &[&str]) -> R {
    let ds = add_dataset(&mut *c, profile_id).await?;
    for gene in genes {
        let f = Finding {
            sensitive: true,
            adult_onset: true,
            ..Finding::default()
        };
        add_finding(&mut *c, profile_id, ds, &fact_hex(), "acmg_sf", Some(gene), f).await?;
    }
    Ok(())
}

async fn status(w: &World, tok: &str, slug: &str) -> anyhow::Result<ToolRes> {
    w.connect_real(tok)
        .await?
        .call_tool("get_health_status", json!({"profile": slug}))
        .await
}

/// Alex's mother is Carmen's profile; Carmen has an LDL-C of 247 mg/dL and a BRCA2 P/LP.
async fn carmen_shares(w: &World, categories: &[&str]) -> anyhow::Result<String> {
    let mut c = w.conn().await?;
    relative(&mut c, w.pid("alex"), "MTH", Some(w.pid("carmen")), None).await?;
    let src = ldl(&mut c, w.pid("carmen"), "247").await?;
    sf(&mut c, w.pid("carmen"), &["BRCA2"]).await?;
    share(&mut c, w.pid("carmen"), w.pid("alex"), categories).await?;
    Ok(src)
}

#[tokio::test]
async fn the_family_section_names_a_hint_by_rule_and_relationship_the_same_for_every_token() -> R {
    let w = world().await?;
    let src = carmen_shares(&w, &["labs"]).await?;
    let mut seen = Vec::new();
    for (lang, mode) in [("en", "full"), ("es", "pseudonymous"), ("en", "minimal")] {
        let tok = w
            .token(Tok {
                lang,
                identity_mode: mode,
                ..Tok::default()
            })
            .await?;
        let r = status(&w, &tok, "alex").await?;
        let env = r.ok();
        let fam = &env["data"]["family"];
        assert_eq!(fam["hints"], json!([{"rule": "lab.ldl_fh", "rel": "MTH"}]), "{env}");
        assert_eq!(fam["family_hash12"].as_str().map(str::len), Some(12), "{fam}");
        for leak in ["http", "Carmen", "carmen", "247", src.as_str()] {
            assert!(!fam.to_string().contains(leak), "{leak} in {fam}");
        }
        assert!(
            r.text.contains("family.hints:\n- rule=lab.ldl_fh rel=MTH"),
            "{}",
            r.text
        );
        seen.push(fam.to_string());
    }
    assert!(seen.iter().all(|s| s == &seen[0]), "{seen:?}");
    // the hint is a lab: without read:labs there is none, and nothing else is in the section here
    let tok = w.tok_scopes(without(&["read:labs"])).await?;
    assert!(status(&w, &tok, "alex").await?.ok()["data"].get("family").is_none());
    // a revoke removes it at once
    sqlx::query("delete from family_share").execute(&w.db.pool).await?;
    let tok = w.tok().await?;
    assert!(status(&w, &tok, "alex").await?.ok()["data"].get("family").is_none());
    Ok(())
}

#[tokio::test]
async fn a_token_sees_a_gene_only_with_read_genome_read_sensitive_and_the_agents_switch() -> R {
    let w = world().await?;
    carmen_shares(&w, &["cascade"]).await?;
    let gene = |env: &Value| env["data"]["family"]["hints"][0].clone();
    // read:records without read:genome: no genome hint, no gene anywhere in the answer
    let tok = w.tok_scopes(without(&["read:genome"])).await?;
    let r = status(&w, &tok, "alex").await?;
    assert!(r.ok()["data"].get("family").is_none(), "{}", r.ok());
    assert!(!r.text.contains("BRCA2") && !r.ok().to_string().contains("BRCA2"));
    // read:genome and read:sensitive with Alex's agents switch off: the hint, no gene
    w.set_switch("alex", false).await?;
    let tok = w.tok().await?;
    let r = status(&w, &tok, "alex").await?;
    assert_eq!(
        gene(&r.ok()),
        json!({"rule": "genome.cascade", "rel": "MTH", "withheld": true})
    );
    assert!(!r.text.contains("BRCA2"), "{}", r.text);
    // the switch on but no read:sensitive: still no gene
    w.set_switch("alex", true).await?;
    let tok = w.tok_scopes(without(&["read:sensitive"])).await?;
    assert_eq!(gene(&status(&w, &tok, "alex").await?.ok())["withheld"], true);
    // the control: everything open names it
    let tok = w.tok().await?;
    assert_eq!(gene(&status(&w, &tok, "alex").await?.ok())["gene"], "BRCA2");
    Ok(())
}

/// The status of a minor never carries a genome hint, and her own results feed no one's.
#[tokio::test]
async fn no_genome_hint_from_or_to_a_minor_in_the_status() -> R {
    let w = world().await?;
    let mut c = w.conn().await?;
    relative(&mut c, w.pid("lucia"), "FTH", Some(w.pid("alex")), None).await?;
    relative(&mut c, w.pid("alex"), "DAU", Some(w.pid("lucia")), None).await?;
    ldl(&mut c, w.pid("alex"), "250").await?;
    ldl(&mut c, w.pid("lucia"), "205").await?;
    sf(&mut c, w.pid("alex"), &["BRCA2"]).await?;
    sf(&mut c, w.pid("lucia"), &["MLH1"]).await?;
    share(&mut c, w.pid("alex"), w.pid("lucia"), &["labs", "cascade"]).await?;
    share(&mut c, w.pid("lucia"), w.pid("alex"), &["labs", "cascade"]).await?;
    drop(c);
    let tok = w
        .token(Tok {
            profiles: vec!["alex", "lucia"],
            ..Tok::default()
        })
        .await?;
    // Alex as Lucía's guardian: the lab hint about her, never the genome one
    let lucia = status(&w, &tok, "lucia").await?.ok();
    assert_eq!(
        lucia["data"]["family"]["hints"],
        json!([{"rule": "lab.ldl_fh", "rel": "FTH"}]),
        "{lucia}"
    );
    // Lucía is never a source
    let alex = status(&w, &tok, "alex").await?.ok();
    assert!(alex["data"].get("family").is_none(), "{alex}");
    Ok(())
}

/// The demo household, with Carmen's page at its fullest: every open screening, a relative for every
/// pattern, and four hints from her son.
#[tokio::test]
async fn status_with_hints_patterns_and_due_at_maximum_stays_under_2kb() -> R {
    let w = world().await?;
    ownhealth_ops::register();
    ownhealth_genome::register();
    assert!(ownhealth_core::demo::seed_demo(&w.db.pool, &w.rt.settings, &w.rt.blobs, false).await?);
    let pool = w.db.pool.clone();
    let id = |slug: &'static str| {
        let pool = pool.clone();
        async move {
            let row: (Uuid, Uuid) = sqlx::query_as(
                "select p.id, coalesce(p.user_id, (select u.id from app_user u where u.household_id = h.id \
             order by u.created_at limit 1)) from profile p join household h on h.id = p.household_id \
             where h.name = $1 and p.slug = $2",
            )
            .bind(ownhealth_core::demo::DEMO_HOUSEHOLD)
            .bind(slug)
            .fetch_one(&pool)
            .await?;
            anyhow::Ok(row)
        }
    };
    let (carmen, alex_user) = id("carmen").await?;
    let (alex, _) = id("alex").await?;
    let mut c = w.conn().await?;
    for (code, kind, onset) in [
        ("FTH", "mi", 50),
        ("PUNCLE", "mi", 60),
        ("MTH", "cancer_colon", 50),
        ("MAUNT", "cancer_ovary", 45),
        ("SIS", "hereditary_variant", 40),
        ("BRO", "mi", 40),
        ("PGRMTH", "cancer_breast", 70),
        ("MGRMTH", "cancer_breast", 70),
    ] {
        relative(&mut c, carmen, code, None, Some((kind, onset))).await?;
    }
    relative(&mut c, carmen, "SON", Some(alex), None).await?;
    ldl(&mut c, alex, "250").await?;
    sf(&mut c, alex, &["BRCA2", "MLH1", "LDLR"]).await?;
    share(&mut c, alex, carmen, &["labs", "cascade"]).await?;
    // Alex as Carmen's guardian reads her genome; her agents switch on names the genes
    set_grant(&mut c, alex_user, carmen, "guardian", None, None, None).await?;
    ownhealth_core::access::set_genome_switches(&mut c, carmen, Some(true), Some(true)).await?;
    let tok = ownhealth_auth::tokens::create_token(
        &mut c,
        &ownhealth_auth::tokens::TokenIn {
            user_id: alex_user,
            label: "t",
            profile_ids: vec![carmen],
            scopes: all().into_iter().collect(),
            default_profile: None,
            days: 90,
            identity_mode: "full",
            lang: "es",
            units: "si",
        },
    )
    .await?
    .plaintext;
    drop(c);
    let env = status(&w, &tok, "carmen").await?.ok();
    let size = ownhealth_mcp::envelope::dumps(&env).len();
    assert!(size < 2048, "{size}");
    let fam = &env["data"]["family"];
    assert_eq!(fam["hint_total"], 4, "{fam}");
    assert_eq!(
        fam["hints"],
        json!([
            {"rule": "lab.ldl_fh", "rel": "SON"},
            {"rule": "genome.cascade", "rel": "SON", "gene": "BRCA2"},
            {"rule": "genome.cascade", "rel": "SON", "gene": "LDLR"},
        ]),
        "{fam}"
    );
    assert!(fam["items"].as_array().is_some_and(|i| !i.is_empty()), "{fam}");
    assert!(
        env["data"]["due"]["items"].as_array().is_some_and(|i| !i.is_empty()),
        "{env}"
    );
    Ok(())
}
