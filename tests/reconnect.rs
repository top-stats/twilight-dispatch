#![recursion_limit = "256"]

//! The "broken pipe" regression: after Redis drops our connection, commands
//! must work again (set TEST_REDIS_URL).
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

/// Control: a plain multiplexed connection (what the old code used) stays
/// broken after the same kill, which is the flood seen in production.
#[tokio::test]
async fn plain_multiplexed_connection_stays_broken() {
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
    tokio::time::sleep(Duration::from_millis(200)).await;

    for _ in 0..5 {
        let result: redis::RedisResult<i64> =
            redis::cmd("GET").arg("k").query_async(&mut plain).await;
        let err = result.expect_err("a dead multiplexed connection never recovers");
        assert!(err.is_connection_dropped() || err.is_io_error(), "{err}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
