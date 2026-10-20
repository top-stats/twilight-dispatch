//! Tracks which gateway sequence numbers of a shard are still being processed.
//!
//! Discord replays everything after the sequence we resume from. The shard's
//! own sequence advances as soon as a payload is received, before we cached and
//! published it, so persisting that number loses whatever was still in flight
//! when the process stopped. [`SeqTracker::resume_seq`] instead returns the
//! highest sequence whose payload, and every payload before it, has been
//! published, which gives at-least-once delivery across restarts.

use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};

/// How long a payload that could not be published holds the resume point back.
/// Discord only keeps a session resumable for a few minutes, so older failures
/// cannot be replayed anyway.
pub const FAILED_RETENTION: Duration = Duration::from_secs(15 * 60);
/// At most one failure is remembered per this interval.
const FAILED_GRANULARITY: Duration = Duration::from_secs(1);

/// Identifies one dispatched payload; hand it back to [`SeqTracker::complete`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeqToken {
    epoch: u64,
    seq: u64,
}

#[derive(Debug, Default)]
pub struct SeqTracker {
    epoch: u64,
    highest: u64,
    in_flight: BTreeMap<u64, usize>,
    /// Payloads that could not be published, with when that happened. A
    /// restart within [`FAILED_RETENTION`] resumes before them so Discord
    /// replays them. Only the first failure per [`FAILED_GRANULARITY`] is
    /// kept (only the lowest live one matters), so an outage cannot grow it.
    failed: VecDeque<(u64, Instant)>,
}

