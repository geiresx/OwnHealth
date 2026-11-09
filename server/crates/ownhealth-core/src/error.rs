//! The one error type of the core. Each variant maps to one HTTP answer in `ownhealth-api`
//! (`ApiError`): the same codes the Python app returned.

/// What a core function can fail with. Lanes wrap it or add their own error types; the HTTP mapping
/// lives in `ownhealth_api::ApiError`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No grant, a token not allowed on the profile, a missing scope or role action. `code` is stable:
    /// `profile_not_granted` (HTTP 404), `profile_required`, `insufficient_scope`, `forbidden` (403).
    #[error("{message}")]
    AccessDenied {
        code: String,
        message: String,
        allowed: Vec<String>,
    },
    /// A review queue conflict (item not open, edits that move a fact). HTTP 409 `conflict`.
    #[error("{0}")]
    Review(String),
    /// A lifecycle conflict (superseding a superseded row, retracting without a reason). HTTP 409.
    #[error("{0}")]
    Lifecycle(String),
    /// A provenance rule broken (unknown src, another profile's src). HTTP 409.
    #[error("{0}")]
    Provenance(String),
    /// An upload nobody can import. HTTP 415 `unsupported_upload`.
    #[error("{0}")]
    Upload(String),
    /// A value that fails validation (the Python ValueError / pydantic error). HTTP 422.
    #[error("{0}")]
    Invalid(String),
    /// A row the caller named does not exist. HTTP 404 `unknown` (the code the Python routes sent).
    #[error("{0}")]
    NotFound(String),
    /// A v0.2 interface whose lane has not landed yet (RUST-CONTRACTS.md section 10). HTTP 501
    /// `not_implemented`. None may be left at release.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn access_denied(code: &str, message: impl Into<String>) -> Self {
        Error::AccessDenied {
            code: code.into(),
            message: message.into(),
            allowed: Vec::new(),
        }
    }

    pub fn access_denied_with(code: &str, message: impl Into<String>, allowed: Vec<String>) -> Self {
        Error::AccessDenied {
            code: code.into(),
            message: message.into(),
            allowed,
        }
    }

    /// The stable code when this is an access error.
    pub fn access_code(&self) -> Option<&str> {
        match self {
            Error::AccessDenied { code, .. } => Some(code),
            _ => None,
        }
    }

    /// The Postgres error message, for checks that a trigger fired (tests and job errors).
    pub fn db_message(&self) -> Option<String> {
        match self {
            Error::Db(sqlx::Error::Database(e)) => Some(e.message().to_string()),
            _ => None,
        }
    }
}
