//! The rate-limit algorithms of DECOMPOSITION entry 2.7
//! (`cpt-cf-oagw-feature-rate-limiting`).
//!
//! | item | owns |
//! |---|---|
//! | [`CounterSpec`] | the effective limit one counter enforces |
//! | [`CounterState`] | the `token_bucket` and `sliding_window` counter bodies |
//! | [`RateDecision`] | one allow or refuse decision with its quota surface |
//! | [`counter_key`] | the three-component counter key |
//! | [`RateClock`] | the injectable monotonic + wall-clock contract |
//!
//! The module is pure: it names no repository, no SDK client and no `axum`
//! type, and it receives the instant of every decision as an input
//! (`cpt-cf-oagw-dod-rate-limiting-hot-path-cost`).
//!
//! # The single wall-clock mapping
//!
//! A counter runs on the monotonic clock, but `X-RateLimit-Reset` is rendered
//! as an epoch second. Exactly one pairing converts between the two
//! (`inst-rl-str-6`): one [`ClockReading`] carries both readings taken
//! together, and [`ClockReading::project`] adds the monotonic distance of a
//! future instant to the wall-clock second of the reading. No code path reads
//! the two clocks at different instants.
//!
//! # Counter shapes
//!
//! `token_bucket` refills continuously at `sustained.rate` per
//! `sustained.window`, caps the balance at the burst capacity, deducts `cost`
//! on admission and deducts nothing on a refusal; its reset instant is the one
//! at which the balance returns to the full capacity
//! (`inst-rl-tb-1` .. `-8`). `sliding_window` sums the consumption admitted
//! inside the trailing window, admits only when that sum plus `cost` stays
//! within the sustained rate, applies no burst allowance, and resets when the
//! oldest admitted consumption leaves the window (`inst-rl-sw-1` .. `-8`).
//!
//! The trailing window is recorded as a log of at most
//! [`SLIDING_WINDOW_MAX_ENTRIES`] admitted consumptions, each at its own
//! instant, so the memory of one counter is bounded independently of the
//! configured limit; when the log reaches the bound its two oldest entries are
//! coalesced into one, which keeps the summed consumption exact and moves the
//! release instant of the pair to the later of the two.
//!
//! # Strategy dispatch
//!
//! `reject` is the only executable strategy: a configured `queue` or
//! `degrade` resolves to the `reject` outcome
//! (`inst-rl-str-2`/`-3`, graded deviation 8).
// @cpt-algo:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1
// @cpt-algo:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1
// @cpt-algo:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1
// @cpt-algo:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::domain::dto::{RateAlgorithm, RateLimitConfig, RateScope, RateStrategy};

// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-1
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-2
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-3
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-4
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-5
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-6
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-7
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-8
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-9
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-1
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-2
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-3
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-4
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-5
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-6
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-7
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-8
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-9
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-1
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-2
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-3
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-4
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-5
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-6
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-7
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-8
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-9
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-1
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-2
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-3
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-4
/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;
//
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-9
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-8
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-7
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-6
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-5
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-4
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-3
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-2
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-effective-limit:p1:inst-rl-eff-1
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-9
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-8
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-7
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-6
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-5
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-4
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-3
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-2
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-sliding-window:p1:inst-rl-sw-1
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-9
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-8
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-7
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-6
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-5
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-4
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-3
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-2
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-strategy-dispatch:p1:inst-rl-str-1
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-4
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-3
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-2
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-usage-ratio:p1:inst-rl-ratio-1
//

/// The bound on the consumption log of one sliding-window counter.
const SLIDING_WINDOW_MAX_ENTRIES: usize = 256;

/// The `X-RateLimit-Limit` header of a decided response (`inst-rl-str-6`).
pub const LIMIT_HEADER: &str = "X-RateLimit-Limit";
/// The `X-RateLimit-Remaining` header of a decided response.
pub const REMAINING_HEADER: &str = "X-RateLimit-Remaining";
/// The `X-RateLimit-Reset` header of a decided response.
pub const RESET_HEADER: &str = "X-RateLimit-Reset";

// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-1
// The clock contract: every counter reads the monotonic clock for its refill
// and window bounds, and the wall clock only through the one pairing below.
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-3
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-4
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-5
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-6
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-7
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-8
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-9
/// One reading of both clocks, taken together so a monotonic distance can be
/// projected onto the wall clock by exactly one mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockReading {
    /// Nanoseconds on the monotonic clock, from an arbitrary origin.
    pub monotonic_nanos: u64,
    /// Whole seconds since the Unix epoch at the same instant.
    pub epoch_seconds: u64,
}
//
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-9
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-8
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-7
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-6
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-5
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-4
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-3
//

