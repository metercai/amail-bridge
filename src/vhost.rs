//! Virtual host routing for multi-domain HTTP serving.
//!
//! When `http.addr` port is 80, the server accepts requests for multiple
//! domains on the same IP.  Each site is either a static-file directory
//! or a reverse proxy to a backend.

use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use std::path::PathBuf;

use crate::config::VhostSiteConfig;

/// A resolved virtual host route, ready to handle requests.
#[derive(Debug)]
pub enum VhostRoute {
    /// Serve static files from the given directory (SPA fallback enabled).
    Static(PathBuf),
    /// Reverse-proxy requests to the given backend URL.
    /// Client is cached — built once, reused for all requests.
    Proxy(String, reqwest::Client),
    /// 301 redirect all requests to the given URL.
    Redirect(String),
}

/// Build ready-to-use routes from config entries.
/// Proxy clients are created once here and reused.
pub fn build_routes(configs: &[VhostSiteConfig]) -> Vec<(String, VhostRoute)> {
    configs
        .iter()
        .filter_map(|cfg| {
            let route = match (&cfg.root, &cfg.proxy, &cfg.redirect) {
                (Some(root), None, None) => {
                    let dir = PathBuf::from(root);
                    if !dir.exists() {
                        tracing::warn!(domain=%cfg.domain, root=%root, "vhost static root does not exist, skipping");
                        return None;
                    }
                    tracing::info!(domain=%cfg.domain, root=%root, kind="static", "vhost site loaded");
                    VhostRoute::Static(dir)
                }
                (None, Some(proxy), None) => {
                    let backend = format!("http://{}", proxy);
                    let client = match reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(30))
                        .pool_max_idle_per_host(10)
                        .build()
                    {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(domain=%cfg.domain, proxy=%proxy, error=%e, "vhost proxy client build failed, skipping");
                            return None;
                        }
                    };
                    tracing::info!(domain=%cfg.domain, proxy=%proxy, kind="proxy", "vhost site loaded");
                    VhostRoute::Proxy(backend, client)
                }
                (None, None, Some(redirect)) => {
                    tracing::info!(domain=%cfg.domain, redirect=%redirect, kind="redirect", "vhost site loaded");
                    VhostRoute::Redirect(redirect.clone())
                }
                _ => {
                    tracing::warn!(domain=%cfg.domain, "vhost must have exactly one of root, proxy, or redirect, skipping");
                    return None;
                }
            };
            Some((cfg.domain.clone(), route))
        })
        .collect()
}

