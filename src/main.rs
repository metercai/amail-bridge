//! amail-bridge — transparent bridge between amail relay and Hermes gateway.
//!
//! Two modes:
//! - **push**: expose a single external endpoint, transparently proxy to gateway webhook ports.
//! - **pull**: outbound long-poll relay's /pending endpoint, forward to gateway, ACK on success.
//!
//! ## CLI
//!
//! ```text
//! amail-bridge [--daemon] [--pid-file <path>] [--log-file <path>] [--config <path>]
//! ```
//!
//! `--daemon` detaches from the terminal (double-fork), redirects stdio to
//! the log file (default: ~/.hermes/amail-bridge.log), and writes a PID file
//! (default: ~/.hermes/amail-bridge.pid).

mod acme;
mod config;
mod pull;
mod push;
mod router;
mod vhost;
mod security;

use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::BridgeConfig;

/// CLI args parsed before daemonize (no async / tokio dependency).
#[derive(Default)]
pub struct CliArgs {
    pub daemon: bool,
    pub pid_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
}

pub fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let mut cli = CliArgs::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--daemon" | "-d" => cli.daemon = true,
            "--pid-file" => { i += 1; cli.pid_file = Some(PathBuf::from(&args[i])); }
            "--log-file" => { i += 1; cli.log_file = Some(PathBuf::from(&args[i])); }
            "--config" | "-c" => { i += 1; cli.config_path = Some(PathBuf::from(&args[i])); }
            "--help" | "-h" => {
                println!("amail-bridge — transparent relay-gateway bridge\n");
                println!("Usage: amail-bridge [OPTIONS]\n");
                println!("Options:");
                println!("  -d, --daemon       Detach from terminal, run in background");
                println!("  --pid-file <path>  PID file path (default: ~/.hermes/amail-bridge.pid)");
                println!("  --log-file <path>  Log file path (default: ~/.hermes/amail-bridge.log)");
                println!("  -c, --config <path> Config file path (default: ./amail_bridge.toml)");
                println!("  -h, --help         Show this help");
                process::exit(0);
            }
            other => {
                eprintln!("Unknown flag: {}\nUse --help for usage.", other);
                process::exit(1);
            }
        }
        i += 1;
    }
    cli
}

/// Double-fork daemonize: detach from terminal, redirect stdio.
/// MUST be called BEFORE any tokio runtime is created.
#[cfg(unix)]
pub fn daemonize(pid_file: &PathBuf, log_file: &PathBuf) {
    // Ensure log directory exists
    if let Some(parent) = log_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // First fork
    match unsafe { libc::fork() } {
        -1 => { eprintln!("fork failed"); process::exit(1); }
        0  => {} // child continues
        _   => process::exit(0), // parent exits
    }

    // New session — detach from terminal
    if unsafe { libc::setsid() } == -1 {
        eprintln!("setsid failed"); process::exit(1);
    }

    // Second fork — no longer session leader, can't reacquire terminal
    match unsafe { libc::fork() } {
        -1 => { eprintln!("second fork failed"); process::exit(1); }
        0  => {} // grandchild continues
        _   => process::exit(0),
    }

    // Redirect stdio to log file
    let log = match std::fs::OpenOptions::new()
        .create(true).append(true).open(log_file)
    {
        Ok(f) => f,
        Err(e) => { eprintln!("Cannot open log file {:?}: {}", log_file, e); process::exit(1); }
    };
    use std::os::unix::io::AsRawFd;
    let log_fd = log.as_raw_fd();
    unsafe {
        libc::dup2(log_fd, 0); // stdin
        libc::dup2(log_fd, 1); // stdout
        libc::dup2(log_fd, 2); // stderr
    }
    drop(log);

    // Write PID file
    if let Some(parent) = pid_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(pid_file, process::id().to_string().as_bytes()) {
        eprintln!("Cannot write PID file {:?}: {}", pid_file, e);
        process::exit(1);
    }
}

/// Daemon mode is not available on non-Unix platforms.
/// Use the platform's native service manager instead (launchd, sc.exe, etc.).
#[cfg(all(not(unix), not(windows)))]
pub fn daemonize(_pid_file: &PathBuf, _log_file: &PathBuf) {
    eprintln!("--daemon is not supported on this platform. Use the native service manager.");
    process::exit(1);
}

/// Windows daemonize: spawn a detached child process (no console window),
/// then exit the parent.  The child detects it is already detached and
/// continues normally — equivalent to Unix double-fork semantics.
#[cfg(windows)]
pub fn daemonize(pid_file: &PathBuf, log_file: &PathBuf) {
    extern "system" {
        fn GetConsoleWindow() -> isize;
    }

    // Guard: if already detached, we are the child — set up PID + logs
    if unsafe { GetConsoleWindow() } == 0 {
        // Write PID file
        if let Some(parent) = pid_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(pid_file, process::id().to_string().as_bytes());
        // Redirect stdio to log file
        if let Ok(log) = std::fs::OpenOptions::new()
            .create(true).append(true).open(log_file)
        {
            let _ = log; // keep alive
            // On Windows, the parent already nulled our stdio handles;
            // tracing output goes to the tokio runtime's writer.
        }
        return;
    }

    // Parent: respawn without a console window
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let exe = std::env::current_exe().expect("Cannot get executable path");
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Spawn detached child with all CLI args intact (including --daemon)
    std::process::Command::new(exe)
        .args(&args)
        .creation_flags(DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("Failed to spawn detached background process");

    // Parent exits — child takes over
    process::exit(0);
}

// ── entry point ────────────────────────────────────────────────────────
// Daemonize BEFORE tokio runtime creation to avoid forking a live runtime.

pub fn main() {
    let cli = parse_args();

    // Resolve default paths (no tokio needed)
    let hermes_home = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".hermes");
    let pid_file = cli.pid_file.clone().unwrap_or_else(|| hermes_home.join("amail-bridge.pid"));
    let log_file = cli.log_file.clone().unwrap_or_else(|| hermes_home.join("amail-bridge.log"));

    // Daemonize before any tokio runtime exists
    if cli.daemon {
        daemonize(&pid_file, &log_file);
    }

    // Now it's safe to create the tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime");

    rt.block_on(async {
        let daemon = cli.daemon;
        let pid_file_display = pid_file.clone();
        let result = async_main(cli, pid_file, log_file).await;
        // Always clean up PID file, even on error
        if daemon {
            let _ = std::fs::remove_file(&pid_file_display);
        }
        if let Err(e) = result {
            tracing::error!(error = %e, "Fatal error");
            std::process::exit(1);
        }
    });
}

