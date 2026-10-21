//! Identify queue shared by every process of the bot, through Redis.
//!
//! Discord's identify limit is per bot: `max_concurrency` identifies per 5 s,
//! one in each bucket `shard % max_concurrency`, whichever process sends them.
//! twilight's `InMemoryQueue` only spaces the identifies of its own process,
//! so processes that started together (a stack deploy, a node reboot)
//! identified in the same bucket at once. Discord answers that with an invalid
//! session (op 9) and the shard identifies again, spending another session
//! start.
//!
//! [`RedisQueue`] keeps the local queue for the daily session start limit and
//! its reset (and the buckets of a lowered SHARDS_CONCURRENCY), and then also
//! takes the bucket's key (`gateway_identify:<bucket>`) in Redis before the
//! shard may identify. The key is a rate limit, not a lock: it is never
//! released but expires one identify interval (plus a margin) after it was
//! taken, so a process that crashed right after taking it blocks no one for
//! longer than an identify of its own would have.
//!
//! The local queue's 5 s alone would not keep this process' identifies of a
//! bucket apart, since it counts them from when it let the previous shard
//! through and that shard then waits for Redis: one that waited for another
//! process' key until just before the next shard got through, or several
//! waiting shards that Redis then fails at once, would identify less than
//! 5 s apart. So [`RedisQueue`] also spaces them itself, from when it lets
//! them identify, and lets them wait for Redis one at a time, in order.

use crate::{constants::identify_key, models::FormattedDateTime, redis_conn::Redis};

use redis::Script;
use std::{
    fmt::Display,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock,
    },
    time::Duration,
};
use tokio::{
    sync::{oneshot, Mutex},
    time::Instant,
};
use tracing::{info, warn};
use twilight_gateway::queue::{InMemoryQueue, Queue, IDENTIFY_DELAY};

/// Added to the identify interval: an identify reaches Discord a little after
/// its bucket was taken, and not equally late from every process (a busy
/// runtime polls the shard later), so back-to-back identifies of two
/// processes must not end up closer together than Discord's 5 s.
pub const IDENTIFY_MARGIN: Duration = Duration::from_millis(500);

/// How long a shard waits for Redis to answer before it identifies with the
/// local queue only. Redis answers in well under a millisecond unless it is
/// down or busy (a large GUILD_CREATE transaction can hold it for a while,
/// which is why the connection's own timeout is longer); identifies must not
/// wait for an unreachable Redis.
pub const REDIS_TIMEOUT: Duration = Duration::from_secs(2);

/// Take the bucket KEYS[1] for ARGV[1] (the owner) for ARGV[2] milliseconds.
/// Returns 0 when it was taken, otherwise the milliseconds until the holder's
/// key expires (at least 1). One script, so that taking and reading the
/// holder's expiry see the same key.
const TAKE_BUCKET: &str = r"
if redis.call('SET', KEYS[1], ARGV[1], 'NX', 'PX', ARGV[2]) then
  return 0
end
local ttl = redis.call('PTTL', KEYS[1])
if ttl == -1 then
  -- Not written by this queue, which always sets an expiry: without one the
  -- bucket would be blocked for good.
  redis.call('PEXPIRE', KEYS[1], ARGV[2])
  return tonumber(ARGV[2])
end
return math.max(ttl, 1)
";

static TAKE: LazyLock<Script> = LazyLock::new(|| Script::new(TAKE_BUCKET));

/// Identify queue of one process that takes turns with the bot's other
/// processes through Redis (see the module docs). Every process must use the
/// same Redis database. Cloning is cheap.
#[derive(Clone)]
pub struct RedisQueue {
    local: InMemoryQueue,
    shared: Arc<Shared>,
}

struct Shared {
    redis: Redis,
    /// Discord's `max_concurrency`, not the local queue's: Discord's buckets
    /// are the same for every process, whatever SHARDS_CONCURRENCY lowered
    /// the local queue to.
    max_concurrency: u32,
    /// How long a taken bucket stays taken: the identify interval plus
    /// [`IDENTIFY_MARGIN`]. Also how far apart this process' identifies of a
    /// bucket are when Redis takes no part, so that they are no closer
    /// together without Redis than with it.
    hold: Duration,
    /// When this process last let a shard of each of Discord's buckets
    /// identify. A shard holds its bucket's lock from waiting for that
    /// spacing until it may identify, so the shards of a bucket wait for
    /// Redis one at a time and in the order the local queue let them through
    /// (tokio's mutex is fair).
    turns: Box<[Mutex<Option<Instant>>]>,
    /// The key's value, only for whoever inspects it: `<pid>:<start unix ms>`
    /// (the pid alone is 1 in every container).
    owner: String,
    /// The last attempt failed, so the fallback was logged already: once per
    /// outage instead of once per identify.
    degraded: AtomicBool,
}