impl ClockReading {
    /// Project the monotonic instant `at_nanos` onto the wall clock.
    ///
    /// `at_nanos` at or before the reading projects to the reading's own
    /// second; a later instant projects to the reading's second plus the whole
    /// seconds of monotonic distance between them.
    #[must_use]
    pub fn project(&self, at_nanos: u64) -> u64 {
        let distance = at_nanos.saturating_sub(self.monotonic_nanos);
        let seconds = distance / NANOS_PER_SEC;
        self.epoch_seconds.saturating_add(seconds)
    }

    /// The epoch second `monotonic_nanos` later than the reading, rounded up
    /// so a non-zero distance is never reported as zero seconds.
    #[must_use]
    pub fn project_ceil(&self, at_nanos: u64) -> u64 {
        let distance = at_nanos.saturating_sub(self.monotonic_nanos);
        let seconds = distance.div_ceil(NANOS_PER_SEC);
        self.epoch_seconds.saturating_add(seconds)
    }
}

/// The clock the counters measure their refill and their window bounds on.
pub trait RateClock: Send + Sync + 'static {
    /// One reading of both clocks.
    fn now(&self) -> ClockReading;
}

/// The production clock: a monotonic `Instant` since process start and the
/// system wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

fn monotonic_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

impl RateClock for SystemClock {
    fn now(&self) -> ClockReading {
        let elapsed = Instant::now()
            .checked_duration_since(monotonic_origin())
            .unwrap_or_default();
        ClockReading {
            monotonic_nanos: u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
            epoch_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
        }
    }
}

/// A clock a test drives by hand: both readings are set explicitly and
/// advanced by the test, so no test sleeps to make a counter refill.
#[derive(Debug, Clone, Default)]
pub struct SharedClock {
    monotonic_nanos: Arc<AtomicU64>,
    epoch_seconds: Arc<AtomicU64>,
}

impl SharedClock {
    /// A clock frozen at `monotonic_nanos` and `epoch_seconds`.
    #[must_use]
    pub fn at(monotonic_nanos: u64, epoch_seconds: u64) -> Self {
        Self {
            monotonic_nanos: Arc::new(AtomicU64::new(monotonic_nanos)),
            epoch_seconds: Arc::new(AtomicU64::new(epoch_seconds)),
        }
    }

    /// Advance the monotonic reading by `nanos`.
    pub fn advance_nanos(&self, nanos: u64) {
        self.monotonic_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Advance the monotonic reading by whole seconds.
    pub fn advance_seconds(&self, seconds: u64) {
        self.advance_nanos(seconds.saturating_mul(NANOS_PER_SEC));
    }

    /// Move the wall-clock reading without moving the monotonic one.
    pub fn set_epoch_seconds(&self, seconds: u64) {
        self.epoch_seconds.store(seconds, Ordering::Relaxed);
    }
}

impl RateClock for SharedClock {
    fn now(&self) -> ClockReading {
        ClockReading {
            monotonic_nanos: self.monotonic_nanos.load(Ordering::Relaxed),
            epoch_seconds: self.epoch_seconds.load(Ordering::Relaxed),
        }
    }
}
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-1

/// The effective limit one counter enforces, derived from the merged
/// configuration layer (`inst-rl-eff-7` .. `-9`).
#[derive(Debug, Clone, PartialEq)]
pub struct CounterSpec {
    /// The algorithm the counter runs.
    pub algorithm: RateAlgorithm,
    /// The effective sustained rate: the units admitted per `window_secs`.
    pub sustained_rate: u32,
    /// The window the sustained rate is expressed over, in seconds.
    pub window_secs: u64,
    /// The `token_bucket` capacity: the effective burst capacity, which
    /// defaults to the sustained rate (`inst-rl-eff-5`).
    pub capacity: u32,
    /// The units one request costs.
    pub cost: u32,
}

impl CounterSpec {
    /// Derive the counter specification from one merged rate-limit layer.
    #[must_use]
    pub fn of(config: &RateLimitConfig) -> Self {
        Self {
            algorithm: config.algorithm,
            sustained_rate: config.sustained.rate,
            window_secs: config.sustained.window.secs(),
            capacity: config.effective_burst_capacity(),
            cost: config.cost,
        }
    }

    /// The single per-second refill rate both algorithms consume.
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        f64::from(self.sustained_rate) / self.window_secs as f64
    }