/// Handle a request via the given vhost route.
pub async fn handle_vhost(route: &VhostRoute, req: Request<Body>, client_ip: Option<std::net::IpAddr>, body_limit_mb: u32) -> Response {
    tracing::info!(?route, "Vhost request");
    match route {
        VhostRoute::Redirect(url) => {
            axum::response::IntoResponse::into_response(
                axum::response::Redirect::permanent(url))
        }
        VhostRoute::Static(root_dir) => {
            // Manually serve files instead of using ServeDir::oneshot,
            // which has issues when called from a middleware context.
            let path = req.uri().path();
            let file_path = if path == "/" || path.is_empty() {
                root_dir.join("index.html")
            } else {
                // Sanitize path to prevent directory traversal
                let clean = path.trim_start_matches('/');
                root_dir.join(clean)
            };
            // Security: ensure resolved path is within root_dir
            if !file_path.starts_with(root_dir) {
                return Response::builder()
                    .status(403)
                    .body(Body::from("forbidden"))
                    .unwrap();
            }
            match tokio::fs::read(&file_path).await {
                Ok(data) => {
                    let mime = if file_path.to_string_lossy().ends_with(".html") {
                        "text/html; charset=utf-8"
                    } else if file_path.to_string_lossy().ends_with(".css") {
                        "text/css; charset=utf-8"
                    } else if file_path.to_string_lossy().ends_with(".js") {
                        "application/javascript"
                    } else {
                        "application/octet-stream"
                    };
                    Response::builder()
                        .status(200)
                        .header("content-type", mime)
                        .body(Body::from(data))
                        .unwrap()
                }
                Err(_) => {
                    // Try SPA fallback: serve index.html for any unmatched path
                    match tokio::fs::read(&root_dir.join("index.html")).await {
                        Ok(data) => Response::builder()
                            .status(200)
                            .header("content-type", "text/html; charset=utf-8")
                            .body(Body::from(data))
                            .unwrap(),
                        Err(_) => Response::builder()
                            .status(404)
                            .body(Body::from("not found"))
                            .unwrap(),
                    }
                }
            }
        }
        VhostRoute::Proxy(backend, client) => {
            let path = req
                .uri()
                .path_and_query()
                .map(|pq| pq.as_str())
                .unwrap_or("/");
            let url = format!("{}{}", backend, path);

            // Extract headers and method before consuming req body
            let method = req.method().clone();
            let mut req_headers = req.headers().clone();
            // Save original Host as X-Forwarded-Host, then remove it.
            let original_host = req_headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            req_headers.remove("host");
            req_headers.insert("x-forwarded-host",
                axum::http::HeaderValue::from_str(&original_host).unwrap_or_else(|_| axum::http::HeaderValue::from_static("unknown")));
            req_headers.insert("x-forwarded-proto",
                axum::http::HeaderValue::from_static("https"));
            req_headers.insert("x-forwarded-for",
                axum::http::HeaderValue::from_str(&client_ip.map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".into()))
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("unknown")));

            // Convert axum Body to bytes for reqwest (matches push body_limit_mb default)
            let axum_body = req.into_body();
            let body_bytes = axum::body::to_bytes(axum_body, (body_limit_mb as usize) * 1024 * 1024)
                .await
                .unwrap_or_default();
            let forwarded = client
                .request(method, &url)
                .headers(req_headers)
                .body(body_bytes);

            match forwarded.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let mut builder = Response::builder().status(status);
                    // Copy safe headers (skip hop-by-hop headers only)
                    for (k, v) in resp.headers().iter() {
                        let key = k.as_str().to_lowercase();
                        // Skip hop-by-hop headers (RFC 2616 §13.5.1)
                        if key != "transfer-encoding" && key != "connection"
                            && key != "keep-alive" && key != "proxy-authenticate"
                            && key != "proxy-authorization" && key != "te"
                            && key != "trailer" && key != "upgrade"
                        {
                            builder = builder.header(k.clone(), v.clone());
                        }
                    }
                    // Stream the response body — no buffering
                    builder
                        .body(Body::from_stream(resp.bytes_stream()))
                        .unwrap()
                }
                Err(e) => {
                    tracing::warn!(backend=%backend, error=%e, "vhost proxy upstream error");
                    Response::builder()
                        .status(502)
                        .body(Body::from("bad gateway"))
                        .unwrap()
                }
            }
        }
    }
}

/// Try to route a request to a matching virtual host.
/// Returns the route reference if the Host header matches a configured domain.
/// Falls back to the `:authority` pseudo-header for HTTP/2 connections,
/// then to the URI authority.
pub fn find_vhost<'a>(
    req: &Request<Body>,
    routes: &'a [(String, VhostRoute)],
) -> Option<&'a VhostRoute> {
    // Strategy: try multiple sources for the hostname
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            // HTTP/2: check :authority pseudo-header
            req.headers()
                .get(":authority")
                .and_then(|v| v.to_str().ok())
        })
        .map(|s| {
            // Strip port if present
            if let Some((name, _)) = s.rsplit_once(':') {
                name
            } else {
                s
            }
        })
        .unwrap_or("");
    if host.is_empty() {
        // Last resort: extract from URI authority
        if let Some(h) = req.uri().host() {
            for (domain, route) in routes {
                if h.eq_ignore_ascii_case(domain) {
                    return Some(route);
                }
            }
        }
        return None;
    }
    for (domain, route) in routes {
        if host.eq_ignore_ascii_case(domain) {
            return Some(route);
        }
    }
    None
}

