//! Profile router: email → (host, port) lookup table, auto-discovered from
//! `~/.hermes/profiles/*/` and `~/.hermes/` (default profile).
//!
//! Route overrides come from `~/.hermes/amail-routes.toml`:
//!
//! ```toml
//! # Exact email → full address (highest priority)
//! "alice@admin.relay" = "127.0.0.1:8645"
//!
//! # Regex pattern → host:port (expanded to matching emails)
//! ".*@admin\\.relay" = "192.168.1.2:8645"
//! ```
//!
//! Matching priority (both in `full_scan()` expansion and `lookup()` at
//! request time):
//!   1. Exact email match
//!   2. Regex pattern fallback (keys containing `*+?[]()^$|\\`)
//!   3. No match → `None` + warning log
//!
//! Auto-discovered local profiles are written to the file after each scan,
//! so admins can see and modify all routes in one place. Manual edits
//! override auto-discovery on the next rescan. inotify watches both
//! profiles and the routes file for hot-reload.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use notify::{Event, EventKind, RecursiveMode, Watcher};

/// A single profile routing entry.
#[derive(Debug, Clone)]
pub struct ProfileRoute {
    pub email: String,
    pub host: String,
    pub port: u16,
    /// Cached target URL — computed once, reused for all lookups.
    target_url: String,
}

impl ProfileRoute {
    /// Return the cached forward target URL.
    pub fn target_url(&self) -> &str {
        &self.target_url
    }

    fn new(email: String, host: String, port: u16) -> Self {
        let target_url = format!("http://{}:{}/webhooks/amail-inbound", host, port);
        Self { email, host, port, target_url }
    }
}

/// Thread-safe email → route lookup table.
pub struct ProfileRouter {
    routes: RwLock<HashMap<String, ProfileRoute>>,
    profiles_dir: PathBuf,
    routes_file: PathBuf,
    /// Regex patterns loaded from routes file, kept for lookup-time fallback.
    /// Each entry: (compiled_regex, raw_key_string, host, port).
    regex_patterns: RwLock<Vec<(regex::Regex, String, String, u16)>>,
    /// Guard flag to prevent write-back loop when writing routes file.
    writing_routes: AtomicBool,
}

