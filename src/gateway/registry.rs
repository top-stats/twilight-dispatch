//! Shared view of every shard of this process.
//!
//! A `Shard` is driven by exactly one task and cannot be shared, so the runner
//! publishes a snapshot of its state here after every message. The metrics,
//! status and session jobs and the command consumer read it from here.

use super::seq::{SeqToken, SeqTracker};
use crate::models::{FormattedDateTime, SessionInfo, StatusInfo};

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use twilight_gateway::{MessageSender, ShardState};

/// Status labels, kept compatible with earlier versions' stage names.
pub const STATUS_LABELS: [&str; 6] = [
    "Connected",
    "Disconnected",
    "Handshaking",
    "Identifying",
    "Resuming",
    "FatallyClosed",
];

pub fn status_label(state: ShardState) -> &'static str {
    match state {
        ShardState::Active => "Connected",
        ShardState::Disconnected { .. } => "Disconnected",
        ShardState::FatallyClosed => "FatallyClosed",
        ShardState::Identifying => "Identifying",
        ShardState::Resuming => "Resuming",
    }
}

#[derive(Clone, Debug)]
pub struct ShardSnapshot {
    pub status: &'static str,
    pub active: bool,
    pub fatal: bool,
    pub close_code: Option<u16>,
    pub latency_ms: u64,
    /// When the last heartbeat ACK arrived.
    pub last_ack: Option<Instant>,
    pub last_message: Option<Instant>,
    pub session_id: Option<String>,
    pub resume_url: Option<String>,
    /// Set while the runner waits for a connection it closed to finish.
    pub close_wait_since: Option<Instant>,
}

impl Default for ShardSnapshot {
    fn default() -> Self {
        Self {
            status: "Disconnected",
            active: false,
            fatal: false,
            close_code: None,
            latency_ms: 0,
            last_ack: None,
            last_message: None,
            session_id: None,
            resume_url: None,
            close_wait_since: None,
        }
    }
}

impl ShardSnapshot {
    pub fn silence(&self) -> Option<Duration> {
        self.last_message.map(|at| at.elapsed())
    }
}

pub struct ShardHandle {
    pub id: u32,
    /// `None` only in tests, where no real shard exists.
    pub sender: Option<MessageSender>,
    snapshot: Mutex<ShardSnapshot>,
    seq: Mutex<SeqTracker>,
}

impl ShardHandle {
    pub fn new(id: u32, sender: Option<MessageSender>, resumed_seq: Option<u64>) -> Self {
        let mut seq = SeqTracker::new();
        if let Some(resumed) = resumed_seq {
            seq.new_session(resumed);
        }
        Self {
            id,
            sender,
            snapshot: Mutex::new(ShardSnapshot::default()),
            seq: Mutex::new(seq),
        }
    }

    pub fn snapshot(&self) -> ShardSnapshot {
        self.snapshot.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn update(&self, f: impl FnOnce(&mut ShardSnapshot)) {
        if let Ok(mut snapshot) = self.snapshot.lock() {
            f(&mut snapshot);
        }
    }

    pub fn new_session(&self, ready_seq: u64) {
        if let Ok(mut seq) = self.seq.lock() {
            seq.new_session(ready_seq);
        }
    }

    pub fn dispatch(&self, seq: u64) -> Option<SeqToken> {
        self.seq
            .lock()
            .ok()
            .map(|mut tracker| tracker.dispatch(seq))
    }

    pub fn complete(&self, token: SeqToken) {
        if let Ok(mut seq) = self.seq.lock() {
            seq.complete(token);
        }
    }

    /// Whether a job was dispatched in the shard's current session.
    pub fn is_current(&self, token: SeqToken) -> bool {
        self.seq
            .lock()
            .map(|seq| seq.is_current(token))
            .unwrap_or(true)
    }

    /// The payload could not be published; keep the resume point before it.
    pub fn fail(&self, token: SeqToken) {
        if let Ok(mut seq) = self.seq.lock() {
            seq.fail(token, Instant::now());
        }
    }

    pub fn in_flight(&self) -> usize {
        self.seq.lock().map(|seq| seq.in_flight()).unwrap_or(0)
    }

    /// The session to persist for a resume after a restart, if any.
    pub fn session_info(&self) -> Option<SessionInfo> {
        let snapshot = self.snapshot();
        let sequence = self.seq.lock().ok()?.resume_seq();
        let info = SessionInfo {
            session_id: snapshot.session_id?,
            sequence,
            resume_url: snapshot.resume_url,
        };
        info.is_resumable().then_some(info)
    }

    pub fn status_info(&self, now: FormattedDateTime) -> StatusInfo {
        let snapshot = self.snapshot();
        let last_ack = snapshot
            .last_ack
            .map(|at| now - time::Duration::milliseconds(at.elapsed().as_millis() as i64))
            .unwrap_or(now);
        StatusInfo {
            shard: self.id,
            status: snapshot.status.to_owned(),
            latency: snapshot.latency_ms,
            last_ack,
        }
    }
}

#[derive(Default)]
pub struct Registry {
    shards: BTreeMap<u32, Arc<ShardHandle>>,
}

impl Registry {
    pub fn new(handles: impl IntoIterator<Item = Arc<ShardHandle>>) -> Self {
        Self {
            shards: handles.into_iter().map(|h| (h.id, h)).collect(),
        }
    }

    pub fn get(&self, shard: u32) -> Option<&Arc<ShardHandle>> {
        self.shards.get(&shard)
    }

    pub fn handles(&self) -> impl Iterator<Item = &Arc<ShardHandle>> {
        self.shards.values()
    }

    pub fn snapshots(&self) -> Vec<(u32, ShardSnapshot)> {
        self.shards
            .iter()
            .map(|(id, h)| (*id, h.snapshot()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.shards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_previous_stage_names() {
        assert_eq!(status_label(ShardState::Active), "Connected");
        assert_eq!(
            status_label(ShardState::Disconnected {
                reconnect_attempts: 3
            }),
            "Disconnected"
        );
        assert_eq!(status_label(ShardState::Identifying), "Identifying");
        assert_eq!(status_label(ShardState::Resuming), "Resuming");
        assert_eq!(status_label(ShardState::FatallyClosed), "FatallyClosed");
        for state in [
            ShardState::Active,
            ShardState::Disconnected {
                reconnect_attempts: 0,
            },
            ShardState::FatallyClosed,
            ShardState::Identifying,
            ShardState::Resuming,
        ] {
            assert!(STATUS_LABELS.contains(&status_label(state)));
        }
    }
}
