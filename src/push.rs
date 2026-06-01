//! Push-mode HTTP server — transparent webhook proxy.
//!
//! Receives POSTs from relay at a single stable endpoint, looks up
//! the target agent via the X-Amail-Email header, and forwards the
//! raw body + all headers to the gateway's webhook port on localhost.
//!
//! Optional per-IP rate limiting for DDoS protection — configure
//! `push.max_requests_per_sec` in amail_bridge.toml.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Per-IP sliding-window rate limiter.
/// Window: 1 second.  If `max_per_sec` is exceeded the request gets 429.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<IpAddr, (Instant, u32)>>>,
    max_per_sec: u32,
}

impl RateLimiter {
    pub fn new(max_per_sec: u32) -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())), max_per_sec }
    }

    /// Returns `true` if the request is allowed, `false` if throttled.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let window_start = now - Duration::from_secs(1);

        // Cleanup stale entries inline (cheap for typical deployment sizes)
        map.retain(|_, (last, _)| *last >= window_start);

        let entry = map.entry(ip).or_insert((now, 0));
        if entry.0 < window_start {
            *entry = (now, 1);
            true
        } else if entry.1 < self.max_per_sec {
            entry.0 = now;
            entry.1 += 1;
            true
        } else {
            false
        }
    }
}

/// Build the axum Router for push mode.
pub fn build_push_router(state: PushState) -> Router {
    let mut router = Router::new()
        .route("/webhooks/{*name}", post(handle_webhook))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10 MB
        .with_state(state.clone());

    if let Some(max_rps) = state.config.push.max_requests_per_sec {
        let limiter = RateLimiter::new(max_rps);
        router = router.layer(middleware::from_fn_with_state(limiter, rate_limit));
        tracing::info!(max_rps, "Per-IP rate limiting enabled");
    }

    router
}

/// Axum middleware: reject requests exceeding per-IP rate limit (429).
async fn rate_limit(
    State(limiter): State<RateLimiter>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::http::Response<axum::body::Body>, StatusCode> {
    if limiter.check(addr.ip()) {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::TOO_MANY_REQUESTS)
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

    let shutdown_signal = async move {
        loop {
            if shutdown.load(Ordering::SeqCst) {
                tracing::info!("Push server shutting down gracefully");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };

    #[cfg(feature = "tls")]
    if config.push.tls {
        let tls_config = build_tls_config(&config.push)?;
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal.await;
            tracing::info!("Push TLS server shutting down gracefully");
            shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
        });
        axum_server::bind_rustls(addr, tls_config)
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
        return Ok(());
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await?;

    Ok(())
}

#[cfg(feature = "tls")]
fn build_tls_config(push: &crate::config::PushConfig) -> Result<axum_server::tls_rustls::RustlsConfig, Box<dyn std::error::Error>> {
    use std::io::BufReader;

    let cert_path = push.tls_cert.as_deref()
        .ok_or("tls_cert path required when tls=true")?;
    let key_path = push.tls_key.as_deref()
        .ok_or("tls_key path required when tls=true")?;

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
