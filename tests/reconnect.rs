#![recursion_limit = "256"]

//! The "broken pipe" regression: after Redis drops our connection, commands
//! must work again; and a slow reply is awaited, not timed out (set
//! TEST_REDIS_URL).
//!
//! With redis 0.22's MultiplexedConnection every command after a dropped
//! connection failed with `broken pipe` forever, which flooded the logs.

mod common;

use std::time::Duration;

/// CLIENT KILL hits every connection of the server, so these run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn kill_clients(conn: &mut impl redis::aio::ConnectionLike) {
    // Kill every other normal client (our manager's connection included);
    // SKIPME yes keeps the connection that sends the command alive.
    let _: () = redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("normal")
        .arg("SKIPME")
        .arg("yes")
        .query_async(conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn connection_recovers_after_redis_drops_it() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = common::redis(3).await else {
        return;
    };
    let base = std::env::var("TEST_REDIS_URL").unwrap();
    let mut killer = redis::Client::open(format!("{}/3", base.trim_end_matches('/')))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();

    for round in 0..3 {
        let _: () = redis::cmd("SET")
            .arg("k")
            .arg(round)
            .query_async(&mut conn)
            .await
            .unwrap();
        kill_clients(&mut killer).await;

        // The first command may still see the dead socket; within a few
        // attempts the manager must have reconnected.
        let mut recovered = false;
        for _ in 0..50 {
            let result: redis::RedisResult<i64> =
                redis::cmd("GET").arg("k").query_async(&mut conn).await;
            if let Ok(value) = result {
                assert_eq!(value, round);
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            recovered,
            "round {round}: still failing after Redis dropped the connection"
        );
    }
}

/// Control: the kill really drops a connection, so the recovery above is not
/// a kill that missed. A plain multiplexed connection (what the old code
/// used) fails the next command. With redis 0.22 it then stayed broken
/// forever, which was the flood seen in production; whether a later redis-rs
/// reconnects it on its own is not checked, since the service does not rely
/// on that (it uses a ConnectionManager).
#[tokio::test]
async fn the_kill_breaks_a_plain_multiplexed_connection() {
    let _serial = SERIAL.lock().await;
    let Some(_) = common::redis(4).await else {
        return;
    };
    let base = std::env::var("TEST_REDIS_URL").unwrap();
    let client = redis::Client::open(format!("{}/4", base.trim_end_matches('/'))).unwrap();
    let mut plain = client.get_multiplexed_async_connection().await.unwrap();
    let mut killer = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::cmd("SET")
        .arg("k")
        .arg(1)
        .query_async(&mut plain)
        .await
        .unwrap();

    kill_clients(&mut killer).await;
    let result: redis::RedisResult<i64> = redis::cmd("GET").arg("k").query_async(&mut plain).await;
    let err = result.expect_err("the first command after the kill fails");
    assert!(err.is_connection_dropped() || err.is_io_error(), "{err}");
}

/// A large GUILD_CREATE transaction can keep Redis from answering for longer
/// than redis-rs' default response timeout (500 ms), which would fail the
/// cache update although Redis goes on to apply it. The connection waits up
/// to `RESPONSE_TIMEOUT` instead.
#[tokio::test]
async fn a_reply_slower_than_redis_rs_default_timeout_is_awaited() {
    // A CLIENT KILL from the other tests would end the pause early.
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = common::redis(5).await else {
        return;
    };
    let base = std::env::var("TEST_REDIS_URL").unwrap();
    let mut pauser = redis::Client::open(format!("{}/5", base.trim_end_matches('/')))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    // Redis holds back every client's commands (and so the replies) for 2 s.
    let _: () = redis::cmd("CLIENT")
        .arg("PAUSE")
        .arg(2000)
        .arg("ALL")
        .query_async(&mut pauser)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let result: redis::RedisResult<()> = redis::cmd("SET")
        .arg("k")
        .arg("v")
        .query_async(&mut conn)
        .await;
    let waited = started.elapsed();
    result.expect("the slow reply is awaited");
    // The margin is for a loaded host that starts the clock late.
    assert!(
        waited > Duration::from_millis(500),
        "not held back for longer than redis-rs' default: {waited:?}"
    );
    assert!(waited < twilight_dispatch::redis_conn::RESPONSE_TIMEOUT);
    let value: String = redis::cmd("GET")
        .arg("k")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(value, "v");
}
