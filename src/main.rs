//! amail-bridge — transparent bridge between amail relay and Hermes gateway.
//!
//! Two modes:
//! - **push**: expose a single external endpoint, transparently proxy to gateway webhook ports.
//! - **pull**: outbound long-poll relay's /pending endpoint, forward to gateway, ACK on success.

mod config;
mod pull;
mod push;
mod router;

use std::sync::Arc;

use tracing_subscriber::EnvFilter;

use crate::config::BridgeConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("amail-bridge starting");

    let config = BridgeConfig::load()?;
    let router = Arc::new(router::ProfileRouter::new(&config.default_profile_dir));

    // Start profile file watcher
    if let Err(e) = router::start_watcher(router.clone()) {
        tracing::warn!(error = %e, "Profile watcher failed to start — routes may be stale");
    }

    match config.mode.as_str() {
        "push" => {
            push::start_push_server(config, router).await?;
        }
        "pull" => {
            pull::start_pull_loop(config, router).await?;
        }
        other => {
            eprintln!("Unknown mode: {}. Use 'push' or 'pull'.", other);
            std::process::exit(1);
        }
    }

    Ok(())
}
