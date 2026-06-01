//! Push-mode HTTP server — transparent webhook proxy.
//!
//! Receives POSTs from relay at a single stable endpoint, looks up
//! the target agent via the X-Amail-Email header, and forwards the
//! raw body + all headers to the gateway's webhook port on localhost.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderName, StatusCode},
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

/// Build the axum Router for push mode.
pub fn build_push_router(state: PushState) -> Router {
    Router::new()
        .route("/webhooks/{*name}", post(handle_webhook))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10 MB
        .with_state(state)
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

    let port = match state.router.lookup(&email) {
        Some(p) => p,
        None => {
            tracing::warn!(email = %email, "No route found");
            return (
                StatusCode::BAD_GATEWAY,
                format!("No route for {}", email),
            )
                .into_response();
        }
    };

    let target = format!("http://127.0.0.1:{}/webhooks/amail-inbound", port);
    tracing::debug!(email = %email, port = port, target = %target, "Forwarding webhook");

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
        tracing::info!("  Add this to ~/.hermes/amail_relay.json:");
        tracing::info!("    \"bridge_url\": \"{}\"", bridge_url);
        tracing::info!("======================================================");
    } else {
        tracing::info!("amail-bridge (push mode) running on {} (no public_url set)", addr);
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Graceful shutdown: serve until SIGTERM
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    tracing::info!("Push server shutting down gracefully");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await?;

    Ok(())
}
