//! The twilight-dispatch service (see the README), and `twilight-dispatch
//! healthcheck`, the Docker HEALTHCHECK command.

#![forbid(unsafe_code)]
#![warn(
    missing_docs,
    rust_2018_idioms,
    clippy::pedantic,
    clippy::allow_attributes_without_reason,
    clippy::dbg_macro,
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::todo,
    clippy::unimplemented,
    clippy::unwrap_used
)]
#![allow(
    clippy::must_use_candidate,
    reason = "callers use the results; #[must_use] on every pure function is noise"
)]

use std::process::ExitCode;
use tracing::{error, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};
use twilight_dispatch::{app, config::Config, metrics};

fn main() -> ExitCode {
    // Before the runtime starts: dotenvy sets process environment variables.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!("Failed to start the runtime: {}", err);
            return ExitCode::FAILURE;
        }
    };

    // `twilight-dispatch healthcheck` is the Docker HEALTHCHECK command.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let port = std::env::var("PROMETHEUS_PORT")
            .ok()
            .and_then(|port| port.trim().parse().ok())
            .unwrap_or(8005);
        let host = std::env::var("PROMETHEUS_HOST").ok();
        let addresses = metrics::probe_addresses(host.as_deref());
        let healthy = runtime.block_on(async {
            for address in addresses {
                if metrics::probe(address, port).await {
                    return true;
                }
            }
            false
        });
        return if healthy {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    // twilight-gateway and twilight-http use rustls without choosing a crypto
    // provider; without one the first connection panics.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            error!("{}", err);
            return ExitCode::FAILURE;
        }
    };
    for name in Config::obsolete_variables(&|name| std::env::var(name).ok()) {
        warn!("{} is no longer used and can be removed", name);
    }

    let result = runtime.block_on(app::run(config));
    // Don't wait for abandoned blocking work (DNS lookups) when exiting.
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{}", err);
            ExitCode::FAILURE
        }
    }
}