impl RedisQueue {
    /// `local` is the queue this process would use on its own (built from
    /// Discord's session start limit); `max_concurrency` is Discord's.
    pub fn new(local: InMemoryQueue, max_concurrency: u16, redis: Redis) -> Self {
        Self::with_interval(local, max_concurrency, redis, IDENTIFY_DELAY)
    }

    /// [`RedisQueue::new`] with another interval between the identifies of a
    /// bucket than Discord's 5 s, so that tests need not wait that long. The
    /// local queue keeps its own 5 s.
    pub fn with_interval(
        local: InMemoryQueue,
        max_concurrency: u16,
        redis: Redis,
        interval: Duration,
    ) -> Self {
        let owner = format!(
            "{}:{}",
            std::process::id(),
            FormattedDateTime::now().unix_millis()
        );
        let max_concurrency = max_concurrency.max(1);
        Self {
            local,
            shared: Arc::new(Shared {
                redis,
                max_concurrency: u32::from(max_concurrency),
                hold: interval.saturating_add(IDENTIFY_MARGIN),
                turns: (0..max_concurrency).map(|_| Mutex::new(None)).collect(),
                owner,
                degraded: AtomicBool::new(false),
            }),
        }
    }
}

impl Queue for RedisQueue {
    fn enqueue(&self, shard: u32) -> oneshot::Receiver<()> {
        let (mut tx, rx) = oneshot::channel();
        let local = self.local.enqueue(shard);
        let shared = self.shared.clone();
        tokio::spawn(async move {
            // A shard stops waiting when its connection ends. Then it neither
            // uses a local permit (the local queue skips a request whose
            // receiver is gone) nor takes the bucket from the next shard.
            tokio::select! {
                granted = local => {
                    if granted.is_err() {
                        // Dropping `tx` unanswered makes the shard enqueue again.
                        return;
                    }
                }
                () = tx.closed() => return,
            }
            tokio::select! {
                () = shared.take_turn(shard) => {
                    let _ = tx.send(());
                }
                () = tx.closed() => {}
            }
        });
        rx
    }
}

impl Shared {
    /// Wait until this process may identify `shard`: at least one hold after
    /// its previous identify in the bucket, and once the bucket is ours (or
    /// Redis failed).
    async fn take_turn(&self, shard: u32) {
        let bucket = shard % self.max_concurrency;
        // Dropped unchanged if the shard stops waiting, which hands the turn
        // to the next shard of the bucket.
        let mut last = self.turns[bucket as usize].lock().await;
        if let Some(last) = *last {
            tokio::time::sleep_until(last + self.hold).await;
        }
        self.take_bucket(bucket).await;
        *last = Some(Instant::now());
    }

    /// Wait until the bucket's key was free and is ours now, or Redis answered
    /// with an error or not within [`REDIS_TIMEOUT`].
    async fn take_bucket(&self, bucket: u32) {
        let key = identify_key(bucket);
        let hold_ms = u64::try_from(self.hold.as_millis()).unwrap_or(u64::MAX);
        let mut take = TAKE.key(&key);
        take.arg(&self.owner).arg(hold_ms);
        loop {
            let mut conn = self.redis.clone();
            match tokio::time::timeout(REDIS_TIMEOUT, take.invoke_async::<u64>(&mut conn)).await {
                Ok(Ok(0)) => {
                    self.answered();
                    return;
                }
                Ok(Ok(left)) => {
                    self.answered();
                    // Bounded by the hold, which is what a holder sets.
                    tokio::time::sleep(Duration::from_millis(left).min(self.hold)).await;
                }
                Ok(Err(err)) => {
                    self.fall_back(err);
                    return;
                }
                Err(_) => {
                    self.fall_back(format!("no answer within {REDIS_TIMEOUT:?}"));
                    return;
                }
            }
        }
    }

    fn fall_back(&self, reason: impl Display) {
        if !self.degraded.swap(true, Ordering::Relaxed) {
            warn!(
                "Cannot take turns identifying with other processes through Redis ({}); identifying with this process' queue only until Redis answers again",
                reason
            );
        }
    }

    fn answered(&self) {
        if self.degraded.swap(false, Ordering::Relaxed) {
            info!("Identifies take turns with other processes through Redis again");
        }
    }
}
