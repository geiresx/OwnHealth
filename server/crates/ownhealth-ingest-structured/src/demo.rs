//! Lane C demo data, imported through the real importers (`demo/hooks/apple_fhir.py`, order 300).
//!
//! - Sam and Lucía: 90 days of Apple Watch and iPhone data (steps on both, resting HR, sleep, VO2max).
//! - Alex: iPhone steps on top of the base seed's Apple Watch series, so the source-priority rule has
//!   overlap to resolve (the summary keeps showing the Watch, never the sum).
//! - Carmen: iPhone steps, and an IPS bundle with a condition, an allergy, a medication, a vaccine and
//!   a procedure. Sam's export carries two Apple clinical records (a prescription and a vaccine).
//!
//! The demo pins 7 open review items, so this hook adds no lab observations that would wait for review.

use std::io::Cursor;

use chrono::NaiveDate;
use futures::future::BoxFuture;
use ownhealth_core::access::get_profile;
use ownhealth_core::demo::DemoContext;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::{Error, Result};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::apple_health::synthetic::{ExportSpec, write_export_to};

pub const ORDER: i32 = 300;

fn sam_clinical() -> Vec<Value> {
    vec![
        json!({
            "resourceType": "MedicationRequest",
            "id": "ah-sam-cetirizine",
            "status": "active",
            "intent": "order",
            "medicationCodeableConcept": {
                "coding": [{"system": "http://www.nlm.nih.gov/research/umls/rxnorm", "code": "1014678"}],
                "text": "Cetirizine 10 mg tablet",
            },
            "authoredOn": "2026-04-03",
            "dosageInstruction": [{"text": "1 tablet a day in spring"}],
        }),
        json!({
            "resourceType": "Immunization",
            "id": "ah-sam-tdap",
            "status": "completed",
            "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "115", "display": "Tdap"}]},
            "occurrenceDateTime": "2024-02-11",
        }),
    ]
}

async fn document(
    conn: &mut PgConnection,
    ctx: &DemoContext,
    slug: &str,
    data: &[u8],
    filename: &str,
    mime: &str,
) -> Result<Uuid> {
    let sha = ctx.blobs.put_bytes(data)?;
    let doc = create_document(
        conn,
        &NewDocument {
            filename: Some(filename),
            channel: "demo",
            doc_type: Some("other"),
            ..NewDocument::upload(
                ctx.family.household_id,
                Some(ctx.family.profile(slug)),
                &sha,
                mime,
                data.len() as i64,
            )
        },
    )
    .await?;
    Ok(doc.id)
}

async fn born(conn: &mut PgConnection, ctx: &DemoContext, slug: &str) -> Result<NaiveDate> {
    get_profile(conn, ctx.family.profile(slug))
        .await?
        .and_then(|p| p.birth_date)
        .ok_or_else(|| Error::NotFound(format!("demo profile {slug} has no birth date")))
}

async fn apple(conn: &mut PgConnection, ctx: &DemoContext, slug: &str, spec: ExportSpec) -> Result<()> {
    let mut buf = Cursor::new(Vec::new());
    write_export_to(&mut buf, &spec)?;
    let doc = document(&mut *conn, ctx, slug, buf.get_ref(), "export.zip", "application/zip").await?;
    crate::apple_health::import_apple_health(conn, &ctx.blobs, doc).await?;
    Ok(())
}

async fn seed(conn: &mut PgConnection, ctx: &mut DemoContext) -> Result<()> {
    let ctx = &*ctx;
    let base = |first_name: &str, birth_date: NaiveDate, seed: u64| ExportSpec {
        first_name: first_name.into(),
        birth_date,
        end: ctx.as_of,
        days: 90,
        seed,
        ..ExportSpec::default()
    };
    let sam = born(&mut *conn, ctx, "sam").await?;
    apple(
        &mut *conn,
        ctx,
        "sam",
        ExportSpec {
            clinical: sam_clinical(),
            ..base("Sam", sam, 11)
        },
    )
    .await?;
    let lucia = born(&mut *conn, ctx, "lucia").await?;
    apple(&mut *conn, ctx, "lucia", base("Lucía", lucia, 12)).await?;
    let alex = born(&mut *conn, ctx, "alex").await?;
    apple(
        &mut *conn,
        ctx,
        "alex",
        ExportSpec {
            watch: false,
            ..base("Alex", alex, 13)
        },
    )
    .await?;
    let carmen = born(&mut *conn, ctx, "carmen").await?;
    apple(
        &mut *conn,
        ctx,
        "carmen",
        ExportSpec {
            watch: false,
            ..base("Carmen", carmen, 14)
        },
    )
    .await?;
    let bundle = crate::fhir::synthetic::ips_bundle("ips-carmen-2026", "Carmen", "Ortega", carmen, false, &[]);
    let data = crate::py::dumps(&bundle);
    let doc = document(
        &mut *conn,
        ctx,
        "carmen",
        data.as_bytes(),
        "carmen-ips.json",
        "application/fhir+json",
    )
    .await?;
    crate::fhir::import_fhir(conn, &ctx.blobs, doc).await?;
    Ok(())
}

pub(crate) fn seed_boxed<'c>(conn: &'c mut PgConnection, ctx: &'c mut DemoContext) -> BoxFuture<'c, Result<()>> {
    Box::pin(seed(conn, ctx))
}
