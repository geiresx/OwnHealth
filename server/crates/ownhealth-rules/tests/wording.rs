//! The wording lint (0.1.1 tests/state/test_wording.py): 0 imperative phrases in everything lane E
//! writes, and proof that it can fail, on known-bad sentences and on the CPIC quotes themselves.

mod common;

use common::{as_of, seeded};
use ownhealth_core::access::all_scopes;
use ownhealth_rules::wording::{SKIP_KEYS, imperative_phrases, lint};
use ownhealth_rules::{build_state, check_drug, dashboard, load_pack, render};

type R = anyhow::Result<()>;

const BAD: [&str; 11] = [
    "You should take vitamin D in winter.",
    "Take 1000 IU daily.",
    "Stop atorvastatin before the test.",
    "Avoid codeine.",
    "Discuss with your prescriber.",
    "Do not drink alcohol.",
    "Tome omeprazol antes del desayuno.",
    "Deberías repetir la TSH.",
    "No tome codeína.",
    "Consulte a su médico.",
    "Dosis: suspenda la estatina.",
];

#[test]
fn lint_catches_known_bad_sentences() {
    for s in BAD {
        assert!(!imperative_phrases(s).is_empty(), "{s}");
    }
}

#[test]
fn lint_passes_our_sentences() {
    for ok in [
        "To discuss with your prescriber.",
        "Flags use each lab's own printed range.",
        "Para comentar con quien lo prescribe.",
        "Control de TSH",
    ] {
        assert_eq!(imperative_phrases(ok), Vec::<String>::new(), "{ok}");
    }
}

#[test]
fn every_template_is_information_not_advice() -> R {
    let p = load_pack()?;
    let mut hits = Vec::new();
    for (k, t) in &p.templates {
        for (lang, s) in t.as_object().into_iter().flatten() {
            for h in imperative_phrases(s.as_str().unwrap_or_default()) {
                hits.push((k.clone(), lang.clone(), h));
            }
        }
    }
    assert_eq!(hits, Vec::new());
    Ok(())
}

#[tokio::test]
async fn status_and_dashboard_text_has_no_imperatives() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let mut hits = Vec::new();
    for slug in ["alex", "sam", "lucia", "carmen"] {
        let pid = s.pid(slug);
        let body = build_state(&mut tx, pid, as_of(), None).await?;
        for lang in ["en", "es"] {
            for mode in ["full", "pseudonymous", "minimal"] {
                hits.extend(lint(&render(&body, lang, "as_reported", mode, None)?, &SKIP_KEYS));
            }
            hits.extend(lint(
                &dashboard(&mut tx, pid, as_of(), lang, &all_scopes()).await?,
                &SKIP_KEYS,
            ));
        }
    }
    assert_eq!(hits, Vec::new());
    Ok(())
}

#[tokio::test]
async fn check_drug_has_no_imperatives_outside_the_cpic_quotes() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let (mut hits, mut quotes) = (Vec::new(), 0);
    let mut drugs: Vec<String> = load_pack()?.drugs.keys().cloned().collect();
    drugs.sort();
    for slug in ["alex", "sam"] {
        for drug in &drugs {
            for lang in ["en", "es"] {
                let out = check_drug(&mut tx, s.pid(slug), drug, &all_scopes(), lang, Some(as_of())).await?;
                hits.extend(
                    lint(&out, &SKIP_KEYS)
                        .into_iter()
                        .map(|h| (slug, drug.clone(), lang, h)),
                );
                quotes += out["guidance"].as_array().map_or(0, Vec::len);
            }
        }
    }
    assert_eq!(hits, Vec::new());
    assert!(quotes > 20, "{quotes}"); // the loop really produced quoted guidance
    Ok(())
}

/// The 0 above comes from skipping verbatim CPIC text, not from a lint that cannot see.
#[tokio::test]
async fn the_quotes_alone_would_fail_the_lint() -> R {
    let s = seeded().await?;
    let mut tx = s.pool().begin().await?;
    let out = check_drug(&mut tx, s.pid("sam"), "codeine", &all_scopes(), "en", Some(as_of())).await?;
    assert_eq!(lint(&out, &SKIP_KEYS), Vec::new());
    assert_ne!(lint(&out, &[]), Vec::new()); // "Use codeine label recommended ..." is an imperative
    Ok(())
}

#[test]
fn the_genome_notice_is_information_not_advice() {
    assert_eq!(imperative_phrases(ownhealth_genome::NOTICE), Vec::<String>::new());
}
