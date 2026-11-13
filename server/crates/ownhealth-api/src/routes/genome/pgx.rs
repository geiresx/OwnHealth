//! The PGx gene grid of `GET /api/profiles/{p}/genome` (genomics.md 6, v0.3): one row per CPIC gene
//! with its call or why there is none, and the current medicines from the person's records that CPIC
//! ties to that gene. The medicines need read:records; without it the grid says so (`meds: null`).

use std::collections::BTreeMap;

use ownhealth_core::Result;
use ownhealth_core::access::Permission;
use ownhealth_core::records::active_medications;
use ownhealth_genome::Findings;
use ownhealth_genome::findings::{genes_for_drug_name, pgx_grid};
use ownhealth_rules::engine::{med_drug, str_of};
use ownhealth_rules::load_pack;
use serde_json::{Value, json};
use sqlx::PgConnection;

/// Gene -> the current medicines (name, kind, src) whose CPIC drug that gene guides.
async fn meds_by_gene(conn: &mut PgConnection, perm: &Permission) -> Result<BTreeMap<String, Vec<Value>>> {
    let pack = load_pack()?;
    let meds = active_medications(conn, perm.profile.id, None, None, None, perm.has("read:sensitive")).await?;
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for m in &meds {
        let Some(drug) = med_drug(&pack, m) else { continue };
        // the label the drug check shows: "Atorvastatin 20 mg"
        let name: Vec<&str> = [str_of(m, "name"), str_of(m, "strength")]
            .into_iter()
            .flatten()
            .filter(|x| !x.is_empty())
            .collect();
        for gene in genes_for_drug_name(&drug) {
            out.entry(gene).or_default().push(json!({
                "name": name.join(" "),
                "kind": str_of(m, "kind"),
                "src": str_of(m, "src_id"),
            }));
        }
    }
    Ok(out)
}

/// The grid for one reader.
pub async fn grid(conn: &mut PgConnection, perm: &Permission, found: &Findings) -> Result<Vec<Value>> {
    let meds = if perm.has("read:records") {
        Some(meds_by_gene(conn, perm).await?)
    } else {
        None
    };
    Ok(pgx_grid(found, meds.as_ref()))
}
