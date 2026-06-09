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

    /// Per-agent host overrides for multi-machine deployments.
    /// Regex pattern → IP or hostname. First match wins (insertion order).
    /// Example: `".*@admin.relay" = "192.168.1.2"`
    #[serde(default, deserialize_with = "deserialize_hosts_vec")]
    pub hosts: Vec<(String, String)>,
}

/// Custom deserializer: reads a TOML table into a Vec, preserving insertion order.
/// Required because `HashMap<String, String>` does NOT preserve order;
/// the user-written TOML order is the intended "first match wins" priority.
fn deserialize_hosts_vec<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<(String, String)>, D::Error> {
    use serde::de::{MapAccess, Visitor};
    use std::fmt;
    struct MapToVec;
    impl<'de> Visitor<'de> for MapToVec {
        type Value = Vec<(String, String)>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a TOML table of host regex → ip/hostname pairs")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut v = Vec::new();
            while let Some((k, val)) = map.next_entry::<String, String>()? {
                v.push((k, val));
            }
            Ok(v)
        }
    }
    d.deserialize_map(MapToVec)
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
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
    pub acme_cache: Option<PathBuf>,
    #[serde(default)]
    pub redirect_http: bool,
    /// IP/CIDR allowlist for DDoS protection.  Only requests from
    /// these addresses can POST webhooks.  Empty = allow all.
    /// Example: `allowed_ips = ["10.0.0.0/8", "192.168.1.1"]`
    #[serde(default)]
    pub allowed_ips: Vec<String>,

    /// IP/CIDR blacklist. Blocked before allowlist check. Empty = none.
    #[serde(default)]
    pub blacklist_ips: Vec<String>,

    /// Rate limit (requests/sec per source IP). 0 = disabled. Default 30.
    #[serde(default = "default_rate_limit")]
    pub rate_limit: u32,

    /// Max request body size in MB. Default 20.
    #[serde(default = "default_body_limit")]
    pub body_limit_mb: u32,

    /// Virtual host sites (optional).
    #[serde(default)]
    pub sites: Vec<VhostSiteConfig>,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            bind_host: "0.0.0.0".into(),
            bind_port: 38080,
            tls: false,
            public_url: String::new(),
            tls_cert: None,
            tls_key: None,
            acme_cache: None,
            redirect_http: false,
            allowed_ips: Vec::new(),
            blacklist_ips: Vec::new(),
            rate_limit: 30,
            body_limit_mb: 20,
            sites: Vec::new(),
        }
    }
}

fn default_rate_limit() -> u32 { 30 }
fn default_body_limit() -> u32 { 20 }

/// TOML-deserialized virtual host site configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct VhostSiteConfig {
    pub domain: String,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub redirect: Option<String>,
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
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_ALLOWED_IPS") {
            cfg.push.allowed_ips = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(v) = std::env::var("HERMES_HOME") {
            cfg.hermes_home = Some(PathBuf::from(v));
        }

        // Push/pull are mutually exclusive — push wins because it has
        // lower latency and better efficiency when a public IP is available.
        // A configured push section (public_url, tls_cert, or non-default
        // bind_port) overrides an explicit pull mode.
        let push_configured = !cfg.push.public_url.is_empty()
            || cfg.push.tls_cert.is_some()
            || cfg.push.tls_key.is_some();
        if cfg.mode == "pull" && push_configured {
            tracing::warn!(
                "Push config detected (public_url/tls_cert) but mode is 'pull' — \
                 push has lower latency, switching to push mode"
            );
            cfg.mode = "push".to_string();
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

    /// Compile host patterns into (Regex, host) pairs for the router.
    /// Invalid patterns are logged and skipped. First match wins.
    pub fn compiled_hosts(&self) -> Vec<(regex::Regex, String)> {
        self.hosts.iter().filter_map(|(pattern, host)| {
            match regex::Regex::new(pattern) {
                Ok(re) => Some((re, host.clone())),
                Err(e) => {
                    tracing::warn!(pattern = %pattern, error = %e, "Invalid host regex — skipping");
                    None
                }
            }
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults() {
        let cfg: BridgeConfig = toml::from_str("mode = \"pull\"\n[pull]\nrelay_url = \"http://x\"\nadmin_key = \"k\"\nsystem_id = \"s\"\n").unwrap();
        assert_eq!(cfg.mode, "pull");
        assert_eq!(cfg.push.body_limit_mb, 20);
        assert_eq!(cfg.push.rate_limit, 30);
        assert!(cfg.push.blacklist_ips.is_empty());
        assert!(cfg.push.allowed_ips.is_empty());
    }

    #[test]
    fn test_push_config_with_limits() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
[push]
blacklist_ips = ["1.2.3.4"]
allowed_ips = ["10.0.0.0/8"]
rate_limit = 100
body_limit_mb = 50
"#).unwrap();
        assert_eq!(cfg.push.rate_limit, 100);
        assert_eq!(cfg.push.body_limit_mb, 50);
        assert_eq!(cfg.push.blacklist_ips, vec!["1.2.3.4"]);
    }

    #[test]
    fn test_vhost_sites_parse() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
[push]
[[push.sites]]
domain = "www.example.com"
root = "/var/www"
[[push.sites]]
domain = "old.example.com"
redirect = "https://www.example.com"
"#).unwrap();
        assert_eq!(cfg.push.sites.len(), 2);
        assert_eq!(cfg.push.sites[0].domain, "www.example.com");
        assert_eq!(cfg.push.sites[0].root.as_deref(), Some("/var/www"));
        assert_eq!(cfg.push.sites[1].redirect.as_deref(), Some("https://www.example.com"));
    }
}