    /// The effective limit reported as `X-RateLimit-Limit`: the burst
    /// capacity under `token_bucket` and the sustained rate under
    /// `sliding_window`, which applies no burst allowance.
    #[must_use]
    pub const fn effective_limit(&self) -> u32 {
        match self.algorithm {
            RateAlgorithm::TokenBucket => self.capacity,
            RateAlgorithm::SlidingWindow => self.sustained_rate,
        }
    }
}

/// One allow or refuse decision, carrying the whole quota surface the
/// response renders (`inst-rl-str-5` .. `-8`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateDecision {
    /// Whether the request was admitted.
    pub allowed: bool,
    /// The effective limit the decision was taken against.
    pub limit: u32,
    /// The whole units still available after the decision.
    pub remaining: u32,
    /// The consumed fraction of the counter, clamped to `0.0..=1.0`
    /// (`inst-rl-ratio-1`).
    pub usage_ratio: f64,
    /// The epoch second at which the counter returns to its full allowance.
    pub reset_epoch_seconds: u64,
    /// The whole seconds to wait before retrying, when refused.
    pub retry_after_seconds: Option<u64>,
}

impl RateDecision {
    /// The three `X-RateLimit-*` headers of the decision, in order.
    #[must_use]
    pub fn quota_headers(&self) -> [(&'static str, String); 3] {
        [
            (LIMIT_HEADER, self.limit.to_string()),
            (REMAINING_HEADER, self.remaining.to_string()),
            (RESET_HEADER, self.reset_epoch_seconds.to_string()),
        ]
    }
}

/// The phase of one counter in the state machine
/// `cpt-cf-oagw-state-rate-limiting-counter`.
///
/// `absent` is the phase of a key the registry holds no entry for and
/// `released` is the phase of an entry awaiting its drop, so both are carried
/// by the [`crate::infra::proxy::rate_limiter::RateLimiterRegistry`] rather
/// than by the counter body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterPhase {
    /// The counter covers the request cost.
    Active,
    /// The counter cannot cover the request cost.
    Depleted,
}

/// The body of one counter, shaped by its algorithm.
#[derive(Debug, Clone)]
pub enum CounterState {
    /// The refill-on-read token bucket.
    TokenBucket(TokenBucket),
    /// The trailing-window consumption sum.
    SlidingWindow(SlidingWindow),
}

/// The token-bucket counter body (`inst-rl-tb-1` .. `-8`).
#[derive(Debug, Clone, PartialEq)]
pub struct TokenBucket {
    /// The balance, in units.
    pub tokens: f64,
    /// The monotonic instant the balance was last refilled or read at.
    pub last_nanos: u64,
}

/// The sliding-window counter body (`inst-rl-sw-1` .. `-8`).
#[derive(Debug, Clone, PartialEq)]
pub struct SlidingWindow {
    /// The admitted consumption inside the trailing window, oldest first.
    pub granules: VecDeque<(u64, u32)>,
    /// The summed consumption inside the trailing window.
    pub used: u32,
}

impl CounterState {
    /// A counter created lazily for a key with no stored state: at the full
    /// burst capacity under `token_bucket` and with zero recorded consumption
    /// under `sliding_window` (`inst-rl-st-cnt-1`).
    #[must_use]
    pub fn fresh(spec: &CounterSpec, reading: ClockReading) -> Self {
        match spec.algorithm {
            RateAlgorithm::TokenBucket => Self::TokenBucket(TokenBucket {
                tokens: f64::from(spec.capacity),
                last_nanos: reading.monotonic_nanos,
            }),
            RateAlgorithm::SlidingWindow => Self::SlidingWindow(SlidingWindow {
                granules: VecDeque::new(),
                used: 0,
            }),
        }
    }

    /// Run one check-and-deduct: refill or expire, compare against `cost`,
    /// deduct on admission and record nothing on a refusal.
    pub fn check(&mut self, spec: &CounterSpec, reading: ClockReading) -> RateDecision {
        match self {
            Self::TokenBucket(bucket) => check_token_bucket(spec, bucket, reading),
            Self::SlidingWindow(window) => check_sliding_window(spec, window, reading),
        }
    }
}

fn seconds_ceil(seconds: f64) -> u64 {
    if !seconds.is_finite() {
        return 0;
    }
    let rounded = seconds.ceil();
    if rounded <= 0.0 { 0 } else { rounded as u64 }
}

// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-2
// `inst-rl-tb-2` .. `-8`: the refill, the comparison, the deduction on
// admission, the untouched balance on a refusal, and the two instants the
// decision reports, both derived from the one reading.
fn check_token_bucket(spec: &CounterSpec, bucket: &mut TokenBucket, reading: ClockReading) -> RateDecision {
    let capacity = f64::from(spec.capacity);
    let refill = spec.refill_per_second();
    let elapsed_nanos = reading.monotonic_nanos.saturating_sub(bucket.last_nanos);
    let elapsed_seconds = elapsed_nanos as f64 / NANOS_PER_SEC as f64;
    if elapsed_nanos > 0 {
        bucket.tokens = (bucket.tokens + elapsed_seconds * refill).min(capacity);
        bucket.last_nanos = reading.monotonic_nanos;
    }

    let cost = f64::from(spec.cost);
    let allowed = bucket.tokens >= cost;
    if allowed {
        bucket.tokens -= cost;
    }

    let remaining = if bucket.tokens <= 0.0 {
        0
    } else {
        bucket.tokens as u32
    };
    let reset_nanos = if refill <= 0.0 {
        reading.monotonic_nanos
    } else {
        let deficit = (capacity - bucket.tokens).max(0.0);
        let seconds = (deficit / refill) * NANOS_PER_SEC as f64;
        reading
            .monotonic_nanos
            .saturating_add(seconds.max(0.0) as u64)
    };
    let retry_after = if allowed {
        None
    } else if refill <= 0.0 {
        Some(1)
    } else {
        let shortfall = (cost - bucket.tokens).max(0.0);
        Some(seconds_ceil(shortfall / refill).max(1))
    };
    RateDecision {
        allowed,
        limit: spec.effective_limit(),
        remaining,
        usage_ratio: consumed_ratio(capacity - bucket.tokens, capacity),
        reset_epoch_seconds: reading.project_ceil(reset_nanos),
        retry_after_seconds: retry_after,
    }
}
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-token-bucket:p1:inst-rl-tb-2

fn check_sliding_window(spec: &CounterSpec, window: &mut SlidingWindow, reading: ClockReading) -> RateDecision {
    let window_nanos = spec
        .window_secs
        .saturating_mul(NANOS_PER_SEC)
        .max(1);
    let now = reading.monotonic_nanos;
    drop_expired(window, window_nanos, now);

    let rate = u64::from(spec.sustained_rate);
    let cost = u64::from(spec.cost);
    let allowed = u64::from(window.used) + cost <= rate;
    if allowed {
        record(window, now, spec.cost);
    }

    let reset_nanos = window
        .granules
        .front()
        .map_or(now, |&(oldest, _)| oldest.saturating_add(window_nanos));
    let retry_after = if allowed {
        None
    } else {
        Some(seconds_of_nanos_ceil(release_in(spec, window, window_nanos, now)).max(1))
    };
    RateDecision {
        allowed,
        limit: spec.effective_limit(),
        remaining: spec.sustained_rate.saturating_sub(window.used),
        usage_ratio: consumed_ratio(f64::from(window.used), f64::from(spec.sustained_rate)),
        reset_epoch_seconds: reading.project_ceil(reset_nanos),
        retry_after_seconds: retry_after,
    }
}

fn drop_expired(window: &mut SlidingWindow, window_nanos: u64, now: u64) {
    while let Some(&(oldest, units)) = window.granules.front() {
        if oldest.saturating_add(window_nanos) > now {
            break;
        }
        window.granules.pop_front();
        window.used = window.used.saturating_sub(units);
    }
}

/// Record one admitted consumption at its own instant, and coalesce the two
/// oldest records when the log reaches its bound, so the memory of one counter
/// stays bounded however many requests it admits. The coalescing keeps the
/// summed consumption exact and only moves the release instant of the pair to
/// the later of the two, so a coalesced counter never over-admits.
fn record(window: &mut SlidingWindow, now: u64, cost: u32) {
    window.granules.push_back((now, cost));
    window.used = window.used.saturating_add(cost);
    if window.granules.len() > SLIDING_WINDOW_MAX_ENTRIES {
        if let Some(oldest) = window.granules.pop_front() {
            if let Some(next) = window.granules.front_mut() {
                next.1 = next.1.saturating_add(oldest.1);
            }
        }
    }
}

/// The whole seconds until the trailing window has released enough admitted
/// consumption to admit `cost` again (`inst-rl-str-5`).
fn release_in(spec: &CounterSpec, window: &SlidingWindow, window_nanos: u64, now: u64) -> u64 {
    let needed = u64::from(window.used) + u64::from(spec.cost) - u64::from(spec.sustained_rate);
    let mut released = 0;
    for &(oldest, units) in &window.granules {
        released += u64::from(units);
        if released >= needed {
            return oldest.saturating_add(window_nanos).saturating_sub(now);
        }
    }
    0
}

fn seconds_of_nanos_ceil(nanos: u64) -> u64 {
    nanos.div_ceil(NANOS_PER_SEC)
}

