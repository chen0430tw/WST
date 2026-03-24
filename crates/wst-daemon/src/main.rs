//! WST Daemon - System resident process for WST

use anyhow::Result;
use std::env;
use std::process;
use wst_config::WstConfig;
use wst_daemon::WstDaemon;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    init_logging();

    let args: Vec<String> = env::args().collect();

    // Parse command line arguments
    match args.get(1).map(|s| s.as_str()) {
        Some("--version") | Some("-v") => {
            println!("WST Daemon version {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            print_usage();
            return Ok(());
        }
        Some("--install") => {
            #[cfg(windows)]
            {
                return wst_daemon::lifecycle::install_service();
            }
            #[cfg(not(windows))]
            {
                eprintln!("Service installation is only supported on Windows");
                process::exit(1);
            }
        }
        Some("--uninstall") => {
            #[cfg(windows)]
            {
                return wst_daemon::lifecycle::uninstall_service();
            }
            #[cfg(not(windows))]
            {
                eprintln!("Service installation is only supported on Windows");
                process::exit(1);
            }
        }
        Some("--stop") => {
            // Stop running daemon
            let client = wst_daemon::ipc::IpcClient::new();
            if client.ping().await {
                println!("Stopping WST daemon...");
                client.shutdown().await?;
                println!("WST daemon stopped");
                return Ok(());
            } else {
                eprintln!("WST daemon is not running");
                process::exit(1);
            }
        }
        Some("--status") => {
            let client = wst_daemon::ipc::IpcClient::new();
            if client.ping().await {
                println!("WST daemon is running");

                let sessions = client.list_sessions().await?;
                println!("Active sessions: {}", sessions.len());
                for session in sessions {
                    println!("  - {}: {} ({} tasks)", session.name, session.backend, session.task_count);
                }

                return Ok(());
            } else {
                println!("WST daemon is not running");
                process::exit(0);
            }
        }
        None | Some("--daemon") => {
            // Run as daemon (default)
        }
        Some(arg) => {
            eprintln!("Unknown argument: {}", arg);
            print_usage();
            process::exit(1);
        }
    }

    // Load configuration
    let config = WstConfig::load_default()
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to load config: {}, using defaults", e);
            WstConfig::default()
        });

    let hotkey_str = config.hotkey_global.as_deref()
        .unwrap_or(&config.hotkey)
        .to_string();

    // Check if already running
    if let Ok(true) = wst_daemon::lifecycle::check_singleton() {
        eprintln!("WST daemon is already running");
        eprintln!("Use --stop to stop the running instance");
        process::exit(1);
    }

    // Create and run daemon
    let daemon = WstDaemon::new(config)?;

    println!();
    println!("========================================");
    println!("WST Daemon starting...");
    println!("Global hotkey: {}", hotkey_str);
    println!("========================================");
    println!();

    tracing::info!("Starting WST daemon");
    daemon.run().await?;

    Ok(())
}

/// Initialize logging to file and console
fn init_logging() {
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;
    use std::sync::Mutex;

    // Create log file that persists for the lifetime of the program
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("wst-daemon.log")
        .expect("Failed to open log file");

    // Set up subscriber with both console and file output
    let subscriber = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stdout)
                .with_target(false)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_ansi(false)
        )
        .with(
            fmt::layer()
                .with_writer(Mutex::new(log_file))
                .with_target(false)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_ansi(false)
        );

    subscriber.init();
}

/// Print usage information
fn print_usage() {
    println!("WST Daemon - Windows Subsystem for TTY Daemon");
    println!();
    println!("Usage: wst-daemon [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --daemon        Run as daemon (default)");
    println!("  --stop          Stop running daemon");
    println!("  --status        Show daemon status");
    println!("  --install       Install as Windows service (future)");
    println!("  --uninstall     Uninstall Windows service (future)");
    println!("  --version, -v   Show version information");
    println!("  --help, -h      Show this help message");
    println!();
    println!("The daemon will:");
    println!("  - Register global hotkey (Ctrl+Alt+F12) for WST");
    println!("  - Manage session persistence");
    println!("  - Keep backend processes alive");
    println!("  - Communicate with frontend via IPC");
}
