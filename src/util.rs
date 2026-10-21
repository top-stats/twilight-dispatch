//! Small helpers shared by the modules.

use std::{
    fmt,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Rate limit for repeated warnings. The old code logged one line per gateway
/// event while Redis was unreachable, hundreds per millisecond.
///
/// Callers log only when [`Throttle::check`] says so and append the count it
/// returns as `({} similar suppressed)`. `Throttle::default()` lets one
/// message through every 5 s.
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    /// When a message was last let through, and how many were suppressed
    /// since.
    state: Mutex<(Option<Instant>, u64)>,
}

impl Throttle {
    /// A throttle that lets the first message through, then at most one per
    /// `interval`, counted from the last message it let through.
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new((None, 0)),
        }
    }

    /// `Some(suppressed)` when a message may be logged now, with the number of
    /// messages suppressed since the last one; `None` to stay quiet. If the
    /// lock was poisoned every message is let through, as `Some(0)`.
    pub fn check(&self) -> Option<u64> {
        self.check_at(Instant::now())
    }

    /// [`Throttle::check`] at a given time, so tests need not sleep through
    /// the interval.
    pub fn check_at(&self, now: Instant) -> Option<u64> {
        let Ok(mut state) = self.state.lock() else {
            return Some(0);
        };
        let (last, suppressed) = &mut *state;
        match last {
            Some(at) if now.duration_since(*at) < self.interval => {
                *suppressed += 1;
                None
            }
            _ => {
                *last = Some(now);
                Some(std::mem::take(suppressed))
            }
        }
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}

/// Displays an error followed by its chain of sources, `outer: inner:
/// innermost`, for logging errors (such as twilight's) whose `Display` leaves
/// the cause out.
#[derive(Clone, Copy, Debug)]
pub struct ErrorChain<'a>(pub &'a (dyn std::error::Error + 'static));

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(err) = source {
            write!(f, ": {err}")?;
            source = err.source();
        }
        Ok(())
    }
}

#[cfg(test)]
mod error_chain_tests {
    use super::ErrorChain;
    use std::{error::Error, fmt};

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for Layer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.1.as_deref().map(|inner| inner as &(dyn Error + 'static))
        }
    }

    #[test]
    fn every_source_is_shown_outermost_first() {
        let innermost = Layer("connection refused", None);
        let inner = Layer("failed to connect", Some(Box::new(innermost)));
        let outer = Layer("failed to reconnect to the gateway", Some(Box::new(inner)));
        assert_eq!(
            ErrorChain(&outer).to_string(),
            "failed to reconnect to the gateway: failed to connect: connection refused"
        );
        assert_eq!(
            ErrorChain(&Layer("alone", None)).to_string(),
            "alone",
            "an error without a source is shown as is"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_reports_suppressed_messages() {
        let throttle = Throttle::new(Duration::from_secs(5));
        let start = Instant::now();
        assert_eq!(throttle.check_at(start), Some(0));
        assert_eq!(throttle.check_at(start + Duration::from_millis(1)), None);
        assert_eq!(throttle.check_at(start + Duration::from_secs(1)), None);
        assert_eq!(throttle.check_at(start + Duration::from_secs(6)), Some(2));
        assert_eq!(throttle.check_at(start + Duration::from_secs(7)), None);
    }

    #[test]
    fn the_interval_runs_from_the_last_logged_message() {
        let interval = Duration::from_secs(5);
        let just_before = interval - Duration::from_nanos(1);
        let throttle = Throttle::new(interval);
        let start = Instant::now();
        assert_eq!(throttle.check_at(start), Some(0));
        assert_eq!(throttle.check_at(start + just_before), None);
        assert_eq!(
            throttle.check_at(start + interval),
            Some(1),
            "exactly one interval"
        );

        // Suppressed checks do not push the next message back.
        let logged = start + interval;
        for second in 1..5 {
            assert_eq!(
                throttle.check_at(logged + Duration::from_secs(second)),
                None
            );
        }
        assert_eq!(throttle.check_at(logged + interval), Some(4));
        // The count starts over after it was reported.
        assert_eq!(throttle.check_at(logged + interval * 2), Some(0));
    }
}

#[cfg(test)]
mod poisoned_throttle_tests {
    use super::*;

    /// A panic while the throttle's lock was held must not silence the
    /// warnings for good: from then on every message is let through, without
    /// a count.
    #[test]
    fn a_poisoned_throttle_lets_every_message_through() {
        let throttle = Throttle::new(Duration::from_secs(60));
        let start = Instant::now();
        assert_eq!(throttle.check_at(start), Some(0));
        assert_eq!(throttle.check_at(start), None, "within the interval");

        let panicked = std::panic::catch_unwind(|| {
            let _state = throttle.state.lock().unwrap();
            panic!("a panic while the throttle is locked");
        });
        assert!(panicked.is_err());
        assert!(throttle.state.is_poisoned());

        for _ in 0..3 {
            assert_eq!(throttle.check_at(start), Some(0));
        }
        assert_eq!(throttle.check(), Some(0));
    }
}
