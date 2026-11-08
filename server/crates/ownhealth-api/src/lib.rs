//! The REST API under `/api` (`api/`). One module per Python router file in `routes/`, each owned
//! by the lane that owns the Python file (RUST-CONTRACTS.md). A module exposes
//! `pub fn router() -> Router<Runtime>`; `router()` below merges them all, so adding a route is
//! editing your own module only.
//!
//! A handler looks like this:
//!
//! ```ignore
//! async fn observations(rt: State<Runtime>, caller: Caller, Path(p): Path<String>) -> ApiResult<Json<Value>> {
//!     let mut tx = rt.pool.begin().await?;            // one transaction per request
//!     let perm = caller.require(&mut tx, &p, Some("read:labs"), None).await?;
//!     ...
//!     tx.commit().await?;
//!     Ok(Json(out))
//! }
//! ```

pub mod error;
pub mod extract;
pub mod routes;
pub mod validate;

use axum::Router;
use ownhealth_core::Runtime;

pub use error::{ApiError, ApiResult};
pub use extract::{Caller, SessionCaller};

/// Every area's router, to be nested at `/api`.
pub fn router() -> Router<Runtime> {
    routes::all()
}
