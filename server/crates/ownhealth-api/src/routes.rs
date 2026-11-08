//! One module per area, owned as the Python router files were (RUST-CONTRACTS.md section 2).

use axum::Router;
use ownhealth_core::Runtime;

pub mod audit;
pub mod auth;
pub mod core;
pub mod documents;
pub mod drug_check;
pub mod export;
pub mod family;
pub mod genome;
pub mod imports;
pub mod mcp_usage;
pub mod observations;
pub mod records;
pub mod review;
pub mod status;
pub mod timeline;
pub mod tokens;
pub mod wearables;

/// In module-name order, as Python's `discover_routers`.
pub fn all() -> Router<Runtime> {
    Router::new()
        .merge(audit::router())
        .merge(auth::router())
        .merge(core::router())
        .merge(documents::router())
        .merge(drug_check::router())
        .merge(export::router())
        .merge(family::router())
        .merge(genome::router())
        .merge(imports::router())
        .merge(mcp_usage::router())
        .merge(observations::router())
        .merge(records::router())
        .merge(review::router())
        .merge(status::router())
        .merge(timeline::router())
        .merge(tokens::router())
        .merge(wearables::router())
}