impl SeqTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start tracking a new session (after READY). Payloads of the previous
    /// session that are still in flight no longer affect the resume point.
    pub fn new_session(&mut self, ready_seq: u64) {
        self.epoch += 1;
        self.in_flight.clear();
        self.failed.clear();
        self.highest = ready_seq;
    }

    /// Record that the payload with `seq` was received and handed off.
    pub fn dispatch(&mut self, seq: u64) -> SeqToken {
        *self.in_flight.entry(seq).or_default() += 1;
        self.highest = self.highest.max(seq);
        SeqToken {
            epoch: self.epoch,
            seq,
        }
    }

    /// Record that a dispatched payload was fully processed.
    pub fn complete(&mut self, token: SeqToken) {
        if token.epoch != self.epoch {
            return;
        }
        if let Some(count) = self.in_flight.get_mut(&token.seq) {
            *count -= 1;
            if *count == 0 {
                self.in_flight.remove(&token.seq);
            }
        }
    }

    /// Record that a dispatched payload could not be published.
    pub fn fail(&mut self, token: SeqToken, now: Instant) {
        if token.epoch != self.epoch {
            return;
        }
        self.complete(token);
        self.prune(now);
        let fresh = self
            .failed
            .back()
            .is_none_or(|(_, at)| now.duration_since(*at) >= FAILED_GRANULARITY);
        if fresh {
            self.failed.push_back((token.seq, now));
        } else if let Some((seq, _)) = self.failed.back_mut() {
            *seq = (*seq).min(token.seq);
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .failed
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) >= FAILED_RETENTION)
        {
            self.failed.pop_front();
        }
    }

    /// Whether a token belongs to the current session.
    pub fn is_current(&self, token: SeqToken) -> bool {
        token.epoch == self.epoch
    }

    /// The sequence that is safe to resume from.
    pub fn resume_seq(&self) -> u64 {
        self.resume_seq_at(Instant::now())
    }

    pub fn resume_seq_at(&self, now: Instant) -> u64 {
        let failed = self
            .failed
            .iter()
            .filter(|(_, at)| now.duration_since(*at) < FAILED_RETENTION)
            .map(|(seq, _)| *seq)
            .min();
        let lowest = match (self.in_flight.keys().next().copied(), failed) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match lowest {
            Some(lowest) => lowest.saturating_sub(1),
            None => self.highest,
        }
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.values().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_tracker_resumes_from_highest() {
        let mut tracker = SeqTracker::new();
        tracker.new_session(1);
        assert_eq!(tracker.resume_seq(), 1);

        let token = tracker.dispatch(2);
        tracker.complete(token);
        assert_eq!(tracker.resume_seq(), 2);
    }

    #[test]
    fn resume_point_never_passes_unfinished_payloads() {
        let mut tracker = SeqTracker::new();
        tracker.new_session(1);
        let t2 = tracker.dispatch(2);
        let t3 = tracker.dispatch(3);
        let t4 = tracker.dispatch(4);

        // 3 and 4 finished before 2 (they went to other workers).
        tracker.complete(t4);
        tracker.complete(t3);
        assert_eq!(tracker.resume_seq(), 1);
        assert_eq!(tracker.in_flight(), 1);

        tracker.complete(t2);
        assert_eq!(tracker.resume_seq(), 4);
        assert_eq!(tracker.in_flight(), 0);
    }

    #[test]
    fn completions_from_an_old_session_are_ignored() {
        let mut tracker = SeqTracker::new();
        tracker.new_session(1);
        let old = tracker.dispatch(50);

        tracker.new_session(1);
        let new = tracker.dispatch(2);
        tracker.complete(old);
        assert_eq!(tracker.resume_seq(), 1);

        tracker.complete(new);
        assert_eq!(tracker.resume_seq(), 2);
    }

    #[test]
    fn failed_publishes_hold_the_resume_point_for_a_while() {
        let start = Instant::now();
        let mut tracker = SeqTracker::new();
        tracker.new_session(1);
        let t2 = tracker.dispatch(2);
        let t3 = tracker.dispatch(3);
        tracker.fail(t2, start);
        tracker.complete(t3);
        assert_eq!(tracker.in_flight(), 0);
        assert_eq!(
            tracker.resume_seq_at(start),
            1,
            "a restart replays the lost payload"
        );
        assert_eq!(
            tracker.resume_seq_at(start + FAILED_RETENTION),
            3,
            "too old to replay"
        );

        tracker.new_session(1);
        assert_eq!(tracker.resume_seq_at(start), 1);
    }

    #[test]
    fn failures_during_an_outage_stay_bounded() {
        let start = Instant::now();
        let mut tracker = SeqTracker::new();
        tracker.new_session(1);
        // An hour of failing publishes, 100 per second.
        for second in 0..3600u64 {
            for i in 0..100 {
                let seq = 2 + second * 100 + i;
                let token = tracker.dispatch(seq);
                tracker.fail(token, start + Duration::from_secs(second));
            }
        }
        assert!(tracker.failed.len() <= 3600, "{}", tracker.failed.len());
        let now = start + Duration::from_secs(3600);
        // The oldest failure still within the retention is the resume point.
        let expected = 2 + (3600 - FAILED_RETENTION.as_secs() + 1) * 100 - 1;
        assert_eq!(tracker.resume_seq_at(now), expected);
        let token = tracker.dispatch(999_999);
        tracker.fail(token, now);
        assert!(tracker.failed.len() <= (FAILED_RETENTION.as_secs() + 1) as usize);
    }

    #[test]
    fn tokens_know_their_session() {
        let mut tracker = SeqTracker::new();
        tracker.new_session(1);
        let old = tracker.dispatch(2);
        assert!(tracker.is_current(old));
        tracker.new_session(1);
        assert!(!tracker.is_current(old));
    }

    #[test]
    fn duplicate_sequences_are_counted() {
        let mut tracker = SeqTracker::new();
        tracker.new_session(0);
        let a = tracker.dispatch(5);
        let b = tracker.dispatch(5);
        tracker.complete(a);
        assert_eq!(tracker.resume_seq(), 4);
        tracker.complete(b);
        assert_eq!(tracker.resume_seq(), 5);
    }
}