impl ProfileRouter {
    pub fn new(hermes_home: &Path, routes_file: PathBuf) -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
            profiles_dir: hermes_home.join("profiles"),
            routes_file,
            regex_patterns: RwLock::new(Vec::new()),
            writing_routes: AtomicBool::new(false),
        }
    }

    /// Full scan of all profiles, then merge with manual routes file.
    pub fn full_scan(&self) {
        // Clear guard so user edits to routes file can trigger rescans again.
        self.writing_routes.store(false, Ordering::SeqCst);

        // Step 0: load entries from file
        let file_overrides = self.load_routes_file();

        let mut routes = self.routes.write().unwrap_or_else(|e| e.into_inner());
        routes.clear();

        // Step 1: auto-discover from named profiles
        self.scan_profile_dir(&mut routes, &self.profiles_dir);

        // Step 2: default profile
        if let Some(default_dir) = self.profiles_dir.parent() {
            if let Some(r) = self.load_route(default_dir) {
                tracing::info!(email = %r.email, host = %r.host, port = r.port,
                               "Default profile route discovered");
                routes.insert(r.email.clone(), r);
            }
        }

        // Step 3: apply file entries — exact email match, then regex expansion.
        let all_emails: Vec<String> = routes.keys().cloned().collect();
        let mut raw_patterns: Vec<(regex::Regex, String, String, u16)> = Vec::new();

        for (key, route) in &file_overrides {
            // Try exact email match first
            if routes.contains_key(key) {
                routes.insert(key.clone(), route.clone());
                tracing::info!(email = %key, host = %route.host, port = route.port,
                               "Route override applied");
                continue;
            }

            // Not an exact match — try as regex pattern, expand to matching emails
            let re = match regex::Regex::new(key) {
                Ok(r) => r,
                Err(_) => {
                    tracing::warn!(key = %key, "Invalid route entry — not an email nor a valid regex");
                    continue;
                }
            };
            if Self::is_regex_pattern(key) {
                raw_patterns.push((re.clone(), key.clone(), route.host.clone(), route.port));
            }
            for email in &all_emails {
                if re.is_match(email) {
                    tracing::info!(email = %email, pattern = %key, host = %route.host, port = route.port,
                                   "Regex pattern expanded");
                    routes.insert(email.clone(), route.clone());
                }
            }
        }
        {
            let mut ptns = self.regex_patterns.write().unwrap_or_else(|e| e.into_inner());
            *ptns = raw_patterns;
        }

        let count = routes.len();
        drop(routes);
        self.write_routes_file_with(&file_overrides);

        tracing::info!(count, "Profile scan complete");
    }

    fn scan_profile_dir(&self, routes: &mut HashMap<String, ProfileRoute>, dir: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(r) = self.load_route(&path) {
                        tracing::info!(email = %r.email, host = %r.host, port = r.port,
                                       "Route discovered");
                        routes.insert(r.email.clone(), r);
                    }
                }
            }
        }
    }

    /// Parse "host:port" string into (host, port).
    /// Supports IPv4 ("192.168.1.2:8645"), IPv6 ("[::1]:8645"), and bare port ("8645").
    fn parse_addr(s: &str) -> Option<(String, u16)> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }

        // IPv6 with brackets: [::1]:8645
        if let Some(rest) = s.strip_prefix('[') {
            let (addr, port_str) = rest.split_once("]:")?;
            let port: u16 = port_str.parse().ok()?;
            return Some((format!("[{}]", addr), port));
        }

        // IPv4 or hostname with port: "192.168.1.2:8645" or "host:8645"
        if let Some((host, port_str)) = s.rsplit_once(':') {
            // Check if it looks like a port number (all digits)
            if let Ok(port) = port_str.parse::<u16>() {
                return Some((host.to_string(), port));
            }
            // If port_str isn't a number, the whole string is a hostname without port
        }

        // No delimiter found — try parsing as port only (bare number)
        if let Ok(port) = s.parse::<u16>() {
            return Some(("127.0.0.1".to_string(), port));
        }

        // Assume it's a hostname without port, use default
        Some((s.to_string(), 8645))
    }

    /// Load route entries from ~/.hermes/amail-routes.toml.
    ///
    /// All entries use `"key" = "host:port"` format.
    /// Key is either an exact email or a regex pattern.
    ///
    /// Returns exact_overrides map.
    fn load_routes_file(&self) -> HashMap<String, ProfileRoute> {
        let path = self.routes_file_path();
        let Some(path) = path else { return HashMap::new() };
        let Ok(file_content) = std::fs::read_to_string(&path) else { return HashMap::new() };

        let parsed: HashMap<String, String> = match toml::from_str(&file_content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse routes file — skipping overrides");
                return HashMap::new();
            }
        };

        let mut overrides = HashMap::new();

        for (key, value) in parsed {
            let (host, port) = match Self::parse_addr(&value) {
                Some(r) => r,
                None => {
                    tracing::warn!(key = %key, value = %value, "Invalid 'host:port' in routes file — skipping");
                    continue;
                }
            };

            overrides.insert(key.clone(), ProfileRoute::new(key, host, port));
        }

        overrides
    }

    /// Check if a key contains regex metacharacters, indicating it's a pattern
    /// rather than an exact email. `.` is excluded because it's common in emails.
    fn is_regex_pattern(key: &str) -> bool {
        key.contains(|c: char| matches!(c, '*' | '+' | '?' | '[' | ']' | '(' | ')' | '^' | '$' | '|' | '\\'))
    }

    fn routes_file_path(&self) -> Option<PathBuf> {
        Some(self.routes_file.clone())
    }

    fn load_route(&self, dir: &Path) -> Option<ProfileRoute> {
        // Read amail.json for email
        let amail_path = dir.join("amail.json");
        let amail: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&amail_path).ok()?,
        )
        .ok()?;
        let email = amail["email"].as_str()?.to_string();

        // Read config.yaml for webhook port
        let config_path = dir.join("config.yaml");
        let config: serde_yaml::Value = serde_yaml::from_str(
            &std::fs::read_to_string(&config_path).ok()?,
        )
        .ok()?;
        let port = match config["platforms"]["webhook"]["extra"]["port"].as_u64() {
            Some(p) => match u16::try_from(p) {
                Ok(port) => port,
                Err(_) => {
                    tracing::warn!(port = p, email = %email, dir = %dir.display(),
                        "Webhook port out of u16 range — skipping profile");
                    return None;
                }
            },
            None => {
                tracing::warn!(email = %email, dir = %dir.display(),
                    "No webhook port in config.yaml — skipping profile");
                return None;
            }
        };

        // Local auto-discovery always uses 127.0.0.1.
        // Regex patterns are applied later in full_scan() after exact overrides.
        Some(ProfileRoute::new(email, "127.0.0.1".to_string(), port))
    }

    /// Look up the route for a given agent email address.
    /// Priority: exact match → regex pattern fallback → None.
    pub fn lookup(&self, email: &str) -> Option<ProfileRoute> {
        let routes = self.routes.read().unwrap_or_else(|e| e.into_inner());

        // 1. Exact match
        if let Some(route) = routes.get(email) {
            return Some(route.clone());
        }
        drop(routes);

        // 2. Regex fallback — check stored patterns
        let pattern_match: Option<(String, String, u16)> = {
            let patterns = self.regex_patterns.read().unwrap_or_else(|e| e.into_inner());
            patterns.iter().find(|(re, _, _, _)| re.is_match(email))
                .map(|(_, k, h, p)| (k.clone(), h.clone(), *p))
        };

        if let Some((ref _key, ref host, port)) = pattern_match {
            tracing::info!(email = %email, host = %host, port = port, "Regex pattern matched on lookup");
            let route = ProfileRoute::new(email.to_string(), host.clone(), port);
            // Cache in routes for future lookups
            let mut w = self.routes.write().unwrap_or_else(|e| e.into_inner());
            w.insert(email.to_string(), route.clone());
            return Some(route);
        }

        // 3. No match
        tracing::warn!(email = %email, "No route found");
        None
    }

    /// Add or update a route for an exact email, then persist to file.
    /// Used by the admin API and remote agent registration.
    pub fn update_route(&self, email: &str, host: &str, port: u16) {
        let mut routes = self.routes.write().unwrap_or_else(|e| e.into_inner());
        routes.insert(email.into(), ProfileRoute::new(email.into(), host.into(), port));
        drop(routes);
        let overrides = self.load_routes_file();
        self.write_routes_file_with(&overrides);
    }

    /// Remove a route by exact email, then persist to file.
    pub fn remove_route(&self, email: &str) {
        let mut routes = self.routes.write().unwrap_or_else(|e| e.into_inner());
        routes.remove(email);
        drop(routes);
        let overrides = self.load_routes_file();
        self.write_routes_file_with(&overrides);
    }

    /// Return all current routes.
    pub fn list_routes(&self) -> Vec<ProfileRoute> {
        self.routes.read().unwrap_or_else(|e| e.into_inner())
            .values().cloned().collect()
    }

    #[allow(dead_code)]
    pub fn route_count(&self) -> usize {
        self.routes.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Return all known agent emails (for pull-mode email filtering).
    pub fn list_emails(&self) -> Vec<String> {
        self.routes.read().unwrap_or_else(|e| e.into_inner())
            .keys().cloned().collect()
    }

    /// Write the route table to ~/.hermes/amail-routes.toml.
    /// Preserves regex-pattern entries from the file (non-exact keys).
    fn write_routes_file_with(&self, file_overrides: &HashMap<String, ProfileRoute>) {
        let path = self.routes_file_path();
        let Some(path) = path else { return };

        self.writing_routes.store(true, Ordering::SeqCst);
        let routes = self.routes.read().unwrap_or_else(|e| e.into_inner());
        let mut out = String::from(
            "# Auto-generated by amail-bridge.\n\
             # File changes take effect immediately.\n"
        );

        // Write regex-pattern entries from file (keys that look like patterns)
        for (key, route) in file_overrides {
            if Self::is_regex_pattern(key) {
                out.push_str(&format!("\"{}\" = \"{}:{}\"\n", key, route.host, route.port));
            }
        }

        // Write all current routes as "email" = "host:port"
        let mut keys: Vec<&String> = routes.keys().collect();
        keys.sort();
        for email in keys {
            if let Some(route) = routes.get(email) {
                out.push_str(&format!(
                    "\"{}\" = \"{}:{}\"\n",
                    email, route.host, route.port
                ));
            }
        }

        if let Err(e) = std::fs::write(&path, &out) {
            tracing::warn!(path = %path.display(), error = %e, "Failed to write routes file");
        }
        // Flag stays true until next full_scan() clears it.
        // This blocks our own write's Modify event from re-triggering a scan.
        // User edits to the routes file are picked up on the next profile
        // change event (which triggers full_scan → clears flag → reads file).
    }
}

