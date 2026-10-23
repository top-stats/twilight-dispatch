//! The twilight-dispatch service (see the README), and `twilight-dispatch
//! healthcheck`, the Docker HEALTHCHECK command.
//!
//! Exit codes: 0 after a clean shutdown or a healthy probe, 1 on a failure or
//! an unhealthy probe, 2 on an unknown argument.

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

use std::{io::IsTerminal, process::ExitCode, time::Duration};
use tokio::runtime::{Builder, Runtime};
use tracing::{error, warn};
use tracing_subscriber::{filter::LevelFilter, EnvFilter};
use twilight_dispatch::{
    app,
    config::{self, Config},
    metrics,
    util::ErrorChain,
};

/// The arguments the binary accepts, for the error on any other.
const USAGE: &str = "twilight-dispatch [healthcheck]";

fn main() -> ExitCode {
    // Before the runtime starts: dotenvy sets process environment variables.
    // Before tracing too, as RUST_LOG may come from `.env`.
    let dotenv = dotenvy::dotenv();

    // No colour codes in container logs (no terminal there), and none with
    // NO_COLOR set, which tracing-subscriber's default honours.
    let no_color = std::env::var_os("NO_COLOR").unwrap_or_default();
    let ansi = std::io::stdout().is_terminal() && no_color.is_empty();
    tracing_subscriber::fmt()
        .with_ansi(ansi)
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    // dotenvy applies the lines before the first one it cannot parse and no
    // others, which must not go unnoticed.
    match dotenv {
        // Without the line: it may hold a token or a password.
        Err(dotenvy::Error::LineParse(..)) => {
            warn!("Failed to load .env: cannot parse a line; it and the rest were skipped");
        }
        Err(err) if !err.not_found() => warn!("Failed to load .env: {}", err),
        _ => {}
    }

    match std::env::args_os().nth(1) {
        None => serve(),
        Some(argument) if argument.to_str() == Some("healthcheck") => healthcheck(),
        Some(argument) => {
            error!(
                "unknown argument {}; usage: {}",
                argument.to_string_lossy(),
                USAGE
            );
            ExitCode::from(2)
        }
    }
}

/// Runs the service until SIGTERM or SIGINT, or until it fails to start.
fn serve() -> ExitCode {
    let Some(runtime) = build_runtime(&mut Builder::new_multi_thread()) else {
        return ExitCode::FAILURE;
    };

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
    if let Some(warning) = Config::member_request_warning(&|name| std::env::var(name).ok()) {
        warn!("{}", warning);
    }

    let result = runtime.block_on(app::run(config));
    // Don't wait for abandoned blocking work (DNS lookups) when exiting.
    runtime.shutdown_timeout(Duration::from_millis(500));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{}", ErrorChain(&err));
            ExitCode::FAILURE
        }
    }
}

/// `twilight-dispatch healthcheck`: whether the service in this container
/// answers `GET /healthcheck` with 200. Reads only `PROMETHEUS_HOST` and
/// `PROMETHEUS_PORT`, parsed as the service parses them, so it needs no other
/// configuration.
fn healthcheck() -> ExitCode {
    // At most two probes, one after the other: no worker threads needed.
    let Some(runtime) = build_runtime(&mut Builder::new_current_thread()) else {
        return ExitCode::FAILURE;
    };
    let port = std::env::var("PROMETHEUS_PORT")
        .ok()
        .and_then(|port| config::parse_value::<u16>(&port))
        .unwrap_or(config::DEFAULT_PROMETHEUS_PORT);
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
    if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The runtime `builder` makes, with IO and timers, or `None` (logged) when
/// it cannot start.
fn build_runtime(builder: &mut Builder) -> Option<Runtime> {
    match builder.enable_all().build() {
        Ok(runtime) => Some(runtime),
        Err(err) => {
            error!("Failed to start the runtime: {}", err);
            None
        }
    }
}
