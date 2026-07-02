//! Route health checker — periodic TCP probe of webhook targets.
//!
//! Runs alongside the pull loop (or push server). Every N seconds, iterates
//! over all unique route targets in the route table and attempts a TCP
//! connection. Targets that fail consecutively beyond a threshold are
//! considered dead and all routes pointing to them are removed.
//!
//! This auto-cleans stale routes when:
//! - A Hermes gateway moves to a different port
//! - A profile is deleted without unregistering its route
//! - A remote agent gateway goes permanently offline

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::MissedTickBehavior;

use crate::config::HealthConfig;
use crate::router::ProfileRouter;

/// Start the route health check loop. Runs until shutdown is signalled.
pub async fn start_route_health(
    router: Arc<ProfileRouter>,
    config: HealthConfig,
    shutdown: Arc<AtomicBool>,
) {
    let mut failures: HashMap<String, u32> = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_secs(config.check_interval_sec));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tracing::info!(
        interval_sec = config.check_interval_sec,
        fail_threshold = config.fail_threshold,
        connect_timeout_sec = config.connect_timeout_sec,
        "Route health check started"
    );

    loop {
        if shutdown.load(Ordering::SeqCst) {
            tracing::info!("Route health check shutting down");
            return;
        }
        interval.tick().await;
        check_routes(&router, &mut failures, &config).await;
    }
}

/// One round of route health checks.
async fn check_routes(router: &ProfileRouter, failures: &mut HashMap<String, u32>, config: &HealthConfig) {
    let routes = router.list_routes();
    if routes.is_empty() {
        return;
    }

    // Group by unique target
    let mut target_groups: HashMap<String, Vec<String>> = HashMap::new();
    for r in &routes {
        let target = format!("{}:{}", r.host, r.port);
        target_groups.entry(target).or_default().push(r.email.clone());
    }

    let mut dead_targets: Vec<String> = Vec::new();

    for (target, emails) in &target_groups {
        let (host, port_str) = match target.split_once(':') {
            Some((h, p)) => (h, p),
            None => continue,
        };
        let port: u16 = match port_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let ok = probe_target(host, port, config.connect_timeout_sec).await;

        if ok {
            failures.remove(target);
            continue;
        }

        // Increment failure counter
        let count = failures.entry(target.clone()).or_insert(0);
        *count += 1;

        if *count >= config.fail_threshold {
            tracing::warn!(
                target = %target,
                emails = ?emails,
                consecutive_failures = *count,
                "Route target unreachable — removing {} route(s)",
                emails.len(),
            );
            dead_targets.push(target.clone());
        } else {
            tracing::debug!(
                target = %target,
                failures = *count,
                threshold = config.fail_threshold,
                "Route target probe failed (will retry)"
            );
        }
    }

    // Remove dead targets
    for target in &dead_targets {
        if let Some(emails) = target_groups.get(target) {
            for email in emails {
                router.remove_route(email);
            }
        }
        failures.remove(target);
    }

    if !dead_targets.is_empty() {
        let remaining = router.route_count();
        tracing::info!(
            removed = dead_targets.len(),
            remaining_routes = remaining,
            "Stale routes cleaned up"
        );
    }
}

/// TCP probe: connect to host:port with timeout.
/// TCP probe: connect to host:port with timeout.
async fn probe_target(host: &str, port: u16, timeout_secs: u64) -> bool {
    let addr = format!("{}:{}", host, port);
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(e)) => {
            tracing::debug!(target = %addr, error = %e, "TCP probe failed");
            false
        }
        Err(_) => {
            tracing::debug!(target = %addr, "TCP probe timed out");
            false
        }
    }
}
