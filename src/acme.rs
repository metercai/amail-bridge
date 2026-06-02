//! Automatic TLS certificate acquisition & renewal via Let's Encrypt (ACME).
#![cfg(feature = "tls")]
//!
//! ## Flow
//!
//! 1. Extract domain from `push.public_url` (caller).
//! 2. Check if a previously-acquired certificate is still valid (< 60 days old).
//! 3. If valid → reuse (skip ACME request, avoid LE rate limits on restart).
//! 4. If absent or expired → request via HTTP-01 challenge (port 80).
//! 5. Save cert + private key (chmod 0o600) to the cache directory.
//! 6. Spawn a background renew thread (checks every 12h, stop flag for clean shutdown).
//!
//! On any failure the caller should fall back to plain HTTP with a warning.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use acme_micro::{create_p384_key, Directory, DirectoryUrl};

/// Result of a successful ACME certificate acquisition (or cache hit).
pub struct AcmeCertPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Stop token for the background renew thread.
///
/// Set to `true` before joining the thread on graceful shutdown.
/// The thread checks this flag every 10 seconds and exits cleanly.
pub type AcmeStopToken = Arc<AtomicBool>;

// ── Public helpers ────────────────────────────────────────────────

/// Extract the bare domain from a public URL.
pub fn extract_domain(public_url: &str) -> Option<String> {
    let s = public_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let domain = s.split(':').next()?.split('/').next()?;
    if domain.is_empty() {
        None
    } else {
        Some(domain.to_string())
    }
}

// ── Public entry point ────────────────────────────────────────────

/// Get a TLS certificate for `domain`, reusing a cached one if still valid.
///
/// Returns the cert/key paths plus a stop token for the renew thread.
/// The caller should store the token and set it to `true` on shutdown.
pub fn get_or_acquire_cert(
    domain: &str,
    cache_dir: &Path,
    acme_email: Option<&str>,
) -> Result<(AcmeCertPaths, AcmeStopToken), Box<dyn std::error::Error + Send + Sync>> {
    std::fs::create_dir_all(cache_dir)?;

    let stop = Arc::new(AtomicBool::new(false));

    // 1. Try existing cert first — avoid re-requesting on restart
    if let Some(paths) = check_existing_cert(domain, cache_dir) {
        tracing::info!(%domain, cert = %paths.cert.display(), "Reusing existing ACME certificate");
        spawn_renew_thread(domain, cache_dir, acme_email, Arc::clone(&stop));
        return Ok((paths, stop));
    }

    // 2. Acquire new cert from Let's Encrypt
    tracing::info!(%domain, "Acquiring new ACME certificate");
    let paths = acquire_cert_inner(domain, cache_dir, acme_email)?;
    // chmod 0o600 on private key
    set_key_permissions(&paths.key)?;

    spawn_renew_thread(domain, cache_dir, acme_email, Arc::clone(&stop));
    Ok((paths, stop))
}

// ── Existing-cert check ───────────────────────────────────────────

/// Check whether a previously-acquired certificate is still usable.
///
/// Let's Encrypt certs are valid for 90 days; we consider one "fresh" if
/// its file modification time is within 60 days.  Using mtime avoids
/// pulling in x509 parsing dependencies.
fn check_existing_cert(domain: &str, cache_dir: &Path) -> Option<AcmeCertPaths> {
    let cert_path = cache_dir.join(format!("{}.cert.pem", domain));
    let key_path = cache_dir.join(format!("{}.key.pem", domain));

    if !cert_path.exists() || !key_path.exists() {
        return None;
    }

    let cert_age = cert_path.metadata().ok()?.modified().ok()?.elapsed().ok()?;

    let max_age = Duration::from_secs(60 * 86400); // 60 days
    if cert_age > max_age {
        tracing::info!(
            %domain,
            age_days = cert_age.as_secs() / 86400,
            "Cached certificate too old, will re-acquire"
        );
        return None;
    }

    Some(AcmeCertPaths {
        cert: cert_path,
        key: key_path,
    })
}

// ── ACME acquisition (inner) ──────────────────────────────────────

