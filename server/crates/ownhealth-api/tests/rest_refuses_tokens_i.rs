//! Lane I part of tests/security/test_rest_refuses_tokens.py: a PAT works only on /mcp. Over REST it
//! would skip the review queue (accept its own proposal, import), the rate limit and the egress
//! budget, so every lane I route answers 403 `forbidden` and changes nothing.

mod lane_i;

use axum::body::Body;
use http::{Request, StatusCode, header};
use lane_i::{R, app};
use serde_json::json;

const ROUTES: [(&str, &str); 12] = [
    ("GET", "/review"),
    ("GET", "/review/00000000-0000-0000-0000-000000000000"),
    ("POST", "/review/00000000-0000-0000-0000-000000000000/accept"),
    ("POST", "/review/00000000-0000-0000-0000-000000000000/reject"),
    ("POST", "/review/00000000-0000-0000-0000-000000000000/reopen"),
    ("POST", "/review/00000000-0000-0000-0000-000000000000/dismiss"),
    ("POST", "/review/bulk-accept"),
    ("POST", "/imports"),
    ("GET", "/jobs"),
    ("GET", "/sources/src_00000000"),
    ("GET", "/me"),
    ("GET", "/profiles/alex/timeline"),
];

#[tokio::test]
async fn test_pat_is_refused_on_rest() -> R {
    let a = app().await?;
    let pat = a.pat().await?;
    let before = a
        .count("select (select count(*) from job) + (select count(*) from review_item)")
        .await?;
    for (method, path) in ROUTES {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {pat}"));
        let body = if method == "POST" {
            b = b.header(header::CONTENT_TYPE, "application/json");
            Body::from("{}")
        } else {
            Body::empty()
        };
        let (s, v) = a.send(b.body(body)?).await?;
        assert_eq!(
            (s, v["detail"]["code"].clone()),
            (StatusCode::FORBIDDEN, json!("forbidden")),
            "{path} {v}"
        );
    }
    let after = a
        .count("select (select count(*) from job) + (select count(*) from review_item)")
        .await?;
    assert_eq!(before, after);
    // the session still reaches them
    assert_eq!(a.get("alex", "/review").await?.0, StatusCode::OK);
    Ok(())
}
