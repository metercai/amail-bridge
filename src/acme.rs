//! Automatic TLS certificate acquisition & renewal via Let's Encrypt (ACME).
#![cfg(feature = "tls")]
//!
//! ## Flow
//!
//! 1. Extract domain from `push.public_url`.
//! 2. Create or load ACME account from `acme_cache` directory.
//! 3. Request a certificate via HTTP-01 challenge (port 80).
//! 4. Save the cert + private key to the cache directory.
//! 5. Spawn a background renew task (checks every 12 hours).
//!
//! On any failure the caller should fall back to plain HTTP with a warning.

use std::path::{Path, PathBuf};
use std::time::Duration;

use acme_micro::{create_p384_key, Directory, DirectoryUrl};

/// Result of a successful ACME certificate acquisition.
pub struct AcmeCertPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Extract the bare domain from a public URL.
pub fn extract_domain(public_url: &str) -> Option<String> {
    let s = public_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let domain = s.split(':').next()?.split('/').next()?;
    if domain.is_empty() { None } else { Some(domain.to_string()) }
}

/// Acquire a TLS certificate from Let's Encrypt via HTTP-01 challenge.
pub fn acquire_cert(
    domain: &str,
    cache_dir: &Path,
    acme_email: Option<&str>,
) -> Result<AcmeCertPaths, Box<dyn std::error::Error + Send + Sync>> {
    std::fs::create_dir_all(cache_dir)?;

    let account_key_path = cache_dir.join("account.key.pem");
    let dir = Directory::from_url(DirectoryUrl::LetsEncrypt)?;
    let contact = vec![format!("mailto:{}", acme_email.unwrap_or("acme@amail-bridge.local"))];

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

    // Order a new cert — may require HTTP-01 challenge
    let mut ord_new = acc.new_order(domain, &[])?;

    let ord_csr = loop {
        if let Some(ord_csr) = ord_new.confirm_validations() {
            break ord_csr;
        }
        let auths = ord_new.authorizations()?;
        if auths.is_empty() {
            return Err("No authorizations available for domain".into());
        }
        let chall = auths[0].http_challenge().ok_or("HTTP challenge not available")?;
        let token = chall.http_token().to_string();
        let proof = chall.http_proof()?;

        // Serve the challenge via a temporary HTTP server on port 80
        serve_challenge(&token, &proof)?;

        chall.validate(Duration::from_secs(5))?;
        ord_new.refresh()?;
    };

    let pkey = create_p384_key()?;
    let ord_cert = ord_csr.finalize_pkey(pkey, Duration::from_secs(5))?;
    let cert = ord_cert.download_cert()?;

    // Save cert + key
    std::fs::write(&cert_key_path, cert.private_key())?;
    std::fs::write(&cert_path, cert.certificate())?;

    tracing::info!(%domain, cert_path = %cert_path.display(), "ACME certificate acquired");

    let renew_domain = domain.to_string();
    let renew_cache = cache_dir.to_path_buf();
    let renew_email = acme_email.map(|s| s.to_string());
    std::thread::spawn(move || {
        renew_loop(&renew_domain, &renew_cache, renew_email.as_deref());
    });

    Ok(AcmeCertPaths { cert: cert_path, key: cert_key_path })
}

fn serve_challenge(token: &str, proof: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Use a blocking HTTP server for simplicity (acme-micro is synchronous)
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let proof_owned = proof.to_string();
    let token_owned = token.to_string();
    let listener = TcpListener::bind("0.0.0.0:80")?;
    listener.set_nonblocking(true)?;

    // Accept connections for up to 30 seconds
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                // Only respond to the challenge token
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

/// Background renew loop — checks every 12 hours, renews after ~60 days.
fn renew_loop(domain: &str, cache_dir: &Path, email: Option<&str>) {
    let cert_path = cache_dir.join(format!("{}.cert.pem", domain));
    let key_path = cache_dir.join(format!("{}.key.pem", domain));
    let renew_age = Duration::from_secs(60 * 86400);

    loop {
        std::thread::sleep(Duration::from_secs(12 * 3600));

        let needs_renew = match std::fs::metadata(&cert_path) {
            Ok(meta) => meta.modified().ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age > renew_age)
                .unwrap_or(true),
            Err(_) => true,
        };

        if !needs_renew { continue; }

        tracing::info!(%domain, "Renewing ACME certificate...");
        match acquire_cert(domain, cache_dir, email) {
            Ok(paths) => {
                let _ = std::fs::rename(&paths.cert, &cert_path);
                let _ = std::fs::rename(&paths.key, &key_path);
                tracing::info!(%domain, "ACME certificate renewed");
            }
            Err(e) => tracing::error!(%domain, error = %e, "ACME renew failed — will retry"),
        }
    }
}
