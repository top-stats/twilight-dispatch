//! A fake Discord (REST API and gateway) for end-to-end tests of the whole
//! service.
//!
//! Include it from a test file in `tests/` (it is not a test target itself):
//!
//! ```ignore
//! #[path = "support/fake_discord.rs"]
//! mod fake_discord;
//! use fake_discord::{FakeDiscord, Frame};
//! ```
//!
//! It only uses crates the package already depends on (hyper 1, hyper-util,
//! http-body-util, bytes, tokio, tokio-websockets with `server`, futures-util,
//! serde_json) and is independent of `tests/common`.
//!
//! # Starting
//!
//! [`FakeDiscord::start`] (or [`FakeDiscord::with_options`]) binds a REST
//! server and a gateway server on 127.0.0.1 with ephemeral ports. Point the
//! service at them with [`FakeDiscord::env`] (`DISCORD_API_PROXY` and
//! `DISCORD_GATEWAY_URL`). Both servers live as long as the test's runtime.
//!
//! # REST API: `fake.api` ([`FakeApi`])
//!
//! Paths are matched and recorded without the `/api/v10` prefix.
//! - `GET /users/@me`: the bot's `CurrentUser` ([`Options::bot_id`]).
//! - `GET /gateway/bot`: `{url, shards, session_start_limit}` from [`Options`].
//! - `POST /channels/{id}/messages`: recorded; answers 200 with the message, or
//!   the status set with [`FakeApi::set_channel_status`] and Discord's JSON
//!   error body (403 `{"code":50013,...}`, 404 `{"code":10003,...}`, ...).
//! - Anything else: 404. [`FakeApi::set_response`] overrides any route (for
//!   example a 401 for `GET /users/@me`), [`FakeApi::set_stall`] makes every
//!   request hang forever (an unresponsive Discord).
//! - [`FakeApi::requests`], [`FakeApi::posts`], [`FakeApi::wait_posts`] and
//!   [`FakeApi::wait_until`] inspect what arrived ([`ApiRequest`]).
//!
//! # Gateway: `fake.gateway` ([`FakeGateway`])
//!
//! A plain `ws://` server. twilight connects to
//! `<DISCORD_GATEWAY_URL>/?v=10&encoding=json&compress=zstd-stream`; the fake
//! only ever sends uncompressed text frames, which twilight accepts. READY
//! payloads built here use [`FakeGateway::resume_url`]
//! (`ws://127.0.0.1:<port>/resume`), so a resume shows up as a connection whose
//! [`GatewayConnection::path`] is `/resume/`.
//!
//! [`FakeGateway::accept`] returns the next connection ([`GatewayConnection`]),
//! which the test then scripts:
//! - server side: `hello`, `ready`, `dispatch` / `dispatch_raw`, `resumed`,
//!   `reconnect` (op 7), `invalid_session` (op 9), `request_heartbeat` (op 1),
//!   `close(code, reason)`, `drop_connection` (TCP drop without a close frame).
//!   Every send returns once the frame was written to the socket and panics
//!   if the connection is already closed.
//! - client side: every frame the client sends is recorded as a [`Frame`]
//!   (JSON text, binary, close with its code, `Gone` when the connection
//!   ended). Read them in order with `next_frame`, `expect_op`,
//!   `expect_identify`, `expect_resume`, `expect_close`, `expect_gone` or
//!   `wait_for`, or get all of them with `frames`. The `expect_*` helpers skip
//!   heartbeats and panic on any other unexpected frame.
//! - heartbeats (op 1) are answered with an ACK (op 11) automatically; turn
//!   that off with `set_auto_ack(false)` to simulate a zombied connection.
//!
//! Every wait has a bound ([`TIMEOUT`] unless the method takes one) and panics
//! with what it saw instead of hanging the test.

#![allow(dead_code)]

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{
    body::Incoming,
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    server::conn::http1,
    service::service_fn,
    Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, Notify},
};
use tokio_websockets::{CloseCode, Message, ServerBuilder, WebSocketStream};