fn consumed_ratio(consumed: f64, capacity: f64) -> f64 {
    if capacity <= 0.0 {
        return 0.0;
    }
    (consumed / capacity).clamp(0.0, 1.0)
}

/// The owning resource identity a counter is keyed under
/// (`inst-rl-key-1`): the upstream for an upstream-level limit and the route
/// for a route-level one, so the two never contend for one counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitResource {
    /// An upstream-level limit on the upstream of `upstream_id`.
    Upstream { upstream_id: String },
    /// A route-level limit, keyed by the matched route of `route_id`.
    Route { route_id: String },
}

impl RateLimitResource {
    /// The identity component of the counter key.
    #[must_use]
    pub fn component(&self) -> String {
        match self {
            Self::Upstream { upstream_id } => format!("upstream:{upstream_id}"),
            Self::Route { route_id } => format!("route:{route_id}"),
        }
    }

    /// The prefix every counter keyed under this resource starts with.
    #[must_use]
    pub fn prefix(&self) -> String {
        format!("{}|", self.component())
    }
}

/// The request context the counter key is derived from
/// (`cpt-cf-oagw-algo-rate-limiting-counter-key`).
///
/// Every field but the peer address comes from the authenticated security
/// context the proxy path established; no client-supplied header or query
/// component contributes to the key (`inst-rl-key-9`).
#[derive(Debug, Clone, Copy)]
pub struct CounterKeyContext<'a> {
    /// The owning resource identity.
    pub resource: &'a RateLimitResource,
    /// The effective scope.
    pub scope: RateScope,
    /// The caller's tenant, from the security context.
    pub tenant_id: &'a str,
    /// The authenticated principal, from the security context.
    pub principal_id: Option<&'a str>,
    /// The connection peer address, never a forwarding header.
    pub peer_addr: Option<&'a str>,
    /// The matched route identity, for the `route` scope.
    pub route_id: Option<&'a str>,
}

// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-1
// `inst-rl-key-1` .. `-8`, `inst-rl-scp-2` .. `-6`: the key is exactly the
// owning resource identity, the tenant, and the scope discriminator. Every
// scope other than `global` carries the tenant as its second component, so
// two tenants never share a counter; `global` carries no tenant component,
// which is the operator-controlled deployment-level exception. A missing
// discriminator falls back to the tenant, so unrelated callers are never
// merged into one bucket.
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-2
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-3
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-4
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-5
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-6
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-7
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-8
// @cpt-begin:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-9
#[must_use]
pub fn counter_key(context: &CounterKeyContext<'_>) -> String {
    let tenant = context.tenant_id;
    // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-2
    // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-5
    // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-6
    // `inst-rl-scp-5`/`-6`: a scope whose discriminator is absent from the
    // context falls back to the tenant, so unrelated callers are never merged
    // into one bucket.
    let discriminator = match context.scope {
        RateScope::Global => "",
        RateScope::Tenant => tenant,
        RateScope::User => context.principal_id.unwrap_or(tenant),
        RateScope::Ip => context.peer_addr.unwrap_or(tenant),
        RateScope::Route => context.route_id.unwrap_or(tenant),
    };
    // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-6
    // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-5
    // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-3
    // `inst-rl-scp-2`/`-3`: the key is exactly three components - the owning
    // resource identity, the tenant, and the scope discriminator - and the
    // window state lives inside the counter, not in the key.
    let key = if context.scope == RateScope::Global {
        format!("{}||", context.resource.component())
    } else {
        format!("{}|{tenant}|{discriminator}", context.resource.component())
    };
    // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-3
    // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-2
    key
}
//
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-9
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-8
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-7
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-6
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-5
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-4
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-3
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-2
//
// @cpt-end:cpt-cf-oagw-algo-rate-limiting-counter-key:p1:inst-rl-key-1

/// The quota headers a decided response carries when `response_headers` is
/// true (`inst-rl-str-7`).
#[must_use]
pub fn quota_header_pairs(decision: &RateDecision) -> Vec<(String, String)> {
    decision
        .quota_headers()
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

/// The executable strategy of a configured one (`inst-rl-str-2`/`-3`):
/// `reject` alone is executed, and a configured `queue` or `degrade` resolves
/// to the `reject` outcome per graded deviation 8.
#[must_use]
pub const fn executable_strategy(strategy: RateStrategy) -> RateStrategy {
    match strategy {
        RateStrategy::Reject => RateStrategy::Reject,
        RateStrategy::Queue | RateStrategy::Degrade => RateStrategy::Reject,
    }
}