async fn async_main(
    cli: CliArgs,
    _pid_file: PathBuf,
    log_file: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // Install ring as the default rustls crypto provider (must be done once at startup)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring crypto provider");

    let config = BridgeConfig::load(cli.config_path.as_deref())?;

    // Init tracing from config (amail-gateway compatible)
    init_tracing(&config.logging, cli.daemon, &log_file);

    tracing::info!("amail-bridge starting (pid={})", process::id());

    config.validate();
    let router = Arc::new(router::ProfileRouter::new(
        &config.default_profile_dir,
        config.routes_file.clone(),
    ));

    if let Err(e) = router::start_watcher(router.clone()) {
        tracing::warn!(error = %e, "Profile watcher failed to start — routes may be stale");
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = async {
                #[cfg(unix)]
                {
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                        Ok(mut sigterm) => sigterm.recv().await,
                        Err(e) => {
                            tracing::warn!(error = %e, "SIGTERM handler unavailable — falling back to SIGINT only");
                            Some(std::future::pending::<()>().await)
                        }
                    }
                }
                #[cfg(not(unix))]
                std::future::pending::<()>().await;
            } => {},
        }
        tracing::info!("Shutdown signal received, initiating graceful shutdown...");
        shutdown_clone.store(true, Ordering::SeqCst);
    });

    match config.mode.as_str() {
        "push" => push::start_push_server(config, router, shutdown).await?,
        "pull" => pull::start_pull_loop(config, router, shutdown).await?,
        other => {
            tracing::error!(mode = %other, "Unknown mode. Use 'push' or 'pull'.");
            return Err(format!("Unknown mode: {other}").into());
        }
    }

    Ok(())
}

/// Initialize tracing subscriber from LoggingConfig (amail-gateway compatible).
/// When daemon mode is active, log file from CLI args takes precedence.
fn init_tracing(cfg: &crate::config::LoggingConfig, daemon: bool, daemon_log: &std::path::Path) {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cfg.level));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);

    // Determine log destination: daemon log file > config log file > stdout
    let writer: Box<dyn std::io::Write + Send> = if daemon {
        let file = std::fs::OpenOptions::new()
            .create(true).append(true).open(daemon_log)
            .unwrap_or_else(|e| panic!("failed to open daemon log file {:?}: {}", daemon_log, e));
        Box::new(file)
    } else if let Some(ref path) = cfg.file {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true).append(true).open(path)
            .unwrap_or_else(|e| panic!("failed to open log file {:?}: {}", path, e));
        Box::new(file)
    } else {
        Box::new(std::io::stdout())
    };

    let (non_blocking, _guard) = tracing_appender::non_blocking(writer);
    builder.with_writer(non_blocking).init();
    std::mem::forget(_guard);
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn test_parse_args_default() {
        let cli = parse_args_from(&["amail-bridge".into()]);
        assert!(!cli.daemon);
        assert!(cli.pid_file.is_none());
        assert!(cli.log_file.is_none());
        assert!(cli.config_path.is_none());
    }

    #[test]
    fn test_parse_args_daemon() {
        let cli = parse_args_from(&["amail-bridge".into(), "--daemon".into()]);
        assert!(cli.daemon);
    }

    #[test]
    fn test_parse_args_config() {
        let cli = parse_args_from(&[
            "amail-bridge".into(),
            "-c".into(),
            "/tmp/test.toml".into(),
        ]);
        assert_eq!(cli.config_path, Some(PathBuf::from("/tmp/test.toml")));
    }

    #[test]
    fn test_parse_args_pid_file() {
        let cli = parse_args_from(&[
            "amail-bridge".into(),
            "--pid-file".into(),
            "/var/run/bridge.pid".into(),
        ]);
        assert_eq!(cli.pid_file, Some(PathBuf::from("/var/run/bridge.pid")));
    }

    #[test]
    fn test_parse_args_log_file() {
        let cli = parse_args_from(&[
            "amail-bridge".into(),
            "--log-file".into(),
            "/var/log/bridge.log".into(),
        ]);
        assert_eq!(cli.log_file, Some(PathBuf::from("/var/log/bridge.log")));
    }
}

/// Parse CLI arguments from a custom args slice (testable).
pub fn parse_args_from(args: &[String]) -> CliArgs {
    let mut cli = CliArgs::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--daemon" | "-d" => cli.daemon = true,
            "--pid-file" => { i += 1; cli.pid_file = Some(PathBuf::from(&args[i])); }
            "--log-file" => { i += 1; cli.log_file = Some(PathBuf::from(&args[i])); }
            "--config" | "-c" => { i += 1; cli.config_path = Some(PathBuf::from(&args[i])); }
            _ => {}
        }
        i += 1;
    }
    cli
}