/// Default bound for every wait. Only a failing test waits this long, so it
/// is generous: a loaded machine can take over 10 s from starting the
/// service to its first gateway connection (identify queue, broker connect).
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// What the fake REST API reports about the bot.
#[derive(Clone, Debug)]
pub struct Options {
    pub bot_id: u64,
    pub bot_name: String,
    /// Recommended shard count (`GET /gateway/bot`).
    pub shards: u32,
    pub session_start_limit: SessionStartLimit,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionStartLimit {
    pub total: u32,
    pub remaining: u32,
    /// Milliseconds.
    pub reset_after: u64,
    pub max_concurrency: u16,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            bot_id: 900,
            bot_name: "e2e-bot".into(),
            shards: 1,
            session_start_limit: SessionStartLimit {
                total: 1000,
                remaining: 1000,
                reset_after: 86_400_000,
                max_concurrency: 1,
            },
        }
    }
}

/// The REST API and the gateway.
pub struct FakeDiscord {
    pub api: FakeApi,
    pub gateway: FakeGateway,
}

impl FakeDiscord {
    pub async fn start() -> Self {
        Self::with_options(Options::default()).await
    }

    pub async fn with_options(options: Options) -> Self {
        let gateway = FakeGateway::start(options.bot_id).await;
        let api = FakeApi::start(&options, &gateway.url()).await;
        Self { api, gateway }
    }

    /// `DISCORD_API_PROXY` and `DISCORD_GATEWAY_URL` for the service.
    pub fn env(&self) -> [(&'static str, String); 2] {
        [
            ("DISCORD_API_PROXY", self.api.proxy()),
            ("DISCORD_GATEWAY_URL", self.gateway.url()),
        ]
    }
}

/// The `d` of a READY that twilight-model parses.
pub fn ready_payload(
    bot_id: u64,
    session_id: &str,
    resume_url: &str,
    shard: [u32; 2],
    guilds: &[u64],
) -> Value {
    json!({
        "v": 10,
        "user": {
            "id": bot_id.to_string(), "username": "e2e-bot", "discriminator": "0",
            "global_name": null, "avatar": null, "bot": true, "mfa_enabled": false,
            "verified": true, "flags": 0,
        },
        "guilds": guilds
            .iter()
            .map(|id| json!({"id": id.to_string(), "unavailable": true}))
            .collect::<Vec<_>>(),
        "session_id": session_id,
        "resume_gateway_url": resume_url,
        "shard": shard,
        "application": {"id": bot_id.to_string(), "flags": 0},
    })
}

// ---------------------------------------------------------------------------
// REST API

/// One request the fake REST API received.
#[derive(Clone, Debug)]
pub struct ApiRequest {
    pub method: String,
    /// Without the `/api/v10` prefix, e.g. `/users/@me`.
    pub path: String,
    pub authorization: Option<String>,
    /// The JSON body (`Null` when empty, a string when it is not JSON).
    pub body: Value,
    /// The status the fake answered with.
    pub status: u16,
}

#[derive(Default)]
struct ApiState {
    /// "METHOD /path" -> (status, body).
    responses: HashMap<String, (u16, Value)>,
    channel_status: HashMap<u64, u16>,
    requests: Vec<ApiRequest>,
    stall: bool,
    messages: u64,
}

impl ApiState {
    fn respond(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        if let Some((status, body)) = self.responses.get(&format!("{method} {path}")) {
            return (*status, body.clone());
        }
        let channel = path
            .strip_prefix("/channels/")
            .and_then(|rest| rest.strip_suffix("/messages"))
            .and_then(|id| id.parse::<u64>().ok());
        if let (Some(channel), "POST") = (channel, method) {
            let status = self.channel_status.get(&channel).copied().unwrap_or(200);
            if !(200..300).contains(&status) {
                return (status, error_body(status));
            }
            self.messages += 1;
            let mut message = json!({
                "id": (1_000_000 + self.messages).to_string(),
                "channel_id": channel.to_string(),
                "content": "",
                "embeds": [],
            });
            if let (Value::Object(message), Value::Object(body)) = (&mut message, body) {
                message.extend(body.clone());
            }
            return (status, message);
        }
        (404, json!({"message": "404: Not Found", "code": 0}))
    }
}

/// Discord's JSON error bodies (twilight-http only reports the status of an
/// error response whose body parses).
pub fn error_body(status: u16) -> Value {
    match status {
        401 => json!({"message": "401: Unauthorized", "code": 0}),
        403 => json!({"message": "Missing Permissions", "code": 50013}),
        404 => json!({"message": "Unknown Channel", "code": 10003}),
        _ => json!({"message": format!("{status}: error"), "code": 0}),
    }
}

#[derive(Clone)]
pub struct FakeApi {
    addr: SocketAddr,
    state: Arc<Mutex<ApiState>>,
    changed: Arc<Notify>,
}

impl FakeApi {
    async fn start(options: &Options, gateway_url: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut state = ApiState::default();
        state.responses.insert(
            "GET /users/@me".into(),
            (
                200,
                json!({
                    "id": options.bot_id.to_string(), "username": options.bot_name,
                    "discriminator": "0", "global_name": null, "avatar": null,
                    "banner": null, "accent_color": null, "bot": true,
                    "mfa_enabled": false, "verified": true, "locale": "en-US",
                    "flags": 0, "public_flags": 0,
                }),
            ),
        );
        let limit = options.session_start_limit;
        state.responses.insert(
            "GET /gateway/bot".into(),
            (
                200,
                json!({
                    "url": gateway_url,
                    "shards": options.shards,
                    "session_start_limit": {
                        "total": limit.total,
                        "remaining": limit.remaining,
                        "reset_after": limit.reset_after,
                        "max_concurrency": limit.max_concurrency,
                    },
                }),
            ),
        );
        let api = Self {
            addr: listener.local_addr().unwrap(),
            state: Arc::new(Mutex::new(state)),
            changed: Arc::new(Notify::new()),
        };
        tokio::spawn(serve_api(listener, api.clone()));
        api
    }

