//! `data/genome/pgx_releases.json` (the latest PharmCAT release and the OwnGenome steps that make a
//! PGx call again) is a cited data file: a version, and an https source on every row.

use ownhealth_rules::cited;

#[test]
fn pgx_releases_is_a_cited_file() -> anyhow::Result<()> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/genome/pgx_releases.json");
    let f = cited::load(&path)?;
    assert!(f.version.starts_with("pgx_releases "), "{}", f.version);
    let ids: Vec<&str> = f.rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(
        ids,
        ["pharmcat", "owngenome.step.36", "owngenome.step.7", "owngenome.step.27"]
    );
    Ok(())
}
