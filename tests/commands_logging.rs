//! What the log lines say about failures: dropped gateway commands are
//! warned about at most once per 5 s and name what was wrong, and errors are
//! logged with the chain of their causes (a failed Discord log post, a
//! message that could not be posted, an `ApiError`). Needs no services.

use std::{
    error::Error,
    fmt,
    sync::{Arc, Once},
    time::{Duration, Instant},
};
use twilight_dispatch::{
    commands::{parse_message, plan, post, InvalidCommand, Plan},
    discord_log::{log_embed, DiscordLog},
    metrics::GATEWAY_COMMANDS,
    models::ApiError,
    util::ErrorChain,
};
use twilight_http::Client;
use twilight_model::id::Id;

/// The warnings this test binary logged.
static WARNINGS: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

struct WarningWriter;

impl std::io::Write for WarningWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        WARNINGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Collect warnings in [`WARNINGS`]; returns how many bytes there already
/// are, so a test can read only what was logged after. The tests run in
/// parallel: `Once` makes each wait until the subscriber is installed, so no
/// warning is logged before it.
fn capture_warnings() -> usize {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(|| WarningWriter)
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .try_init();
    });
    WARNINGS.lock().unwrap().len()
}

fn warnings_since(start: usize) -> String {
    String::from_utf8_lossy(&WARNINGS.lock().unwrap()[start..]).into_owned()
}

// ---------------------------------------------------------------------------
// Dropped gateway commands

