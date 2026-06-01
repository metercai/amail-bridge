//! Bridge configuration: `amail_bridge.toml` + env var overrides.

use serde::Deserialize;
use std::path::PathBuf;

/// Full bridge configuration, deserialised from `amail_bridge.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeConfig {
    #[serde(default = "default_mode")]
    pub mode: String, // "push" | "pull"

    #[serde(default)]
    pub push: PushConfig,

    #[serde(default)]
    pub pull: PullConfig,

    /// Where to find Hermes profiles (default: ~/.hermes/profiles).
    #[serde(default)]
    pub hermes_home: Option<PathBuf>,

    /// Path to the Hermes default profile (~/.hermes/). Computed at startup.
    #[serde(skip)]
    pub default_profile_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PushConfig {
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    #[serde(default = "default_push_port")]
    pub bind_port: u16,
    #[serde(default)]
    pub tls: bool,
    /// Full externally-accessible URL (without trailing path), e.g. "https://bridge.example.com".
    /// Printed at startup as a hint for the admin to copy into `amail_relay.json`.
    #[serde(default)]
    pub public_url: String,
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,
    #[serde(default)]
    pub tls_key: Option<PathBuf>,
    #[serde(default)]
    pub acme_domain: Option<String>,
    #[serde(default)]
    pub acme_cache: Option<PathBuf>,
    #[serde(default)]
    pub redirect_http: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullConfig {
    #[serde(default)]
    pub relay_url: String,
    #[serde(default)]
    pub admin_key: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_sec: u64,
    #[serde(default)]
    pub system_id: String,
}

// Default helpers
fn default_mode() -> String { "pull".into() }
fn default_bind_host() -> String { "0.0.0.0".into() }
fn default_push_port() -> u16 { 38080 }
fn default_poll_interval() -> u64 { 10 }

impl Default for PullConfig {
    fn default() -> Self {
        Self {
            relay_url: String::new(),
            admin_key: String::new(),
            poll_interval_sec: 10,
            system_id: String::new(),
        }
    }
}

impl BridgeConfig {
    /// Load config from a TOML file, then apply environment variable overrides.
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = path
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join("amail_bridge.toml"));
        let mut cfg: BridgeConfig = {
            let content = std::fs::read_to_string(&config_path)?;
            toml::from_str(&content)?
        };

        // Env overrides
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_MODE") { cfg.mode = v; }
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_PUBLIC_URL") { cfg.push.public_url = v; }
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_RELAY_URL") { cfg.pull.relay_url = v; }
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_ADMIN_KEY") { cfg.pull.admin_key = v; }
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_SYSTEM_ID") { cfg.pull.system_id = v; }
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_POLL_SECS") {
            cfg.pull.poll_interval_sec = v.parse().unwrap_or(10);
        }
        if let Ok(v) = std::env::var("HERMES_HOME") {
            cfg.hermes_home = Some(PathBuf::from(v));
        }

        // Default profile dir
        let hermes_root = cfg.hermes_home.clone().unwrap_or_else(|| {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(".hermes")
        });
        cfg.default_profile_dir = hermes_root;

        Ok(cfg)
    }

    /// Validate configuration and emit warnings for insecure settings.
    pub fn validate(&self) {
        if self.mode == "pull" {
            if self.pull.relay_url.is_empty() {
                tracing::warn!("pull.relay_url is empty — pull loop will fail");
            }
            if self.pull.admin_key.is_empty() {
                tracing::warn!("pull.admin_key is empty — authentication will fail");
            }
            if self.pull.system_id.is_empty() {
                tracing::warn!("pull.system_id is empty — pending query will fail");
            }
        }
        if self.mode == "push" && !self.push.tls && !self.push.public_url.is_empty() {
            tracing::warn!("push.public_url is set but TLS is disabled — consider enabling TLS");
        }
        if self.push.tls && self.push.bind_port == 80 {
            tracing::warn!("TLS enabled on port 80 — usually port 443 is expected");
        }
    }
}