    /// `http://127.0.0.1:<port>`, the value for `DISCORD_API_PROXY`.
    pub fn proxy(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Answer `method path` (without `/api/v10`) with this status and body.
    pub fn set_response(&self, method: &str, path: &str, status: u16, body: Value) {
        self.state
            .lock()
            .unwrap()
            .responses
            .insert(format!("{method} {path}"), (status, body));
    }

    /// Answer messages posted to `channel` with this status (200 by default).
    pub fn set_channel_status(&self, channel: u64, status: u16) {
        self.state
            .lock()
            .unwrap()
            .channel_status
            .insert(channel, status);
    }

    /// Record requests but never answer them.
    pub fn set_stall(&self, stall: bool) {
        self.state.lock().unwrap().stall = stall;
    }

    pub fn requests(&self) -> Vec<ApiRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Bodies of every message posted to `channel`, answered or rejected.
    pub fn posts(&self, channel: u64) -> Vec<Value> {
        posts_to(&self.state.lock().unwrap().requests, channel)
    }

    /// Wait until at least `count` messages were posted to `channel`.
    pub async fn wait_posts(&self, channel: u64, count: usize) -> Vec<Value> {
        self.wait_until(&format!("{count} posts to channel {channel}"), |requests| {
            let posts = posts_to(requests, channel);
            (posts.len() >= count).then_some(posts)
        })
        .await
    }

    /// Wait until `check` returns something for the requests received so far.
    pub async fn wait_until<T>(
        &self,
        what: &str,
        mut check: impl FnMut(&[ApiRequest]) -> Option<T>,
    ) -> T {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            // Registered before checking, so no notification is missed.
            let changed = self.changed.notified();
            if let Some(value) = check(&self.state.lock().unwrap().requests) {
                return value;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                panic!(
                    "fake Discord API: timed out waiting for {what}; requests: {:#?}",
                    self.requests()
                );
            }
        }
    }
}

fn posts_to(requests: &[ApiRequest], channel: u64) -> Vec<Value> {
    let path = format!("/channels/{channel}/messages");
    requests
        .iter()
        .filter(|request| request.method == "POST" && request.path == path)
        .map(|request| request.body.clone())
        .collect()
}

async fn serve_api(listener: TcpListener, api: FakeApi) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let api = api.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let api = api.clone();
                async move { Ok::<_, Infallible>(handle(api, request).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn handle(api: FakeApi, request: Request<Incoming>) -> Response<Full<Bytes>> {
    let method = request.method().as_str().to_owned();
    let full_path = request.uri().path().to_owned();
    let path = full_path
        .strip_prefix("/api/v10")
        .unwrap_or(&full_path)
        .to_owned();
    let authorization = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = match request.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => Bytes::new(),
    };
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };

    let (status, response, stall) = {
        let mut state = api.state.lock().unwrap();
        let (status, response) = state.respond(&method, &path, &body);
        state.requests.push(ApiRequest {
            method,
            path,
            authorization,
            body,
            status,
        });
        (status, response, state.stall)
    };
    api.changed.notify_waiters();
    if stall {
        std::future::pending::<()>().await;
    }

    let mut reply = Response::new(Full::new(Bytes::from(response.to_string())));
    *reply.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::IM_A_TEAPOT);
    reply
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    reply
}

