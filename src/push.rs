//! Push-mode HTTP server — transparent webhook proxy.
//!
//! Receives POSTs from relay at a single stable endpoint, looks up
//! the target agent via the X-Amail-Email header, and forwards the
//! raw body + all headers to the gateway's webhook port on localhost.
//!
//! Optional per-IP allowlist for DDoS protection — configure
//! `push.allowed_ips` in amail_bridge.toml.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, HeaderName, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;


use crate::config::BridgeConfig;
use crate::router::ProfileRouter;

/// Application state shared across push handlers.
#[derive(Clone)]
pub struct PushState {
    pub router: Arc<ProfileRouter>,
    pub http_client: reqwest::Client,
    pub config: BridgeConfig,
    pub startup: Instant,
}

/// IP/CIDR allowlist.  Empty = allow all.  Otherwise, only requests from
/// matching source IPs are permitted (403 Forbidden on mismatch).
#[derive(Clone)]
pub struct IpAllowlist {
    entries: Vec<(IpAddr, u8)>,  // (network, prefix_len)
}

impl IpAllowlist {
    /// Parse an allowlist from config strings ("192.168.1.1" or "10.0.0.0/8").
    /// Invalid entries are logged and skipped.
    pub fn from_config(raw: &[String]) -> Self {
        let entries = raw.iter().filter_map(|s| {
            match parse_cidr(s) {
                Some(e) => Some(e),
                None => {
                    tracing::warn!(entry = %s, "Invalid IP/CIDR in allowed_ips — skipping");
                    None
                }
            }
        }).collect();
        Self { entries }
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        if self.entries.is_empty() {
            return true; // no allowlist = allow all
        }
        self.entries.iter().any(|&(network, prefix)| ip_matches(ip, network, prefix))
    }
}

/// Parse "192.168.1.1" or "10.0.0.0/8" into (network, prefix_len).
fn parse_cidr(s: &str) -> Option<(IpAddr, u8)> {
    let (ip_s, prefix) = if let Some((ip, pfx)) = s.split_once('/') {
        (ip, pfx.parse::<u8>().ok()?)
    } else {
        (s, if s.contains(':') { 128 } else { 32 }) // implicit /32 or /128
    };
    let ip: IpAddr = ip_s.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    if prefix > max { return None; }
    Some((ip, prefix))
}

/// IP/CIDR blacklist. Empty = allow all.
#[derive(Clone)]
pub struct IpBlacklist {
    entries: Vec<(IpAddr, u8)>,
}

impl IpBlacklist {
    pub fn from_config(raw: &[String]) -> Self {
        let entries = raw.iter().filter_map(|s| {
            match parse_cidr(s) {
                Some(e) => Some(e),
                None => {
                    tracing::warn!(entry = %s, "Invalid IP/CIDR in blacklist_ips — skipping");
                    None
                }
            }
        }).collect();
        Self { entries }
    }

    pub fn blocks(&self, ip: IpAddr) -> bool {
        if self.entries.is_empty() { return false; }
        self.entries.iter().any(|&(network, prefix)| ip_matches(ip, network, prefix))
    }
}

/// Simple per-IP rate limiter using a sliding window.
#[derive(Clone)]
pub struct IpRateLimiter {
    window: Arc<Mutex<HashMap<IpAddr, (u64, u32)>>>, // ip → (window_start_secs, count)
    max_per_sec: u32,
}

impl IpRateLimiter {
    pub fn new(max_per_sec: u32) -> Self {
        Self { window: Arc::new(Mutex::new(HashMap::new())), max_per_sec }
    }

    /// Returns true if the request is allowed (under limit).
    /// Also cleans up stale entries older than 2 seconds.
    pub fn check(&self, ip: IpAddr) -> bool {
        if self.max_per_sec == 0 { return true; }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64; // millisecond precision — avoids second-boundary flakiness
        let mut w = self.window.lock().unwrap();
        // Cleanup stale entries (older than 2 seconds)
        w.retain(|_, (ts, _)| now.saturating_sub(*ts) <= 2000);
        let entry = w.entry(ip).or_insert((now, 0));
        if now.saturating_sub(entry.0) > 1000 {
            // New second window — reset count
            entry.0 = now;
            entry.1 = 1;
            return true;
        }
        if entry.1 >= self.max_per_sec {
            return false;
        }
        entry.1 += 1;
        true
    }
}

