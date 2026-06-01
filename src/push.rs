//! Push-mode HTTP server — transparent webhook proxy.
//!
//! Receives POSTs from relay at a single stable endpoint, looks up
//! the target agent via the X-Amail-Email header, and forwards the
//! raw body + all headers to the gateway's webhook port on localhost.
//!
//! Optional per-IP allowlist for DDoS protection — configure
//! `push.allowed_ips` in amail_bridge.toml.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, HeaderName, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Router,
};


use crate::config::BridgeConfig;
use crate::router::ProfileRouter;

/// Application state shared across push handlers.
#[derive(Clone)]
pub struct PushState {
    pub router: Arc<ProfileRouter>,
    pub http_client: reqwest::Client,
    pub config: BridgeConfig,
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
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10 MB
        .with_state(state.clone());

    if !state.config.push.allowed_ips.is_empty() {
        let allowlist = IpAllowlist::from_config(&state.config.push.allowed_ips);
        router = router.layer(middleware::from_fn_with_state(allowlist, check_ip));
        tracing::info!(count = state.config.push.allowed_ips.len(), "IP allowlist enabled");
    }

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
async fn health(State(state): State<PushState>) -> String {
    format!(
        "amail-bridge push mode OK\nroutes: {}\nbinding: {}:{}\n",
        state.router.route_count(),
        state.config.push.bind_host,
        state.config.push.bind_port,
    )
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
    // ── Batch mode (X-Batch header) ─────────────────────────────
    if headers.get("x-batch").and_then(|v| v.to_str().ok()) == Some("1") {
        return handle_batch_webhook(axum::extract::State(state), body).await;
    }

    // ── Single mode (legacy) ────────────────────────────────────
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
    tracing::debug!(email = %email, host = %route.host, port = route.port, target = %target, "Forwarding webhook");

    // Forward only business headers — avoid leaking Host, Content-Length, etc.
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
    };

    let app = build_push_router(state);

    let addr: SocketAddr = format!("{}:{}", config.push.bind_host, config.push.bind_port)
        .parse()?;

    // Print bridge URL hint for admin
    if !config.push.public_url.is_empty() {
        let bridge_url = format!(
            "{}/webhooks/amail-inbound",
            config.push.public_url.trim_end_matches('/')
        );
        tracing::info!("======================================================");
        tracing::info!("  amail-bridge (push mode) running on {}", addr);
        tracing::info!("  Public URL: {}", config.push.public_url);
        tracing::info!("  TLS: {}", if config.push.tls { "enabled" } else { "disabled" });
        tracing::info!("  Add this to ~/.hermes/amail_relay.json:");
        tracing::info!("    \"bridge_url\": \"{}\"", bridge_url);
        tracing::info!("======================================================");
    } else {
        tracing::info!("amail-bridge (push mode) running on {}", addr);
    }

    #[cfg(feature = "tls")]
    if config.push.tls {
        // Determine TLS cert source: static files > ACME > HTTP fallback
        let (cert_path, key_path) = if config.push.tls_cert.is_some() && config.push.tls_key.is_some() {
            (config.push.tls_cert.clone().unwrap(), config.push.tls_key.clone().unwrap())
        } else if !config.push.public_url.is_empty() {
            match crate::acme::extract_domain(&config.push.public_url) {
                Some(domain) => {
                    let cache = config.push.acme_cache.clone()
                        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".hermes").join("acme"));
                    tracing::info!(%domain, cache = %cache.display(), "Attempting ACME certificate...");
                    match crate::acme::acquire_cert(&domain, &cache, None) {
                        Ok(paths) => {
                            tracing::info!("ACME succeeded — using auto-cert");
                            (paths.cert, paths.key)
                        }
                        Err(e) => {
                            tracing::warn!(%domain, error = %e,
                                "ACME certificate acquisition failed — falling back to HTTP");
                            return start_push_http(shutdown, app, addr).await;
                        }
                    }
                }
                None => {
                    tracing::warn!("Cannot extract domain from public_url '{}' — falling back to HTTP",
                                   config.push.public_url);
                    return start_push_http(shutdown, app, addr).await;
                }
            }
        } else {
            tracing::warn!("TLS enabled but no cert config — falling back to HTTP");
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

/// Handle batched webhook (X-Batch: 1): parse {"body":..., "entries":[...]}
/// and fan-out to each recipient's gateway with per-recipient headers.
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

    let shared_body = match batch.get("body") {
        Some(b) => b.clone(),
        None => return (StatusCode::BAD_REQUEST, "Missing 'body' in batch").into_response(),
    };

    let entries = match batch.get("entries").and_then(|e| e.as_array()) {
        Some(arr) => arr,
        None => return (StatusCode::BAD_REQUEST, "Missing 'entries' array in batch").into_response(),
    };

    let total = entries.len();
    let mut delivered = 0usize;

    for entry in entries {
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
        (StatusCode::OK, format!("Batch: {}/{} delivered", delivered, total)).into_response()
    } else {
        (StatusCode::MULTI_STATUS, format!("Batch: {}/{} delivered", delivered, total)).into_response()
    }
}

#[cfg(feature = "tls")]
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
