//! Pull-mode long-poll client.
//!
//! Repeatedly GETs relay's /pending endpoint, forwards each delivery
//! to the gateway webhook port, then ACKs the relay.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

    let mut seen: HashMap<i64, Instant> = HashMap::new();
    let seen_ttl = Duration::from_secs(7200);
    let poll_interval = config.pull.poll_interval_sec;
    let mut consecutive_failures: u32 = 0;
    const MAX_BACKOFF: u64 = 300; // 5 min

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

        let sleep_secs = match fetch_pending(&state).await {
            Ok(deliveries) => {
                consecutive_failures = 0;
                if !deliveries.is_empty() {
                    tracing::info!(count = deliveries.len(), "Fetched pending deliveries");
                }
                // ... process deliveries (same as before, just no backoff_secs assignment)
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

                    // Look up route — do NOT ACK if route is missing.
                    // Routes may be temporarily empty during a rescan; the relay
                    // should have its own cron cleanup for stale pending deliveries.
                    let route = match state.router.lookup(&d.email) {
                        Some(r) => r,
                        None => {
                            tracing::warn!(email = %d.email, id = d.id, "No route — skipping (will retry)");
                            continue;
                        }
                    };

                    let target = route.target_url();

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

                poll_interval
            }
            Err(e) => {
                consecutive_failures += 1;
                let bs = (poll_interval * 2_u64.pow(consecutive_failures.min(6)))
                    .min(MAX_BACKOFF);
                tracing::error!(
                    error = %e,
                    consecutive_failures,
                    backoff_secs = bs,
                    "Pull fetch failed — backing off"
                );
                bs
            }
        };

        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
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

/// Fetch pending deliveries from relay.  Returns empty vec if no known emails.
async fn fetch_pending(state: &PullState) -> Result<Vec<PendingDelivery>, Box<dyn std::error::Error>> {
    let emails: Vec<String> = state.router.list_emails();
    if emails.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "{}/api/v1/admin/pending",
        state.config.pull.relay_url.trim_end_matches('/'),
    );
    let body = serde_json::json!({
        "limit": 50,
        "emails": emails,
    });

    let resp = state
        .http_client
        .post(&url)
        .header("X-Api-Key", &state.config.pull.admin_key)
        .json(&body)
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