// ---------------------------------------------------------------------------
// Gateway

/// A frame the client sent (or the end of the connection).
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    /// A text frame, parsed as JSON (a JSON string when it was not JSON).
    Json(Value),
    Binary(Vec<u8>),
    /// A close frame (`None`: without a status code).
    Close {
        code: Option<u16>,
        reason: String,
    },
    /// The connection ended; nothing follows.
    Gone,
}

impl Frame {
    pub fn json(&self) -> Option<&Value> {
        match self {
            Self::Json(value) => Some(value),
            _ => None,
        }
    }

    /// The gateway opcode of a JSON frame.
    pub fn op(&self) -> Option<u64> {
        self.json()?.get("op")?.as_u64()
    }

    /// The `d` of a JSON frame.
    pub fn d(&self) -> Option<&Value> {
        self.json()?.get("d")
    }

    pub fn is_heartbeat(&self) -> bool {
        self.op() == Some(1)
    }
}

pub struct FakeGateway {
    addr: SocketAddr,
    accepted: tokio::sync::Mutex<mpsc::UnboundedReceiver<GatewayConnection>>,
    count: Arc<AtomicUsize>,
}

impl FakeGateway {
    async fn start(bot_id: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let count = Arc::new(AtomicUsize::new(0));
        tokio::spawn(serve_gateway(
            listener,
            tx,
            count.clone(),
            bot_id,
            format!("ws://{addr}/resume"),
        ));
        Self {
            addr,
            accepted: tokio::sync::Mutex::new(rx),
            count,
        }
    }

    /// `ws://127.0.0.1:<port>`, the value for `DISCORD_GATEWAY_URL`.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// The `resume_gateway_url` of READYs built by [`GatewayConnection::ready`].
    pub fn resume_url(&self) -> String {
        format!("ws://{}/resume", self.addr)
    }

