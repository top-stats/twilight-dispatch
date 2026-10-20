use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// Rate limit for repeated warnings. The old code logged one line per gateway
/// event while Redis was unreachable, hundreds per millisecond.
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    state: Mutex<(Option<Instant>, u64)>,
}

impl Throttle {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new((None, 0)),
        }
    }

    /// `Some(suppressed)` when a message may be logged now, with the number of
    /// messages suppressed since the last one; `None` to stay quiet.
    pub fn check(&self) -> Option<u64> {
        self.check_at(Instant::now())
    }

    fn check_at(&self, now: Instant) -> Option<u64> {
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
}