fn ip_matches(ip: IpAddr, network: IpAddr, prefix: u8) -> bool {
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

/// Build the axum Router for push mode.
pub fn build_push_router(state: PushState) -> Router {
    let mut router = Router::new()
        .route("/webhooks/{*name}", post(handle_webhook))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max((state.config.push.body_limit_mb as usize) * 1024 * 1024))
        .with_state(state.clone());

    // IP blacklist (checked first, before allowlist)
    if !state.config.push.blacklist_ips.is_empty() {
        let blacklist = IpBlacklist::from_config(&state.config.push.blacklist_ips);
        router = router.layer(middleware::from_fn_with_state(blacklist, check_blacklist));
        tracing::info!(count = state.config.push.blacklist_ips.len(), "IP blacklist enabled");
    }

    if !state.config.push.allowed_ips.is_empty() {
        let allowlist = IpAllowlist::from_config(&state.config.push.allowed_ips);
        router = router.layer(middleware::from_fn_with_state(allowlist, check_ip));
        tracing::info!(count = state.config.push.allowed_ips.len(), "IP allowlist enabled");
    }

    // Rate limiting (per source IP)
    if state.config.push.rate_limit > 0 {
        let limiter = IpRateLimiter::new(state.config.push.rate_limit);
        router = router.layer(middleware::from_fn_with_state(limiter, check_rate_limit));
        tracing::info!(rps = state.config.push.rate_limit, "Rate limiting enabled");
    }

    // Vhost routing: intercept unmatched paths via fallback
    if !state.config.push.sites.is_empty() {
        let vhost_routes = std::sync::Arc::new(
            crate::vhost::build_routes(&state.config.push.sites)
        );
        let vr = vhost_routes.clone();
        router = router.fallback(move |req: axum::http::Request<axum::body::Body>| {
            let vr = vr.clone();
            async move {
                // Extract client IP from axum's ConnectInfo extension
                let client_ip = req.extensions()
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ci| ci.0.ip());
                if let Some(route) = crate::vhost::find_vhost(&req, &vr) {
                    return crate::vhost::handle_vhost(route, req, client_ip).await;
                }
                axum::http::Response::builder()
                    .status(404)
                    .body(axum::body::Body::from("not found"))
                    .unwrap()
            }
        });
        tracing::info!(count = state.config.push.sites.len(), "Vhost sites loaded");
    }

    router = crate::security::apply_security_headers(router);
    router
}