    /// How many websocket connections were established so far.
    pub fn connections(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// The next connection; panics after [`TIMEOUT`].
    pub async fn accept(&self) -> GatewayConnection {
        self.try_accept(TIMEOUT).await.unwrap_or_else(|| {
            panic!(
                "fake gateway: no connection within {TIMEOUT:?} ({} so far)",
                self.connections()
            )
        })
    }

    /// The next connection, if one is established within `timeout`.
    pub async fn try_accept(&self, timeout: Duration) -> Option<GatewayConnection> {
        let mut accepted = self.accepted.lock().await;
        tokio::time::timeout(timeout, accepted.recv())
            .await
            .ok()
            .flatten()
    }
}

async fn serve_gateway(
    listener: TcpListener,
    accepted: mpsc::UnboundedSender<GatewayConnection>,
    count: Arc<AtomicUsize>,
    bot_id: u64,
    resume_url: String,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let accepted = accepted.clone();
        let count = count.clone();
        let resume_url = resume_url.clone();
        tokio::spawn(async move {
            let Ok((request, ws)) = ServerBuilder::new().accept(stream).await else {
                return;
            };
            let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
            let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
            let frames = Arc::new(Mutex::new(Vec::new()));
            let auto_ack = Arc::new(AtomicBool::new(true));
            let connection = GatewayConnection {
                index: count.fetch_add(1, Ordering::SeqCst),
                path: request.uri().path().to_owned(),
                query: request.uri().query().unwrap_or_default().to_owned(),
                bot_id,
                resume_url,
                outbound: outbound_tx,
                inbound: inbound_rx,
                frames: frames.clone(),
                auto_ack: auto_ack.clone(),
            };
            if accepted.send(connection).is_ok() {
                drive(ws, outbound_rx, inbound_tx, frames, auto_ack).await;
            }
        });
    }
}

enum Outbound {
    Send(Message, oneshot::Sender<bool>),
    Drop(oneshot::Sender<bool>),
}