/// Start the inotify watcher in a background task.
/// Returns immediately after registering the watcher and performing the initial scan.
pub fn start_watcher(router: Arc<ProfileRouter>) -> notify::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })?;

    let watch_dir = router.profiles_dir.clone();
    if watch_dir.exists() {
        watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    } else {
        tracing::warn!(dir = %watch_dir.display(), "Profiles directory not found — routes will be empty");
    }

    // Watch default profile files specifically (not entire parent dir
    // which contains frequently-updated files like models_dev_cache.json)
    // Also watch the routes file so manual edits take effect immediately.
    if let Some(parent) = watch_dir.parent() {
        for f in &["amail.json", "config.yaml"] {
            let p = parent.join(f);
            if p.exists() {
                let _ = watcher.watch(&p, RecursiveMode::NonRecursive);
            }
        }
        // Watch routes file (written by full_scan below, so exists afterwards)
        let routes_path = router.routes_file.clone();
        if routes_path.exists() {
            if let Err(e) = watcher.watch(&routes_path, RecursiveMode::NonRecursive) {
                tracing::warn!(path = %routes_path.display(), error = %e,
                    "Failed to watch routes file — edits won't hot-reload");
            }
        }
    }

    // Initial full scan
    router.full_scan();

    // Background event loop — owns watcher to keep it alive
    tokio::spawn(async move {
        let _watcher = watcher; // keep alive for the lifetime of this task
        while let Ok(event) = rx.recv() {
            let should_rescan = match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                    event.paths.iter().any(|p| {
                        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if name == "amail-routes.toml"
                            && *p == router.routes_file
                            && router.writing_routes.load(Ordering::SeqCst)
                        {
                            // Our own write_routes_file_with — skip to avoid loop
                            return false;
                        }
                        p.is_dir()
                            || name == "amail.json"
                            || name == "config.yaml"
                            || name == "amail-routes.toml"
                    })
                }
                _ => false,
            };
            if should_rescan {
                tracing::debug!(paths = ?event.paths, "Profile filesystem change detected");
                router.full_scan();
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_profile(dir: &std::path::Path, email: &str, port: u16) {
        std::fs::create_dir_all(dir).unwrap();
        let amail = serde_json::json!({"email": email});
        std::fs::write(dir.join("amail.json"), amail.to_string()).unwrap();
        let config = format!("platforms:\n  webhook:\n    extra:\n      port: {port}\n");
        std::fs::write(dir.join("config.yaml"), config).unwrap();
    }

    // ── parse_addr ──────────────────────────────────────

    #[test]
    fn test_parse_addr_ipv4_with_port() {
        let (host, port) = ProfileRouter::parse_addr("192.168.1.2:8645").unwrap();
        assert_eq!(host, "192.168.1.2");
        assert_eq!(port, 8645);
    }

    #[test]
    fn test_parse_addr_ipv6_with_port() {
        let (host, port) = ProfileRouter::parse_addr("[::1]:8645").unwrap();
        assert_eq!(host, "[::1]");
        assert_eq!(port, 8645);
    }

    #[test]
    fn test_parse_addr_bare_port() {
        let (host, port) = ProfileRouter::parse_addr("8645").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8645);
    }

    #[test]
    fn test_parse_addr_bare_hostname() {
        let (host, port) = ProfileRouter::parse_addr("myhost.local").unwrap();
        assert_eq!(host, "myhost.local");
        assert_eq!(port, 8645);
    }

    #[test]
    fn test_parse_addr_empty() {
        assert!(ProfileRouter::parse_addr("").is_none());
    }

    #[test]
    fn test_parse_addr_hostname_with_port() {
        let (host, port) = ProfileRouter::parse_addr("bridge.example.com:38080").unwrap();
        assert_eq!(host, "bridge.example.com");
        assert_eq!(port, 38080);
    }

    // ── load_route ──────────────────────────────────────

    #[test]
    fn test_load_route_valid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prof = tmp.path().join("profiles").join("test-agent");
        make_profile(&prof, "alice@x.com", 8645);
        let router = ProfileRouter::new(tmp.path(), tmp.path().join("amail-routes.toml"));
        let route = router.load_route(&prof).unwrap();
        assert_eq!(route.email, "alice@x.com");
        assert_eq!(route.host, "127.0.0.1");
        assert_eq!(route.port, 8645);
    }

    // ── full_scan ───────────────────────────────────────

    #[test]
    fn test_full_scan_and_lookup() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles")).unwrap();
        let prof = tmp.path().join("profiles").join("a1");
        make_profile(&prof, "a1@t.local", 8001);
        let router = ProfileRouter::new(tmp.path(), tmp.path().join("amail-routes.toml"));
        router.full_scan();
        assert_eq!(router.route_count(), 1);
        assert!(router.lookup("a1@t.local").is_some());
        assert!(router.lookup("nobody@t.local").is_none());
    }

    #[test]
    fn test_routes_file_written_after_full_scan() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles")).unwrap();
        let prof = tmp.path().join("profiles").join("a1");
        make_profile(&prof, "a1@t.local", 8001);
        let router = ProfileRouter::new(tmp.path(), tmp.path().join("amail-routes.toml"));
        router.full_scan();
        // Routes file should exist after full_scan
        let routes_file = tmp.path().join("amail-routes.toml");
        assert!(routes_file.exists(), "Routes file should exist after full_scan");
        // Verify content
        let content = std::fs::read_to_string(&routes_file).unwrap();
        assert!(content.contains("a1@t.local"), "Routes file should contain discovered email");
        assert!(content.contains("127.0.0.1:8001"), "Routes file should contain addr");
    }

    #[test]
    fn test_routes_file_overrides_auto_discovery() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles")).unwrap();
        let prof = tmp.path().join("profiles").join("a1");
        make_profile(&prof, "a1@t.local", 8001);
        // Write routes file with a manual override BEFORE full_scan
        let routes_file = tmp.path().join("amail-routes.toml");
        std::fs::write(&routes_file, r#""a1@t.local" = "10.0.0.5:9999""#).unwrap();
        let router = ProfileRouter::new(tmp.path(), tmp.path().join("amail-routes.toml"));
        router.full_scan();
        // The file's override should take precedence
        let route = router.lookup("a1@t.local").unwrap();
        assert_eq!(route.host, "10.0.0.5", "File override should win over auto-discovery");
        assert_eq!(route.port, 9999, "File override should win over auto-discovery");
    }

    #[test]
    fn test_routes_file_persists_across_scans() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles")).unwrap();
        let prof = tmp.path().join("profiles").join("a1");
        make_profile(&prof, "a1@t.local", 8001);
        let router = ProfileRouter::new(tmp.path(), tmp.path().join("amail-routes.toml"));
        router.full_scan();
        // First scan: auto-discovered
        let route1 = router.lookup("a1@t.local").unwrap();
        assert_eq!(route1.host, "127.0.0.1");
        // Manually edit the routes file (simulate admin intervention)
        let routes_file = tmp.path().join("amail-routes.toml");
        std::fs::write(&routes_file,
            r#""a1@t.local" = "10.0.0.99:7777""#).unwrap();
        // Second scan: file should override
        router.full_scan();
        let route2 = router.lookup("a1@t.local").unwrap();
        assert_eq!(route2.host, "10.0.0.99", "Manual edit should survive rescan");
        assert_eq!(route2.port, 7777);
    }

    #[test]
    fn test_routes_file_preserves_manual_entries_not_in_profiles() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles")).unwrap();
        // Write routes file with a manual entry for a non-existent profile
        let routes_file = tmp.path().join("amail-routes.toml");
        std::fs::write(&routes_file,
            r#""remote@admin.relay" = "192.168.1.100:8645""#).unwrap();
        let router = ProfileRouter::new(tmp.path(), tmp.path().join("amail-routes.toml"));
        router.full_scan();
        // Entry has no matching profile and contains no regex metacharacters —
        // not a pattern, not an exact email. Ignored entirely.
        assert!(router.lookup("remote@admin.relay").is_none(),
            "No matching profile — entry not in routes");
        // The entry doesn't look like a regex pattern (no metacharacters),
        // so it's not preserved in the file.
        let content = std::fs::read_to_string(&routes_file).unwrap();
        assert!(!content.contains("remote@admin.relay"),
            "Entry without regex metacharacters is not preserved");
    }
    #[test]
    fn test_routes_file_host_pattern_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("profiles")).unwrap();
        // Create a profile for a local agent
        make_profile(&tmp.path().join("profiles").join("a1"), "a1@admin.relay", 8765);
        // Write routes file with a regex pattern (also "host:port" format)
        let routes_file = tmp.path().join("amail-routes.toml");
        std::fs::write(&routes_file, r#"".*@admin" = "10.0.0.88:8765""#).unwrap();
        let router = ProfileRouter::new(tmp.path(), tmp.path().join("amail-routes.toml"));
        router.full_scan();
        // The host and port should come from the regex pattern
        let route = router.lookup("a1@admin.relay").unwrap();
        assert_eq!(route.host, "10.0.0.88", "Regex '.*@admin' should match");
        assert_eq!(route.port, 8765, "Port from regex pattern");
    }

}