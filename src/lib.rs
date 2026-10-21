//! Discord gateway to AMQP bridge with a Redis state cache.
//!
//! [`app::run`] runs the service with a [`config::Config`] read from the
//! environment. The shards of this process ([`gateway`]) receive Discord's
//! events, which update the cache ([`cache`]) and are published to the broker
//! ([`amqp`]); gateway commands from the broker are sent to their shard
//! ([`commands`]); [`metrics`] serves the Prometheus metrics and the health
//! check. The README describes the data formats and operations.

#![forbid(unsafe_code)]
// Lints for the service code. Test code (the unit test modules and tests/)
// keeps the default set.
#![cfg_attr(
    not(test),
    warn(
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
    )
)]
#![allow(
    clippy::must_use_candidate,
    reason = "callers use the results; #[must_use] on every pure function is noise"
)]

pub mod amqp;
pub mod app;
pub mod cache;
pub mod commands;
pub mod config;
pub mod constants;
pub mod discord_log;
pub mod gateway;
pub mod metrics;
mod model_patches;
pub mod models;
pub mod redis_conn;
pub mod util;