/// Owns one websocket: records what the client sends, ACKs heartbeats and
/// sends what the test scripts. Keeps running (echoing close frames, ACKing
/// heartbeats) after the test dropped its [`GatewayConnection`].
async fn drive(
    mut ws: WebSocketStream<TcpStream>,
    mut outbound: mpsc::UnboundedReceiver<Outbound>,
    inbound: mpsc::UnboundedSender<Frame>,
    frames: Arc<Mutex<Vec<Frame>>>,
    auto_ack: Arc<AtomicBool>,
) {
    let record = |frame: Frame| {
        frames.lock().unwrap().push(frame.clone());
        let _ = inbound.send(frame);
    };
    let mut scripted = true;
    loop {
        tokio::select! {
            received = ws.next() => {
                let message = match received {
                    Some(Ok(message)) => message,
                    Some(Err(_)) | None => {
                        record(Frame::Gone);
                        return;
                    }
                };
                if let Some(text) = message.as_text() {
                    let value = serde_json::from_str(text)
                        .unwrap_or_else(|_| Value::String(text.to_owned()));
                    let heartbeat = value.get("op").and_then(Value::as_u64) == Some(1);
                    record(Frame::Json(value));
                    if heartbeat && auto_ack.load(Ordering::SeqCst) {
                        let _ = ws.send(Message::text(r#"{"op":11}"#)).await;
                    }
                } else if let Some((code, reason)) = message.as_close() {
                    // tokio-websockets answers the close frame itself.
                    let code = u16::from(code);
                    record(Frame::Close {
                        code: (code != 1005).then_some(code),
                        reason: reason.to_owned(),
                    });
                } else if message.is_binary() {
                    record(Frame::Binary(message.as_payload().to_vec()));
                }
            }
            command = outbound.recv(), if scripted => match command {
                Some(Outbound::Send(message, done)) => {
                    let _ = done.send(ws.send(message).await.is_ok());
                }
                Some(Outbound::Drop(done)) => {
                    record(Frame::Gone);
                    let _ = done.send(true);
                    return;
                }
                None => scripted = false,
            },
        }
    }
}

/// One client connection to the fake gateway.
pub struct GatewayConnection {
    /// 0 for the first connection to this gateway, then 1, ...
    pub index: usize,
    /// The request path: `/` for `DISCORD_GATEWAY_URL`, `/resume/` for the
    /// resume URL of READYs built here.
    pub path: String,
    /// The query string (`v=10&encoding=json&compress=zstd-stream`).
    pub query: String,
    bot_id: u64,
    resume_url: String,
    outbound: mpsc::UnboundedSender<Outbound>,
    inbound: mpsc::UnboundedReceiver<Frame>,
    frames: Arc<Mutex<Vec<Frame>>>,
    auto_ack: Arc<AtomicBool>,
}

impl GatewayConnection {
    /// Whether the client connected to the resume URL.
    pub fn is_resume(&self) -> bool {
        self.path.starts_with("/resume")
    }

    pub fn resume_url(&self) -> &str {
        &self.resume_url
    }

    /// Send a text frame; returns whether it was written (false once the
    /// connection is closing).
    pub async fn try_send_text(&self, text: impl Into<String>) -> bool {
        let (done, written) = oneshot::channel();
        let message = Message::text(text.into());
        if self.outbound.send(Outbound::Send(message, done)).is_err() {
            return false;
        }
        written.await.unwrap_or(false)
    }

    /// Send a text frame; panics if the connection is closed.
    pub async fn send_text(&self, text: impl Into<String>) {
        let text = text.into();
        assert!(
            self.try_send_text(text.clone()).await,
            "fake gateway connection {} is closed; could not send {text}",
            self.index
        );
    }

    pub async fn send_json(&self, value: &Value) {
        self.send_text(value.to_string()).await;
    }

    /// HELLO (op 10).
    pub async fn hello(&self, heartbeat_interval_ms: u64) {
        self.send_json(&json!({
            "op": 10, "s": null, "t": null,
            "d": {"heartbeat_interval": heartbeat_interval_ms},
        }))
        .await;
    }

    /// A dispatch (op 0) in Discord's field order.
    pub async fn dispatch(&self, kind: &str, seq: u64, d: &Value) {
        self.dispatch_raw(kind, seq, &d.to_string()).await;
    }

    /// A dispatch whose `d` is sent exactly as given.
    pub async fn dispatch_raw(&self, kind: &str, seq: u64, d: &str) {
        self.send_text(format!(r#"{{"t":"{kind}","s":{seq},"op":0,"d":{d}}}"#))
            .await;
    }

    /// READY with this session, resuming at this gateway's resume URL.
    /// Returns its `d`.
    pub async fn ready(
        &self,
        seq: u64,
        session_id: &str,
        shard: [u32; 2],
        guilds: &[u64],
    ) -> Value {
        let d = ready_payload(self.bot_id, session_id, &self.resume_url, shard, guilds);
        self.dispatch("READY", seq, &d).await;
        d
    }

    pub async fn resumed(&self, seq: u64) {
        self.dispatch("RESUMED", seq, &json!({"_trace": ["fake-gateway"]}))
            .await;
    }

    /// RECONNECT (op 7).
    pub async fn reconnect(&self) {
        self.send_json(&json!({"op": 7, "s": null, "t": null, "d": null}))
            .await;
    }

    /// INVALID_SESSION (op 9).
    pub async fn invalid_session(&self, resumable: bool) {
        self.send_json(&json!({"op": 9, "s": null, "t": null, "d": resumable}))
            .await;
    }

    /// Ask the client for a heartbeat (op 1 from the gateway).
    pub async fn request_heartbeat(&self) {
        self.send_json(&json!({"op": 1, "s": null, "t": null, "d": null}))
            .await;
    }

    /// Close the websocket with a close code (e.g. 4009, 4014).
    pub async fn close(&self, code: u16, reason: &str) {
        let code = CloseCode::try_from(code).expect("a sendable close code");
        let (done, written) = oneshot::channel();
        let sent = self
            .outbound
            .send(Outbound::Send(Message::close(Some(code), reason), done))
            .is_ok();
        assert!(
            sent && written.await.unwrap_or(false),
            "fake gateway connection {} is already closed",
            self.index
        );
    }

    /// Drop the TCP connection without a close frame.
    pub async fn drop_connection(&self) {
        let (done, dropped) = oneshot::channel();
        if self.outbound.send(Outbound::Drop(done)).is_ok() {
            let _ = dropped.await;
        }
    }

    /// Whether heartbeats are ACKed (on by default).
    pub fn set_auto_ack(&self, ack: bool) {
        self.auto_ack.store(ack, Ordering::SeqCst);
    }

    /// Every frame received on this connection so far, including the ones
    /// already read with `next_frame` and friends.
    pub fn frames(&self) -> Vec<Frame> {
        self.frames.lock().unwrap().clone()
    }

    /// The next frame, if one arrives within `timeout`. After the end of the
    /// connection this is always [`Frame::Gone`].
    pub async fn try_next_frame(&mut self, timeout: Duration) -> Option<Frame> {
        match tokio::time::timeout(timeout, self.inbound.recv()).await {
            Ok(Some(frame)) => Some(frame),
            Ok(None) => Some(Frame::Gone),
            Err(_) => None,
        }
    }

    /// The next frame; panics after [`TIMEOUT`].
    pub async fn next_frame(&mut self) -> Frame {
        match self.try_next_frame(TIMEOUT).await {
            Some(frame) => frame,
            None => panic!(
                "fake gateway connection {}: nothing received within {TIMEOUT:?}; so far: {:?}",
                self.index,
                self.frames()
            ),
        }
    }

    /// Skip frames until one matches.
    pub async fn wait_for(&mut self, what: &str, matches: impl Fn(&Frame) -> bool) -> Frame {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.try_next_frame(left).await {
                Some(frame) if matches(&frame) => return frame,
                Some(Frame::Gone) => panic!(
                    "fake gateway connection {} ended while waiting for {what}: {:?}",
                    self.index,
                    self.frames()
                ),
                Some(_) => {}
                None => panic!(
                    "fake gateway connection {}: no {what} within {TIMEOUT:?}; so far: {:?}",
                    self.index,
                    self.frames()
                ),
            }
        }
    }

    /// The next frame that is not a heartbeat.
    async fn next_non_heartbeat(&mut self) -> Frame {
        loop {
            let frame = self.next_frame().await;
            if !frame.is_heartbeat() {
                return frame;
            }
        }
    }

    /// The next non-heartbeat frame, which must have gateway opcode `op`.
    /// Returns the whole payload.
    pub async fn expect_op(&mut self, op: u64) -> Value {
        let frame = self.next_non_heartbeat().await;
        match frame.json() {
            Some(value) if frame.op() == Some(op) => value.clone(),
            _ => panic!(
                "fake gateway connection {}: expected op {op}, got {frame:?}; so far: {:?}",
                self.index,
                self.frames()
            ),
        }
    }

    /// The `d` of the next non-heartbeat frame, which must be an IDENTIFY.
    pub async fn expect_identify(&mut self) -> Value {
        self.expect_op(2).await["d"].clone()
    }

    /// The `d` of the next non-heartbeat frame, which must be a RESUME.
    pub async fn expect_resume(&mut self) -> Value {
        self.expect_op(6).await["d"].clone()
    }

    /// The code of the next non-heartbeat frame, which must be a close frame.
    pub async fn expect_close(&mut self) -> Option<u16> {
        let frame = self.next_non_heartbeat().await;
        match frame {
            Frame::Close { code, .. } => code,
            other => panic!(
                "fake gateway connection {}: expected a close frame, got {other:?}; so far: {:?}",
                self.index,
                self.frames()
            ),
        }
    }

    /// Skip everything until the connection ended.
    pub async fn expect_gone(&mut self) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.try_next_frame(left).await {
                Some(Frame::Gone) => return,
                Some(_) => {}
                None => panic!(
                    "fake gateway connection {} still open after {TIMEOUT:?}: {:?}",
                    self.index,
                    self.frames()
                ),
            }
        }
    }
}