/// Perform a full ACME HTTP-01 challenge flow to obtain a certificate.
///
/// Callers should use `get_or_acquire_cert` instead — this is the raw
/// acquisition path, also used by the renew thread.
fn acquire_cert_inner(
    domain: &str,
    cache_dir: &Path,
    acme_email: Option<&str>,
) -> Result<AcmeCertPaths, Box<dyn std::error::Error + Send + Sync>> {
    let account_key_path = cache_dir.join("account.key.pem");
    let dir = Directory::from_url(DirectoryUrl::LetsEncrypt)?;
    let contact = vec![format!(
        "mailto:{}",
        acme_email.unwrap_or("acme@amail-bridge.local")
    )];

    let acc = if account_key_path.exists() {
        let pem = std::fs::read_to_string(&account_key_path)?;
        dir.load_account(&pem, contact)?
    } else {
        let acc = dir.register_account(contact)?;
        std::fs::write(&account_key_path, acc.acme_private_key_pem()?)?;
        acc
    };

    let cert_key_path = cache_dir.join(format!("{}.key.pem", domain));
    let cert_path = cache_dir.join(format!("{}.cert.pem", domain));

    let mut ord_new = acc.new_order(domain, &[])?;

    let ord_csr = loop {
        if let Some(ord_csr) = ord_new.confirm_validations() {
            break ord_csr;
        }
        let auths = ord_new.authorizations()?;
        if auths.is_empty() {
            return Err("No authorizations available for domain".into());
        }
        let chall = auths[0]
            .http_challenge()
            .ok_or("HTTP challenge not available")?;
        let token = chall.http_token().to_string();
        let proof = chall.http_proof()?;

        serve_challenge(&token, &proof)?;
        chall.validate(Duration::from_secs(5))?;
        ord_new.refresh()?;
    };

    let pkey = create_p384_key()?;
    let ord_cert = ord_csr.finalize_pkey(pkey, Duration::from_secs(5))?;
    let cert = ord_cert.download_cert()?;

    std::fs::write(&cert_key_path, cert.private_key())?;
    std::fs::write(&cert_path, cert.certificate())?;

    // Lock down private key file
    set_key_permissions(&cert_key_path)?;

    tracing::info!(%domain, cert_path = %cert_path.display(), "ACME certificate acquired");

    Ok(AcmeCertPaths {
        cert: cert_path,
        key: cert_key_path,
    })
}

// ── Permissions ───────────────────────────────────────────────────

fn set_key_permissions(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

// ── HTTP-01 challenge server ──────────────────────────────────────

fn serve_challenge(
    token: &str,
    proof: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let proof_owned = proof.to_string();
    let token_owned = token.to_string();
    let listener = TcpListener::bind("0.0.0.0:80")?;
    listener.set_nonblocking(true)?;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let expected = format!("/.well-known/acme-challenge/{}", token_owned);
                if line.contains(&expected) {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        proof_owned.len(),
                        proof_owned
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return Err("Challenge server error".into()),
        }
    }
    Ok(())
}

// ── Background renew ──────────────────────────────────────────────

/// Spawn a background thread that renews the certificate periodically.
fn spawn_renew_thread(domain: &str, cache_dir: &Path, email: Option<&str>, stop: Arc<AtomicBool>) {
    let d = domain.to_string();
    let c = cache_dir.to_path_buf();
    let e = email.map(|s| s.to_string());
    std::thread::spawn(move || {
        renew_loop(&d, &c, e.as_deref(), stop);
    });
}

/// Background renew loop — checks every 12 hours, renews after ~60 days.
///
/// Polls `stop` flag every 10 seconds so the caller can signal a clean exit.
fn renew_loop(domain: &str, cache_dir: &Path, email: Option<&str>, stop: Arc<AtomicBool>) {
    let cert_path = cache_dir.join(format!("{}.cert.pem", domain));
    let renew_age = Duration::from_secs(60 * 86400);

    loop {
        // Sleep in 10-second increments so stop flag is checked frequently
        for _ in 0..(12 * 3600 / 10) {
            if stop.load(Ordering::Relaxed) {
                tracing::info!(%domain, "ACME renew thread: stop flag received, exiting");
                return;
            }
            std::thread::sleep(Duration::from_secs(10));
        }

        let needs_renew = match std::fs::metadata(&cert_path) {
            Ok(meta) => meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age > renew_age)
                .unwrap_or(true),
            Err(_) => true,
        };

        if !needs_renew {
            continue;
        }

        if stop.load(Ordering::Relaxed) {
            return;
        }

        tracing::info!(%domain, "Renewing ACME certificate...");
        match acquire_cert_inner(domain, cache_dir, email) {
            Ok(paths) => {
                // acquire_cert_inner writes directly to cert_path/key_path
                let _ = set_key_permissions(&paths.key);
                tracing::info!(%domain, cert = %paths.cert.display(), "ACME certificate renewed");
            }
            Err(e) => tracing::error!(%domain, error = %e, "ACME renew failed — will retry"),
        }
    }
}
