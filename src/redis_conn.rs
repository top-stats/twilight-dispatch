//! Redis connection setup.
//!
//! The "broken pipe" flood came from redis 0.22's `MultiplexedConnection`,
//! which never reconnects: once its socket died, every later command failed
//! immediately with a synthesized `BrokenPipe`. A `ConnectionManager`
//! reconnects in the background instead. Its sockets use TCP keepalive (first
//! probe after 30 s idle, then every 10 s) and, on Linux, `TCP_USER_TIMEOUT`
//! (20 s), so a socket that was silently dropped (for example by the Swarm
//! overlay network) fails after about 20 s with a command in flight and about
//! 40 s when idle (about 60 s idle elsewhere), instead of after the kernel's
//! retransmission timeout or never. A command that gets no reply fails after
//! [`RESPONSE_TIMEOUT`].

use crate::models::ApiResult;

use redis::{
    aio::{ConnectionManager, ConnectionManagerConfig},
    io::tcp::{socket2::TcpKeepalive, TcpSettings},
    Client, IntoConnectionInfo,
};
use std::time::Duration;

/// The Redis connection the service uses, made by [`connect`]. Clones share
/// one connection, which reconnects in the background.
pub type Redis = ConnectionManager;

/// Per-command response timeout. Large `GUILD_CREATE` transactions need more
/// than redis-rs' 500 ms default.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A client for `url` whose sockets use TCP keepalive and, on Linux,
/// `TCP_USER_TIMEOUT` (see the module docs).
fn client(url: &str) -> ApiResult<Client> {
    let info = url.into_connection_info()?;
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    let tcp = TcpSettings::default()
        .set_nodelay(true)
        .set_keepalive(keepalive);
    #[cfg(target_os = "linux")]
    let tcp = tcp.set_user_timeout(Duration::from_secs(20));
    Ok(Client::open(info.set_tcp_settings(tcp))?)
}

/// The connection manager's settings: reconnects with exponential backoff
/// (between 100 ms and 5 s), a 3 s connection timeout, and
/// [`RESPONSE_TIMEOUT`] per command.
fn manager_config() -> ConnectionManagerConfig {
    ConnectionManagerConfig::new()
        .set_number_of_retries(5)
        .set_exponent_base(2.0)
        .set_min_delay(Duration::from_millis(100))
        .set_max_delay(Duration::from_secs(5))
        .set_connection_timeout(Some(Duration::from_secs(3)))
        .set_response_timeout(Some(RESPONSE_TIMEOUT))
}

/// Connect (retrying with backoff) and return a reconnecting connection.
///
/// # Errors
///
/// Fails when `url` is not a valid Redis URL, or when no connection could be
/// made within the retries.
pub async fn connect(url: &str) -> ApiResult<Redis> {
    Ok(ConnectionManager::new_with_config(client(url)?, manager_config()).await?)
}
