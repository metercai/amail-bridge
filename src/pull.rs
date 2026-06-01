//! Pull-mode long-poll client.
//!
//! Repeatedly GETs relay's /pending endpoint, forwards each delivery
//! to the gateway webhook port, then ACKs the relay.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use urlencoding::encode;

use crate::config::BridgeConfig;
use crate::router::ProfileRouter;

/// Application state shared across pull tasks.
#[derive(Clone)]
pub struct PullState {
    pub router: Arc<ProfileRouter>,
    pub http_client: reqwest::Client,
    pub config: BridgeConfig,
}

/// Main pull loop: poll → forward → ACK, with dedup cache.
pub async fn start_pull_loop(
    config: BridgeConfig,
    router: Arc<ProfileRouter>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = PullState {
        router: router.clone(),
        http_client: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        config: config.clone(),
    };

    let interval = Duration::from_secs(config.pull.poll_interval_sec);
    let mut seen: HashMap<i64, Instant> = HashMap::new(); // delivery_id → forwarded_at
    let seen_ttl = Duration::from_secs(7200); // 2h

    tracing::info!(
        relay_url = %config.pull.relay_url,
        system_id = %config.pull.system_id,
        poll_interval_sec = config.pull.poll_interval_sec,
        "Starting pull loop"
    );

    loop {
        // Check graceful shutdown
        if shutdown.load(Ordering::SeqCst) {
            tracing::info!("Pull loop shutting down gracefully");
            return Ok(());
        }

        // Periodic cleanup of stale dedup entries
        seen.retain(|_, t| t.elapsed() < seen_ttl);

        match fetch_pending(&state).await {
            Ok(deliveries) => {
                if !deliveries.is_empty() {
                    tracing::info!(count = deliveries.len(), "Fetched pending deliveries");
                }
                let mut ack_ids: Vec<i64> = Vec::new();
                for d in &deliveries {
                    // Dedup: skip already-forwarded deliveries
                    if let Some(t) = seen.get(&d.id) {
                        if t.elapsed() < seen_ttl {
                            tracing::debug!(id = d.id, "Already forwarded — ACKing without re-forward");
                            ack_ids.push(d.id);
                            continue;
                        }
                    }

                    // Look up route
                    let port = match state.router.lookup(&d.email) {
                        Some(p) => p,
                        None => {
                            tracing::warn!(email = %d.email, id = d.id, "No route — skipping");
                            ack_ids.push(d.id); // ACK anyway to avoid accumulating unrouteable
                            continue;
                        }
                    };

                    let target = format!("http://127.0.0.1:{}/webhooks/amail-inbound", port);

                    // Parse headers from relay payload
                    let headers: HashMap<String, String> = match serde_json::from_str(&d.headers) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!(id = d.id, error = %e, "Invalid headers JSON");
                            ack_ids.push(d.id);
                            continue;
                        }
                    };

                    let payload: serde_json::Value = match serde_json::from_str(&d.payload) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(id = d.id, error = %e, "Invalid payload JSON");
                            ack_ids.push(d.id);
                            continue;
                        }
                    };

                    // Forward to gateway
                    let mut req_builder = state.http_client.post(&target);
                    for (k, v) in &headers {
                        req_builder = req_builder.header(k.as_str(), v.as_str());
                    }
                    req_builder = req_builder.json(&payload);

                    match req_builder.send().await {
                        Ok(resp) if resp.status().is_success() => {
                            tracing::info!(id = d.id, email = %d.email, "Forwarded successfully");
                            seen.insert(d.id, Instant::now());
                            ack_ids.push(d.id);
                        }
                        Ok(resp) => {
                            tracing::warn!(
                                id = d.id,
                                email = %d.email,
                                status = %resp.status(),
                                "Forward got non-2xx — retrying next poll"
                            );
                        }
                        Err(e) => {
                            tracing::error!(id = d.id, email = %d.email, error = %e, "Forward failed");
                        }
                    }
                }

                // ACK the forwarded deliveries
                if !ack_ids.is_empty() {
                    match ack_deliveries(&state, &ack_ids).await {
                        Ok(count) => {
                            tracing::info!(acked = count, "ACKed deliveries");
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "ACK failed — will retry");
                        }
                    }
                }

                // Cleanup delivered entries from dedup cache
                seen.retain(|_, t| t.elapsed() < seen_ttl);
            }
            Err(e) => {
                tracing::error!(error = %e, "Pull fetch failed");
            }
        }

        tokio::time::sleep(interval).await;
    }
}

/// Pending delivery from relay.
#[derive(Debug, serde::Deserialize)]
struct PendingDelivery {
    id: i64,
    email: String,
    headers: String, // JSON object string
    payload: String, // JSON object string
}

/// Fetch pending deliveries from relay.
async fn fetch_pending(state: &PullState) -> Result<Vec<PendingDelivery>, Box<dyn std::error::Error>> {
    let url = format!(
        "{}/api/v1/admin/pending?system_id={}&limit=50",
        state.config.pull.relay_url.trim_end_matches('/'),
        encode(&state.config.pull.system_id),
    );

    let resp = state
        .http_client
        .get(&url)
        .header("X-Api-Key", &state.config.pull.admin_key)
        .send()
        .await?
        .error_for_status()?;

    let body: serde_json::Value = resp.json().await?;
    let deliveries: Vec<PendingDelivery> = serde_json::from_value(body["deliveries"].clone())
        .unwrap_or_default();

    Ok(deliveries)
}

/// ACK deliveries back to relay.
async fn ack_deliveries(state: &PullState, ids: &[i64]) -> Result<usize, Box<dyn std::error::Error>> {
    let url = format!(
        "{}/api/v1/admin/pending/ack",
        state.config.pull.relay_url.trim_end_matches('/'),
    );

    let resp = state
        .http_client
        .post(&url)
        .header("X-Api-Key", &state.config.pull.admin_key)
        .json(&serde_json::json!({ "ids": ids }))
        .send()
        .await?
        .error_for_status()?;

    let body: serde_json::Value = resp.json().await?;
    Ok(body["acked"].as_u64().unwrap_or(0) as usize)
}
