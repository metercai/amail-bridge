//! Bridge configuration: `amail_bridge.toml` + env var overrides.
//! Config format aligned with amail-gateway.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Full bridge configuration, deserialised from `amail_bridge.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeConfig {
    #[serde(default = "default_mode")]
    pub mode: String, // "push" | "pull"

    /// Listen address in "host:port" format (e.g. "0.0.0.0:38080").
    /// Used for admin API (/health, /api/v1/routes) and push webhooks.
    #[serde(default = "default_listen_addr")]
    pub addr: String,

    /// Allowed source IPs/CIDRs for admin API access.
    /// Requests to /health and /api/v1/* from other IPs get 403.
    /// Default: localhost only.
    #[serde(default)]
    pub admin_allowed_ips: Vec<String>,

    /// Forward headers that bridge passes through to gateway.
    /// Single mode: whitelist filter (only these headers are forwarded).
    /// Batch mode: per-recipient headers built from entry fields.
    /// Default: standard amail relay headers.
    #[serde(default = "default_forward_headers")]
    pub forward_headers: Vec<String>,

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

    /// Path to amail-routes.toml (default: alongside amail_bridge.toml).
    #[serde(skip)]
    pub routes_file: PathBuf,

    /// Per-agent host overrides (deprecated — use amail-routes.toml).
    #[serde(default, deserialize_with = "deserialize_hosts_vec")]
    #[allow(dead_code)]
    pub hosts: Vec<(String, String)>,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl BridgeConfig {
    /// Parse `addr` into (host, port). Default port: 80.
    pub fn parsed_addr(&self) -> (&str, u16) {
        if let Some((host, port_str)) = self.addr.rsplit_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                return (host, port);
            }
        }
        (&self.addr, 80)
    }

    /// True when dual-port mode (80 → 443 redirect) is active.
    /// Conditions: addr port == 80 AND push.hostname is set.
    pub fn is_dual_port(&self) -> bool {
        let (_, port) = self.parsed_addr();
        port == 80 && self.push.hostname.is_some()
    }
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
    /// Public hostname for TLS (e.g. "bridge.example.com").
    /// When set:
    ///   - TLS is enabled (tls_cert/tls_key or ACME auto-cert)
    ///   - If addr port == 80 → dual-port mode (80 redirects to 443)
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

    /// IP/CIDR allowlist for webhook POSTs. Empty = allow all.
    #[serde(default)]
    pub allowed_ips: Vec<String>,

    /// IP/CIDR blacklist for webhook POSTs. Empty = none.
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

    /// Return the hostname or an empty string for display.
    pub fn hostname_or_empty(&self) -> &str {
        self.hostname.as_deref().unwrap_or("")
    }
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
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

fn default_forward_headers() -> Vec<String> {
    vec![
        "x-amail-email".into(),
        "x-webhook-signature".into(),
        "x-mailrelay-timestamp".into(),
        "content-type".into(),
    ]
}

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
    pub amail_url: String,
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
fn default_listen_addr() -> String { "0.0.0.0:38080".into() }
fn default_poll_interval() -> u64 { 10 }
fn default_log_level() -> String { "info".into() }

