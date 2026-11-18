//! `GET /api/profiles/{p}/genome/events`: the ClinVar diff's history for one profile (genomics.md 9),
//! newest first, `read:genome`. Events of a genome whose file was retracted are gone, as its findings are.
//!
//! The reader's own gate decides what each event shows: the gene, the VariationID and both
//! classifications only when the sensitive switch and read:sensitive open it and the minor rule does
//! not hold it. Otherwise the event reads "a withheld result changed", with nothing that names it, and
//! is counted in `withheld`. One audit row per read, identifiers only.
//!
//! Beside the events: `release`, the newest ClinVar release compared for this profile's genomes (null
//! before the first comparison); `weekly`, whether the household's weekly ClinVar check is on; and
//! `rerun`, where a reclassified call is made again (OwnGenome; OwnHealth only compares).

use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use chrono::{DateTime, Utc};
use ownhealth_core::Runtime;
use ownhealth_core::access::Principal;
use ownhealth_core::audit::write_audit;
use ownhealth_core::pyjson::py_isoformat_utc;
use ownhealth_genome::clinvar::{Before, EventText, rerun_text, summary};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{event, gate};
use crate::extract::Caller;
use crate::routes::auth::{R, ok};

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    variation_id: i64,
    kind: String,
    gene: Option<String>,
    sensitive: bool,
    adult_onset: bool,
    carrier: bool,
    old: sqlx::types::Json<Value>,
    new: sqlx::types::Json<Value>,
    release_from: Option<String>,
    release_to: String,
    alerted: bool,
    created_at: DateTime<Utc>,
    assay: String,
}

fn class(v: &Value) -> Option<&'static str> {
    match v.get("class").and_then(Value::as_str)? {
        "plp" => Some("plp"),
        "vus" => Some("vus"),
        "conflicting" => Some("conflicting"),
        "blb" => Some("blb"),
        _ => Some("other"),
    }
}

/// `GET /profiles/{p}/genome/events` for one principal.
pub async fn events(rt: &Runtime, principal: &Principal, p: &str) -> R<Value> {
    let mut tx = rt.pool.begin().await?;
    let perm = Caller(principal.clone())
        .require(&mut tx, p, Some("read:genome"), None)
        .await?;
    let gate = gate(rt, &mut tx, &perm, principal).await?;
    let rows: Vec<Row> = sqlx::query_as(
        "select e.id, e.variation_id, e.kind, e.gene, e.sensitive, e.adult_onset, e.carrier, e.old, e.new, \
         e.release_from, e.release_to, e.alerted, e.created_at, d.assay from genome_event e \
         join genome_dataset d on d.id = e.dataset_id where e.profile_id = $1 and d.status <> 'retracted' \
         and not exists (select 1 from document doc where doc.id = d.document_id and doc.status = 'retracted') \
         order by e.created_at desc, e.release_to desc, e.variation_id",
    )
    .bind(perm.profile.id)
    .fetch_all(&mut *tx)
    .await?;
    let (mut withheld_sensitive, mut withheld_minor) = (0, 0);
    let mut out = Vec::new();
    for r in &rows {
        let reason = if gate.minor && (r.sensitive || r.adult_onset || r.carrier) {
            Some("minor")
        } else if r.sensitive && !gate.sensitive {
            Some("sensitive")
        } else {
            None
        };
        let unconfirmed = matches!(r.assay.as_str(), "chip" | "chip_imputed");
        let old = class(&r.old.0).map(|c| Before {
            class: c,
            stars: r
                .old
                .0
                .get("stars")
                .and_then(Value::as_i64)
                .and_then(|s| i16::try_from(s).ok()),
        });
        let new_stars = r
            .new
            .0
            .get("stars")
            .and_then(Value::as_i64)
            .and_then(|s| i16::try_from(s).ok());
        let text = EventText {
            gene: r.gene.as_deref(),
            old: old.as_ref(),
            new_class: class(&r.new.0).unwrap_or("other"),
            new_stars: new_stars.unwrap_or(0),
            withheld: reason.is_some(),
            unconfirmed,
            not_in_previous_release: r.old.0.get("in_previous_release") == Some(&Value::Bool(false)),
        };
        let mut e = json!({
            "id": r.id,
            "release_from": r.release_from,
            "release_to": r.release_to,
            "unconfirmed": unconfirmed,
            "created_at": py_isoformat_utc(&r.created_at),
            "text": {"en": summary(&text, "en"), "es": summary(&text, "es")},
        });
        match reason {
            Some(why) => {
                if why == "minor" {
                    withheld_minor += 1;
                } else {
                    withheld_sensitive += 1;
                }
                e["withheld"] = json!(why);
            }
            None => {
                e["alerted"] = json!(r.alerted);
                e["kind"] = json!(r.kind);
                e["gene"] = json!(r.gene);
                e["variation_id"] = json!(r.variation_id);
                e["old"] = r.old.0.clone();
                e["new"] = r.new.0.clone();
            }
        }
        out.push(e);
    }
    let withheld = json!({"sensitive": withheld_sensitive, "minor_rule": withheld_minor});
    let release: Option<chrono::NaiveDate> = sqlx::query_scalar(
        "select max(d.clinvar_compared) from genome_dataset d where d.profile_id = $1 and d.status = 'current' \
         and not exists (select 1 from document doc where doc.id = d.document_id and doc.status = 'retracted')",
    )
    .bind(perm.profile.id)
    .fetch_one(&mut *tx)
    .await?;
    let weekly: bool = sqlx::query_scalar(
        "select exists (select 1 from source where household_id = $1 and kind = 'clinvar' and enabled)",
    )
    .bind(perm.profile.household_id)
    .fetch_one(&mut *tx)
    .await?;
    let mut a = event(principal, &perm, "read", "GET /api/profiles/{p}/genome/events");
    a.resource_type = Some("genome_event".into());
    a.resource_ids = Some(rows.iter().map(|r| r.id.to_string()).collect());
    a.rows_out = i32::try_from(rows.len()).ok();
    a.withheld = Some(withheld.clone());
    write_audit(&mut tx, &a).await?;
    tx.commit().await?;
    Ok(json!({
        "events": out,
        "withheld": withheld,
        "release": release.map(|d| d.to_string()),
        "weekly": weekly,
        "rerun": {"en": rerun_text("en"), "es": rerun_text("es")},
    }))
}

async fn genome_events(State(rt): State<Runtime>, Caller(c): Caller, Path(p): Path<String>) -> R {
    ok(events(&rt, &c, &p).await?)
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/profiles/{p}/genome/events", get(genome_events))
}
