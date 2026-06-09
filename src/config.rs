//! Bridge configuration: `amail_bridge.toml` + env var overrides.
//! Config format aligned with amail-gateway.

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

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
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
pub struct PushConfig {
    /// Listen address in "host:port" format (e.g. "0.0.0.0:38080").
    /// When port = 80 and hostname is set → dual-port mode (80 → 443 redirect).
    #[serde(default = "default_push_addr")]
    pub addr: String,

    /// Public hostname for TLS (e.g. "bridge.example.com").
    /// When set:
    ///   - TLS is enabled (tls_cert/tls_key or ACME auto-cert)
    ///   - If addr port == 80 → dual-port mode (80 redirects to 443)
    ///   - Printed at startup as a hint for relay config
    #[serde(default)]
    pub hostname: Option<String>,

    /// Optional static TLS certificate path (PEM format).
    #[serde(default)]
    pub tls_cert: Option<PathBuf>,

    /// Optional static TLS private key path (PEM format).
    #[serde(default)]
    pub tls_key: Option<PathBuf>,

    /// ACME certificate cache directory. Defaults to ./acme_cache.
    #[serde(default)]
    pub acme_cache: Option<PathBuf>,

    /// IP/CIDR allowlist for DDoS protection. Only requests from
    /// these addresses can POST webhooks. Empty = allow all.
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

impl PushConfig {
    /// Returns true if TLS should be enabled (hostname is set).
    pub fn has_tls(&self) -> bool {
        self.hostname.is_some()
    }

    /// Parse `addr` into (host, port). If no port is specified, defaults to 80.
    pub fn parsed_addr(&self) -> (&str, u16) {
        if let Some((host, port_str)) = self.addr.rsplit_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                return (host, port);
            }
        }
        (&self.addr, 80)
    }

    /// True when dual-port mode (80 + 443) should be enabled.
    /// Conditions: addr port == 80 AND hostname is set.
    pub fn is_dual_port(&self) -> bool {
        let (_, port) = self.parsed_addr();
        port == 80 && self.hostname.is_some()
    }

    /// Return the hostname or an empty string for display.
    pub fn hostname_or_empty(&self) -> &str {
        self.hostname.as_deref().unwrap_or("")
    }
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:38080".into(),
            hostname: None,
            tls_cert: None,
            tls_key: None,
            acme_cache: None,
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

/// Logging configuration (amail-gateway compatible).
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
        }
    }
}

// Default helpers
fn default_mode() -> String { "pull".into() }
fn default_push_addr() -> String { "0.0.0.0:38080".into() }
fn default_poll_interval() -> u64 { 10 }
fn default_log_level() -> String { "info".into() }

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
        if let Ok(v) = std::env::var("AMAIL_BRIDGE_HOSTNAME") {
            if !v.is_empty() { cfg.push.hostname = Some(v); }
        }
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
        // A configured push section (hostname, tls_cert, or non-default addr)
        // overrides an explicit pull mode.
        let push_configured = cfg.push.hostname.is_some()
            || cfg.push.tls_cert.is_some()
            || cfg.push.tls_key.is_some()
            || cfg.push.addr != *"0.0.0.0:38080";
        if cfg.mode == "pull" && push_configured {
            tracing::warn!(
                "Push config detected (hostname/tls_cert) but mode is 'pull' — \
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
        if self.mode == "push" && self.push.hostname.is_some() && self.push.tls_cert.is_none() && self.push.tls_key.is_none() {
            tracing::info!("push.hostname is set — will attempt ACME auto-certificate");
        }
        if self.push.is_dual_port() {
            tracing::info!("push: dual-port mode enabled (port 80 → 443)");
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
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "pull"
[pull]
relay_url = "http://x"
admin_key = "k"
system_id = "s"
"#).unwrap();
        assert_eq!(cfg.mode, "pull");
        assert_eq!(cfg.push.body_limit_mb, 20);
        assert_eq!(cfg.push.rate_limit, 30);
        assert!(cfg.push.blacklist_ips.is_empty());
        assert!(cfg.push.allowed_ips.is_empty());
        assert_eq!(cfg.push.addr, "0.0.0.0:38080");
        assert!(cfg.push.hostname.is_none());
    }

    #[test]
    fn test_push_config_with_limits() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
[push]
addr = "0.0.0.0:8080"
hostname = "bridge.example.com"
blacklist_ips = ["1.2.3.4"]
allowed_ips = ["10.0.0.0/8"]
rate_limit = 100
body_limit_mb = 50
"#).unwrap();
        assert_eq!(cfg.push.rate_limit, 100);
        assert_eq!(cfg.push.body_limit_mb, 50);
        assert_eq!(cfg.push.blacklist_ips, vec!["1.2.3.4"]);
        assert_eq!(cfg.push.addr, "0.0.0.0:8080");
        assert_eq!(cfg.push.hostname, Some("bridge.example.com".into()));
        assert!(cfg.push.has_tls());
        assert!(!cfg.push.is_dual_port()); // port is 8080, not 80
    }

    #[test]
    fn test_dual_port_detection() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
[push]
addr = "0.0.0.0:80"
hostname = "example.com"
"#).unwrap();
        assert!(cfg.push.is_dual_port());
    }

    #[test]
    fn test_no_tls_without_hostname() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
[push]
addr = "0.0.0.0:38080"
"#).unwrap();
        assert!(!cfg.push.has_tls());
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
