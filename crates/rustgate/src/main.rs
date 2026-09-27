use std::{path::PathBuf, time::Duration};

use anyhow::Context;
use clap::{Parser, ValueEnum};
use rustgate::{Gateway, config::Config};
use tracing::info;

#[derive(Debug, Parser)]
#[command(version, about = "RustGate API gateway")]
struct Args {
    /// Path to the YAML configuration file.
    #[arg(
        short,
        long,
        env = "RUSTGATE_CONFIG",
        default_value = "config/dev.yaml"
    )]
    config: PathBuf,
    #[arg(long, env = "RUSTGATE_LOG_FORMAT", value_enum, default_value_t = LogFormat::Pretty)]
    log_format: LogFormat,
    /// Seconds to let in-flight requests finish after SIGTERM/Ctrl-C before aborting them.
    #[arg(long, env = "RUSTGATE_DRAIN_TIMEOUT_SECS", default_value_t = 30)]
    drain_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    rustgate::telemetry::init(matches!(args.log_format, LogFormat::Json));

    let config =
        Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    let mut gateway = Gateway::start(&config).await?;
    info!(data = %gateway.data_addr(), admin = %gateway.admin_addr(), "rustgate listening");

    tokio::select! {
        () = gateway.wait() => anyhow::bail!("a gateway task exited unexpectedly"),
        () = shutdown_signal() => info!("shutdown signal received, draining"),
    }
    gateway
        .shutdown(Duration::from_secs(args.drain_timeout_secs))
        .await?;
    info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
