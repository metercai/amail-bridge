//! Security hardening middleware for public-facing HTTP servers.

use axum::{middleware, Router};
use axum::http::HeaderValue;
use std::net::IpAddr;

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
    response
}

/// Check if an IP matches a CIDR entry.
pub fn ip_matches(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => {
            let mask = if prefix == 0 { 0 } else { !0u32 << (32 - prefix) };
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            let mask = if prefix == 0 { 0 } else { !0u128 << (128 - prefix) };
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
    }
}
