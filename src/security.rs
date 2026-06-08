//! Security hardening middleware for public-facing HTTP servers.

use axum::{middleware, Router};
use axum::http::HeaderValue;

pub fn apply_security_headers(router: Router) -> Router {
    router.layer(middleware::from_fn(add_security_headers))
}

async fn add_security_headers(
    req: axum::http::Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    let _ = headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    let _ = headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    let _ = headers.insert("referrer-policy", HeaderValue::from_static("strict-origin-when-cross-origin"));
    let _ = headers.insert("strict-transport-security", HeaderValue::from_static("max-age=31536000; includeSubDomains"));
    response
}
