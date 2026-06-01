//! Profile router: email → (host, port) lookup table, auto-discovered from
//! `~/.hermes/profiles/*/` and `~/.hermes/` (default profile).
//!
//! Per-agent host overrides come from `[hosts]` in `amail_bridge.toml`
//! for multi-machine deployments. Agents without an explicit host entry
//! default to `127.0.0.1` (same-machine).
//!
//! Uses `inotify` (via the `notify` crate) to watch for profile changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use notify::{Event, EventKind, RecursiveMode, Watcher};

/// A single profile routing entry.
#[derive(Debug, Clone)]
pub struct ProfileRoute {
    pub email: String,
    pub host: String,
    pub port: u16,
}

impl ProfileRoute {
    /// Build the forward target URL from this route.
    pub fn target_url(&self) -> String {
        format!("http://{}:{}/webhooks/amail-inbound", self.host, self.port)
    }
}

/// Thread-safe email → route lookup table.
pub struct ProfileRouter {
    routes: RwLock<HashMap<String, ProfileRoute>>,
    profiles_dir: PathBuf,
    /// Compiled regex-pattern → host overrides. First match wins.
    host_patterns: Vec<(regex::Regex, String)>,
}

impl ProfileRouter {
    pub fn new(hermes_home: &Path, host_patterns: Vec<(regex::Regex, String)>) -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
            profiles_dir: hermes_home.join("profiles"),
            host_patterns,
        }
    }

    /// Full scan of all profiles, then merge with manual routes file.
    pub fn full_scan(&self) {
        let mut routes = self.routes.write().unwrap_or_else(|e| e.into_inner());
        routes.clear();

        // Step 1: auto-discover from profiles
        self.scan_profile_dir(&mut routes, &self.profiles_dir);
        if let Some(default_dir) = self.profiles_dir.parent().map(|p| p.to_path_buf()) {
            self.scan_profile_dir(&mut routes, &default_dir);
        }

        // Step 2: merge manual overrides from routes file
        self.merge_routes_file(&mut routes);

        // Step 3: write the merged result back for visibility
        self.write_routes_file();

        tracing::info!(count = routes.len(), "Profile scan complete");
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

    /// Merge manual overrides from ~/.hermes/amail-bridge-routes.toml.
    /// Existing auto-discovered entries are overwritten; entries only in
    /// the file are added (supports agents not yet auto-discovered).
    fn merge_routes_file(&self, routes: &mut HashMap<String, ProfileRoute>) {
        let path = self.routes_file_path();
        let Some(path) = path else { return };
        let Ok(content) = std::fs::read_to_string(&path) else { return };

        #[derive(serde::Deserialize)]
        struct RouteEntry { host: String, port: u16 }

        let parsed: HashMap<String, RouteEntry> = match toml::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse routes file — skipping overrides");
                return;
            }
        };

        let mut overridden = 0usize;
        let mut added = 0usize;
        for (email, entry) in parsed {
            let route = ProfileRoute {
                email: email.clone(),
                host: entry.host,
                port: entry.port,
            };
            if routes.contains_key(&email) {
                overridden += 1;
            } else {
                added += 1;
            }
            routes.insert(email, route);
        }
        if overridden > 0 || added > 0 {
            tracing::info!(overridden, added, "Merged manual routes from file");
        }
    }

    fn routes_file_path(&self) -> Option<PathBuf> {
        self.profiles_dir.parent().map(|p| p.join("amail-bridge-routes.toml"))
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
        let port = config["platforms"]["webhook"]["extra"]["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())?;

        // Resolve host: first regex match wins, default to 127.0.0.1
        let host = self.host_patterns
            .iter()
            .find(|(re, _)| re.is_match(&email))
            .map(|(_, h)| h.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string());

        Some(ProfileRoute { email, host, port })
    }

    /// Look up the route for a given agent email address.
    pub fn lookup(&self, email: &str) -> Option<ProfileRoute> {
        self.routes.read().unwrap_or_else(|e| e.into_inner())
            .get(email).cloned()
    }

    pub fn route_count(&self) -> usize {
        self.routes.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Return all known agent emails (for pull-mode email filtering).
    pub fn list_emails(&self) -> Vec<String> {
        self.routes.read().unwrap_or_else(|e| e.into_inner())
            .keys().cloned().collect()
    }

    /// Write the route table to ~/.hermes/amail-bridge-routes.toml.
    fn write_routes_file(&self) {
        let path = self.routes_file_path();
        let Some(path) = path else { return };

        let routes = self.routes.read().unwrap_or_else(|e| e.into_inner());
        let mut out = String::from(
            "# Auto-generated by amail-bridge.\n\
             # Edit to override host/port. Changes take effect on next file watch event.\n\n"
        );
        for email in {
            let mut keys: Vec<&String> = routes.keys().collect();
            keys.sort();
            keys
        } {
            if let Some(route) = routes.get(email) {
                out.push_str(&format!(
                    "{} = {{ host = \"{}\", port = {} }}\n",
                    email, route.host, route.port
                ));
            }
        }
        if let Err(e) = std::fs::write(&path, &out) {
            tracing::warn!(path = %path.display(), error = %e, "Failed to write routes file");
        }
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
    }

    // Watch parent (~/.hermes/) for default profile changes
    if let Some(parent) = watch_dir.parent() {
        let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
    }

    // Initial full scan
    router.full_scan();

    // Background event loop — owns watcher to keep it alive
    tokio::spawn(async move {
        let _watcher = watcher; // keep alive for the lifetime of this task
        while let Ok(event) = rx.recv() {
            let should_rescan = match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
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