/// A producer stuck on a bad command must not flood the log: the three
/// warnings about dropped commands (malformed, for a shard the bot does not
/// have, rejected) share one throttle that lets a line through every 5 s and
/// then says how many it suppressed. Every command is still dropped. No
/// other test of this binary drops commands, so the throttle starts fresh.
#[test]
fn dropped_commands_are_warned_about_once_per_window() {
    let logged = capture_warnings();
    let bodies: [&[u8]; 3] = [
        b"junk",
        br#"{"op":1,"shard":7}"#,
        br#"{"op":0,"shard":0,"data":{"op":2}}"#,
    ];
    for _ in 0..100 {
        for body in bodies {
            assert_eq!(plan(body, |_| true, 1), Plan::Drop);
        }
    }
    let warnings = warnings_since(logged);
    let lines = warnings.matches("Dropping gateway command").count();
    // A second line only if the loop took longer than the window.
    assert!(
        (1..=2).contains(&lines),
        "{lines} lines for 300 dropped commands:\n{warnings}"
    );
    assert!(
        warnings.contains("Dropping gateway command: malformed delivery: "),
        "{warnings}"
    );
    assert!(warnings.contains("(0 similar suppressed)"), "{warnings}");

    // Once the window is over the next one is logged, with the count.
    std::thread::sleep(Duration::from_millis(5_200));
    let logged = capture_warnings();
    assert_eq!(plan(br#"{"op":1,"shard":7}"#, |_| true, 1), Plan::Drop);
    let warnings = warnings_since(logged);
    assert!(
        warnings.contains("Dropping gateway command for invalid shard 7 ("),
        "{warnings}"
    );
    if lines == 1 {
        assert!(warnings.contains("(299 similar suppressed)"), "{warnings}");
    }
}

/// Invalid messages (op 2) are warned about like invalid commands, at most
/// once per 5 s and with what was wrong, on a throttle of their own. No other
/// test of this binary drops messages, so the throttle starts fresh.
#[test]
fn dropped_messages_are_warned_about_once_per_window() {
    let logged = capture_warnings();
    let blank = br#"{"op":2,"data":{"channel_id":"1","content":" "}}"#;
    for _ in 0..100 {
        assert_eq!(plan(blank, |_| true, 1), Plan::Drop);
    }
    let warnings = warnings_since(logged);
    let lines = warnings.matches("Dropping message delivery: ").count();
    assert!((1..=2).contains(&lines), "{lines} lines:\n{warnings}");
    let reason = "message has neither content nor embeds (0 similar suppressed)";
    assert!(warnings.contains(reason), "{warnings}");
}

/// The reason of a rejected command is an error of its own, and an opcode
/// that may not be sent is named as a number, not as an `Option`.
#[test]
fn rejected_commands_are_errors_with_a_readable_reason() {
    let rejected = InvalidCommand::OpcodeNotAllowed(Some(2));
    let reason: Box<dyn Error + Send + Sync> = Box::new(rejected);
    assert_eq!(reason.to_string(), "gateway opcode 2 may not be sent");
    assert_eq!(
        InvalidCommand::OpcodeNotAllowed(None).to_string(),
        "data has no unsigned integer op"
    );
}

// ---------------------------------------------------------------------------
// Error chains

const SHARD_LOG: u64 = 11;

/// twilight's `Display` of a failed request leaves its cause out, so a
/// failed post is logged with the error's sources: here why the connection
/// was refused.
#[tokio::test]
async fn failed_discord_log_posts_are_logged_with_their_cause() {
    let logged = capture_warnings();
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Nothing listens there once the listener is dropped.
    let refused = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let client = Arc::new(
        Client::builder()
            .token("Bot test".to_owned())
            .proxy(refused.to_string(), true)
            .ratelimiter(None)
            .build(),
    );

    // The same request, to know what twilight reports for it.
    let embeds = [log_embed(0, Some("probe".into()), None, None)];
    let Err(err) = client
        .create_message(Id::new(SHARD_LOG))
        .embeds(&embeds)
        .await
    else {
        panic!("a request to a closed port succeeded");
    };
    let chain = ErrorChain(&err).to_string();
    assert_ne!(chain, err.to_string(), "the error has a source");

    let log = DiscordLog::new(client, SHARD_LOG, 0);
    log.shard(0, "refused");
    let line = format!("Failed to post message to Discord: {chain}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut warnings = warnings_since(logged);
    while !warnings.contains(&line) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        warnings = warnings_since(logged);
    }
    assert!(warnings.contains(&line), "no {line:?} in:\n{warnings}");
}

fn failed_posts() -> u64 {
    GATEWAY_COMMANDS.with_label_values(&["", "failed"]).get()
}

/// A message that cannot be posted (here Discord cannot be reached) is
/// counted as failed and not retried. The failure is logged with its cause,
/// at most once per 5 s. No other test of this binary posts messages, so the
/// throttle starts fresh.
#[tokio::test]
async fn failed_posts_are_counted_and_warned_about_once_per_window() {
    let logged = capture_warnings();
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Nothing listens there once the listener is dropped.
    let refused = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let client = Client::builder()
        .token("Bot test".to_owned())
        .proxy(refused.to_string(), true)
        .ratelimiter(None)
        .build();

    // The same request, to know what twilight reports for it.
    let Err(err) = client.create_message(Id::new(42)).content("hi").await else {
        panic!("a request to a closed port succeeded");
    };
    let chain = ErrorChain(&err).to_string();

    let message = parse_message(br#"{"op":2,"data":{"channel_id":"42","content":"hi"}}"#);
    let message = message.unwrap();
    let failed = failed_posts();
    post(&client, &message).await;
    post(&client, &message).await;
    assert_eq!(failed_posts(), failed + 2);

    let warnings = warnings_since(logged);
    let lines = warnings
        .matches("Failed to post a message to channel 42: ")
        .count();
    assert_eq!(lines, 1, "{warnings}");
    let line = format!("channel 42: {chain} (0 similar suppressed)");
    assert!(warnings.contains(&line), "no {line:?} in:\n{warnings}");
}

#[derive(Debug)]
struct Leaf;

impl fmt::Display for Leaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("leaf")
    }
}

impl Error for Leaf {}

#[derive(Debug)]
struct Middle(Leaf);

impl fmt::Display for Middle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("middle")
    }
}

impl Error for Middle {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// An `ApiError` shows the wrapped error's message, and its source is that
/// error's own source, so an error chain shows every cause once.
#[test]
fn api_errors_chain_the_causes_of_the_wrapped_error() {
    let err = ApiError::from(std::io::Error::other(Middle(Leaf)));
    assert_eq!(err.to_string(), "Io: middle");
    assert_eq!(
        err.source().map(ToString::to_string).as_deref(),
        Some("leaf")
    );
    assert_eq!(ErrorChain(&err).to_string(), "Io: middle: leaf");

    // Without a cause the message is not repeated.
    let plain = ApiError::from(std::io::Error::other("disk full"));
    assert_eq!(ErrorChain(&plain).to_string(), "Io: disk full");

    // A message has no source.
    assert!(ApiError::Other("x".into()).source().is_none());
    assert_eq!(ErrorChain(&ApiError::from("x")).to_string(), "Other: x");
    let message = ApiError::from(String::from("y"));
    assert!(matches!(message, ApiError::Other(_)), "{message:?}");
    assert_eq!(message.to_string(), "Other: y");
}