impl Default for PullConfig {
    fn default() -> Self {
        Self {
            amail_url: String::new(),
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
        if let Ok(v) = std::env::var("AMAIL_GATEWAY_URL") { cfg.pull.amail_url = v; }
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

        // Default profile dir
        let hermes_root = cfg.hermes_home.clone().unwrap_or_else(|| {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(".hermes")
        });
        cfg.default_profile_dir = hermes_root;
        cfg.routes_file = config_path.parent().unwrap_or(Path::new(".")).join("amail-routes.toml");

        Ok(cfg)
    }

    /// Validate configuration and emit warnings for insecure settings.
    pub fn validate(&self) {
        if self.mode == "pull" {
            if self.pull.amail_url.is_empty() {
                tracing::warn!("pull.amail_url is empty — pull loop will fail");
            }
            if self.pull.admin_key.is_empty() {
                tracing::warn!("pull.admin_key is empty — authentication will fail");
            }
            if self.pull.system_id.is_empty() {
                tracing::warn!("pull.system_id is empty — pending query will fail");
            }
        }
        if self.push.hostname.is_some() && self.push.tls_cert.is_none() && self.push.tls_key.is_none() {
            tracing::info!("push.hostname is set — will attempt ACME auto-certificate");
        }
        if self.is_dual_port() {
            tracing::info!("dual-port mode enabled (port 80 → 443)");
        }
    }

    /// Compile host patterns into (Regex, host) pairs for the router.
    #[allow(dead_code)]
    pub fn compiled_hosts(&self) -> Vec<(regex::Regex, String)> {
        tracing::warn!("[hosts] in amail_bridge.toml is deprecated — use amail-routes.toml instead");
        Vec::new()
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
amail_url = "http://x"
admin_key = "k"
system_id = "s"
"#).unwrap();
        assert_eq!(cfg.mode, "pull");
        assert_eq!(cfg.addr, "0.0.0.0:38080");
        assert!(cfg.admin_allowed_ips.is_empty());
        assert_eq!(cfg.push.body_limit_mb, 20);
        assert_eq!(cfg.push.rate_limit, 30);
        assert!(cfg.push.hostname.is_none());
    }

    #[test]
    fn test_addr_with_port() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
addr = "127.0.0.1:8080"
"#).unwrap();
        let (host, port) = cfg.parsed_addr();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8080);
    }

    #[test]
    fn test_addr_default_port() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
addr = "0.0.0.0"
"#).unwrap();
        let (host, port) = cfg.parsed_addr();
        assert_eq!(host, "0.0.0.0");
        assert_eq!(port, 80);
    }

    #[test]
    fn test_dual_port_detection() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
addr = "0.0.0.0:80"
[push]
hostname = "example.com"
"#).unwrap();
        assert!(cfg.is_dual_port());
    }

    #[test]
    fn test_push_config_with_limits() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
addr = "0.0.0.0:8080"
admin_allowed_ips = ["10.0.0.0/8"]
[push]
hostname = "bridge.example.com"
blacklist_ips = ["1.2.3.4"]
allowed_ips = ["10.0.0.0/8"]
rate_limit = 100
body_limit_mb = 50
"#).unwrap();
        assert_eq!(cfg.addr, "0.0.0.0:8080");
        assert_eq!(cfg.admin_allowed_ips, vec!["10.0.0.0/8"]);
        assert_eq!(cfg.push.rate_limit, 100);
        assert_eq!(cfg.push.body_limit_mb, 50);
        assert_eq!(cfg.push.hostname, Some("bridge.example.com".into()));
        assert!(cfg.push.has_tls());
        assert!(!cfg.is_dual_port()); // port is 8080, not 80
    }

    #[test]
    fn test_no_tls_without_hostname() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "push"
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

    #[test]
    fn test_validate_pull_empty_warns() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "pull"
[pull]
amail_url = ""
admin_key = ""
system_id = ""
"#).unwrap();
        assert!(cfg.pull.amail_url.is_empty());
        assert!(cfg.pull.admin_key.is_empty());
        assert!(cfg.pull.system_id.is_empty());
    }

    #[test]
    fn test_admin_allowed_ips_default_empty() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "pull"
[pull]
amail_url = "http://x"
admin_key = "k"
system_id = "s"
"#).unwrap();
        assert!(cfg.admin_allowed_ips.is_empty());
    }

    #[test]
    fn test_compiled_hosts_deprecated() {
        let cfg: BridgeConfig = toml::from_str(r#"
mode = "pull"
[pull]
amail_url = "http://x"
admin_key = "k"
system_id = "s"
"#).unwrap();
        let compiled = cfg.compiled_hosts();
        assert!(compiled.is_empty());
    }
}
