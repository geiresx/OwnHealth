//! The built SvelteKit app (`app._mount_ui`). Unknown non-API paths fall back to index.html.
//! The UI is embedded from `server/static` at build time; OWNHEALTH_STATIC_DIR serves a directory
//! instead (as the Python backend did).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use ownhealth_api::ApiError;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../static"]
#[allow_missing = true]
struct Embedded;

#[derive(Debug, Clone)]
pub enum Ui {
    Embedded,
    Dir(PathBuf),
}

impl Ui {
    pub fn from_settings(static_dir: Option<&Path>) -> Ui {
        match static_dir {
            Some(d) => Ui::Dir(d.to_path_buf()),
            None => Ui::Embedded,
        }
    }

    fn read(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        match self {
            Ui::Embedded => Embedded::get(path).map(|f| f.data),
            Ui::Dir(root) => {
                let root = root.canonicalize().ok()?;
                let candidate = root.join(path).canonicalize().ok()?;
                if !candidate.starts_with(&root) || !candidate.is_file() {
                    return None;
                }
                std::fs::read(candidate).ok().map(Cow::Owned)
            }
        }
    }

    pub async fn serve(&self, uri: &Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        if path == "api" || path == "mcp" || path.starts_with("api/") || path.starts_with("mcp/") {
            return ApiError::not_found().into_response();
        }
        if !path.is_empty()
            && let Some(data) = self.read(path)
        {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return file(data, mime.as_ref(), None);
        }
        match self.read("index.html") {
            Some(data) => file(data, "text/html; charset=utf-8", Some("no-cache")),
            None => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "ui_not_built",
                "The UI is not built. Run `pnpm build` in ui/.",
            )
            .into_response(),
        }
    }
}

fn file(data: Cow<'static, [u8]>, content_type: &str, cache: Option<&'static str>) -> Response {
    let mut r = Response::new(Body::from(data.into_owned()));
    if let Ok(v) = HeaderValue::from_str(content_type) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    if let Some(c) = cache {
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(c));
    }
    r
}