/// Axum middleware: reject requests not in IP allowlist (403).
async fn check_ip(
    State(allowlist): State<IpAllowlist>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::http::Response<axum::body::Body>, StatusCode> {
    if allowlist.allows(addr.ip()) {
        Ok(next.run(request).await)
    } else {
        tracing::warn!(ip = %addr.ip(), "Blocked by IP allowlist");
        Err(StatusCode::FORBIDDEN)
    }
}

/// Health check — also prints route count.
/// Middleware: reject blacklisted IPs (403).
async fn check_blacklist(
    State(blacklist): State<IpBlacklist>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::http::Response<axum::body::Body>, StatusCode> {
    if blacklist.blocks(addr.ip()) {
        tracing::warn!(ip = %addr.ip(), "Blocked by IP blacklist");
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// Middleware: rate-limit per source IP (429 Too Many Requests).
async fn check_rate_limit(
    State(limiter): State<IpRateLimiter>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::http::Response<axum::body::Body>, StatusCode> {
    if !limiter.check(addr.ip()) {
        tracing::warn!(ip = %addr.ip(), "Rate limited");
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    Ok(next.run(request).await)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
    version: &'static str,
}

async fn health(State(state): State<PushState>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        uptime_secs: state.startup.elapsed().as_secs(),
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Transparent webhook proxy.
///
/// Relay sends:  POST /webhooks/amail-inbound
///               X-Amail-Email: alice@admin.relay
///               X-Webhook-Signature: sha256=...
///               {payload}
///
/// Bridge: looks up `alice@admin.relay` → port 8645,
/// forwards body + headers verbatim to `127.0.0.1:8645/webhooks/amail-inbound`.
async fn handle_webhook(
    State(state): State<PushState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // ── Multi-recipient: signatures array in payload, no x-batch header ──
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
        if v.get("signatures").and_then(|s| s.as_array()).map(|a| !a.is_empty()).unwrap_or(false) {
            return handle_batch_webhook(axum::extract::State(state), body).await;
        }
    }

    // ── Single mode (X-Amail-Email header) ───────────────────────
    // Resolve target from X-Amail-Email header
    let email = match headers.get("x-amail-email").and_then(|v| v.to_str().ok()) {
        Some(e) => e.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing X-Amail-Email header",
            )
                .into_response();
        }
    };

    let route = match state.router.lookup(&email) {
        Some(r) => r,
        None => {
            tracing::warn!(email = %email, "No route found");
            return (
                StatusCode::BAD_GATEWAY,
                format!("No route for {}", email),
            )
                .into_response();
        }
    };

    let target = route.target_url();
    tracing::info!(email = %email, host = %route.host, port = route.port, target = %target, "Webhook relayed");

    // Forward only business headers — avoid leaking Host, Content-Length, etc.
    // This whitelist is intentionally narrow: each header must have a known purpose.
    // Adding a new relay→gateway header? Add it here.
    let mut fwd_headers = HeaderMap::new();
    for name in &["x-amail-email", "x-webhook-signature", "x-mailrelay-timestamp", "content-type"] {
        if let Some(val) = headers.get(*name) {
            fwd_headers.insert(HeaderName::from_static(name), val.clone());
        }
    }

    match state
        .http_client
        .post(&target)
        .headers(fwd_headers)
        .body(body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body_bytes = resp.bytes().await.unwrap_or_default();
            tracing::info!(
                email = %email,
                status = %status,
                "Webhook forwarded"
            );
            (status, body_bytes).into_response()
        }
        Err(e) => {
            tracing::error!(email = %email, error = %e, "Forward failed");
            (
                StatusCode::BAD_GATEWAY,
                format!("Forward failed: {}", e),
            )
                .into_response()
        }
    }
}

/// Start the push-mode HTTP server.
pub async fn start_push_server(
    config: BridgeConfig,
    router: Arc<ProfileRouter>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = PushState {
        router: router.clone(),
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?,
        config: config.clone(),
        startup: Instant::now(),
    };

    let app = build_push_router(state);

    let addr: SocketAddr = config.push.addr.parse()?;

    // Print bridge URL hint for admin
    let hostname = config.push.hostname_or_empty();
    if !hostname.is_empty() {
        let bridge_url = format!(
            "https://{}/webhooks/amail-inbound",
            hostname
        );
        tracing::info!("======================================================");
        tracing::info!("  amail-bridge (push mode) running on {}", addr);
        tracing::info!("  Hostname: {} ({})", hostname, if config.push.has_tls() { "TLS" } else { "plain" });
        tracing::info!("  Add this to ~/.hermes/amail_relay.json:");
        tracing::info!("    \"bridge_url\": \"{}\"", bridge_url);
        tracing::info!("======================================================");
    } else {
        tracing::info!("amail-bridge (push mode) running on {}", addr);
    }

    if config.push.has_tls() {
        // Determine TLS cert source: static files > ACME > HTTP fallback
        let mut acme_stop: Option<Arc<AtomicBool>> = None;

        let (cert_path, key_path) = if config.push.tls_cert.is_some() && config.push.tls_key.is_some() {
            (config.push.tls_cert.clone().unwrap(), config.push.tls_key.clone().unwrap())
        } else if let Some(ref hostname) = config.push.hostname {
            let cache = config.push.acme_cache.clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("acme_cache"));
            tracing::info!(%hostname, cache = %cache.display(), "Attempting ACME certificate...");
            match crate::acme::get_or_acquire_cert(hostname, &cache, None).await {
                Ok((paths, stop)) => {
                    tracing::info!("ACME succeeded — using auto-cert");
                    acme_stop = Some(stop);
                    (paths.cert, paths.key)
                }
                Err(e) => {
                    tracing::warn!(%hostname, error = %e,
                        "ACME certificate acquisition failed — falling back to HTTP");
                    return start_push_http(shutdown, app, addr).await;
                }
            }
        } else {
            tracing::error!("has_tls() returned true but hostname is None — falling back to HTTP");
            return start_push_http(shutdown, app, addr).await;
        };

        let tls_config = build_tls_config_from_paths(&cert_path, &key_path)?;
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        let shutdown_tls = shutdown.clone();
        tokio::spawn(async move {
            loop {
                if shutdown_tls.load(Ordering::SeqCst) {
                    tracing::info!("Push TLS server shutting down gracefully");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
        axum_server::bind_rustls(addr, tls_config)
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;

        // Signal ACME renew thread to stop
        if let Some(stop) = acme_stop {
            stop.store(true, Ordering::SeqCst);
            // Thread polls every 10s — give it a moment to notice
            std::thread::sleep(std::time::Duration::from_millis(500));
            tracing::info!("ACME renew thread signalled to stop");
        }

        return Ok(());
    }

    start_push_http(shutdown, app, addr).await
}

async fn start_push_http(
    shutdown: Arc<AtomicBool>,
    app: Router,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown_signal = async move {
        loop {
            if shutdown.load(Ordering::SeqCst) {
                tracing::info!("Push server shutting down gracefully");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await?;
    Ok(())
}

/// Handle multi-recipient webhook: payload has "signatures" array,
/// payload itself IS the body (no "body" wrapper).
/// Fan-out to each recipient's gateway with per-recipient headers.
async fn handle_batch_webhook(
    State(state): State<PushState>,
    body: Bytes,
) -> axum::http::Response<axum::body::Body> {
    let batch: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid batch JSON: {}", e)).into_response();
        }
    };

    let sigs = match batch.get("signatures").and_then(|e| e.as_array()) {
        Some(arr) => arr,
        None => return (StatusCode::BAD_REQUEST, "Missing 'signatures' array").into_response(),
    };

    // Shared body = payload minus the signatures array
    let mut shared_body = batch.clone();
    let _ = shared_body.as_object_mut().map(|o| o.remove("signatures"));

    let total = sigs.len();
    let mut delivered = 0usize;

    for entry in sigs {
        let email = match entry["email"].as_str() {
            Some(e) => e,
            None => continue,
        };

        let route = match state.router.lookup(email) {
            Some(r) => r,
            None => {
                tracing::warn!(email = %email, "Batch entry has no route");
                continue;
            }
        };

        let target = route.target_url();
        let sig = entry["signature"].as_str().unwrap_or("");
        let ts = entry["timestamp"].as_str().unwrap_or("");

        match state.http_client
            .post(&target)
            .header("x-amail-email", email)
            .header("x-webhook-signature", sig)
            .header("x-mailrelay-timestamp", ts)
            .header("content-type", "application/json")
            .json(&shared_body)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(email = %email, "Batch entry forwarded");
                delivered += 1;
            }
            Ok(resp) => {
                tracing::warn!(email = %email, status = %resp.status(), "Batch entry non-2xx");
            }
            Err(e) => {
                tracing::error!(email = %email, error = %e, "Batch entry forward failed");
            }
        }
    }

    if delivered == total {
        (StatusCode::OK, format!("{}/{} delivered", delivered, total)).into_response()
    } else {
        (StatusCode::MULTI_STATUS, format!("{}/{} delivered", delivered, total)).into_response()
    }
}

fn build_tls_config_from_paths(cert_path: &std::path::Path, key_path: &std::path::Path) -> Result<axum_server::tls_rustls::RustlsConfig, Box<dyn std::error::Error>> {
    use std::io::BufReader;
    let cert_file = std::fs::File::open(cert_path)?;
    let key_file = std::fs::File::open(key_path)?;
    let mut cert_reader = BufReader::new(cert_file);
    let mut key_reader = BufReader::new(key_file);

    let certs: Vec<rustls::pki_types::CertificateDer> =
        rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<_, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or("No private key found in tls_key file")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        std::sync::Arc::new(config),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn test_parse_cidr_single_ip() {
        assert_eq!(parse_cidr("192.168.1.1"), Some(("192.168.1.1".parse().unwrap(), 32)));
    }

    #[test]
    fn test_parse_cidr_network() {
        assert_eq!(parse_cidr("10.0.0.0/8"), Some(("10.0.0.0".parse().unwrap(), 8)));
    }

    #[test]
    fn test_parse_cidr_invalid() {
        assert_eq!(parse_cidr("not-an-ip"), None);
        assert_eq!(parse_cidr("10.0.0.0/33"), None);
    }

    #[test]
    fn test_ip_matches() {
        let ip: IpAddr = "192.168.1.50".parse().unwrap();
        let net: IpAddr = "192.168.1.0".parse().unwrap();
        assert!(ip_matches(ip, net, 24));
        assert!(!ip_matches(ip, net, 32));
    }

    #[test]
    fn test_allowlist_allows() {
        let allowlist = IpAllowlist::from_config(&["10.0.0.0/8".into()]);
        assert!(allowlist.allows("10.1.2.3".parse().unwrap()));
        assert!(!allowlist.allows("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn test_allowlist_empty_allows_all() {
        let allowlist = IpAllowlist::from_config(&[]);
        assert!(allowlist.allows("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn test_blacklist_blocks() {
        let blacklist = IpBlacklist::from_config(&["10.0.0.0/8".into()]);
        assert!(blacklist.blocks("10.1.2.3".parse().unwrap()));
        assert!(!blacklist.blocks("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn test_rate_limiter_allows_under_limit() {
        let limiter = IpRateLimiter::new(5);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..5 {
            assert!(limiter.check(ip));
        }
        assert!(!limiter.check(ip)); // 6th should be blocked
    }

    #[test]
    fn test_rate_limiter_disabled() {
        let limiter = IpRateLimiter::new(0);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..100 {
            assert!(limiter.check(ip));
        }
    }
}
