//! The in-memory counter registry of entry 2.7
//! (`cpt-cf-oagw-flow-rate-limiting-counter-scope`).
//!
//! | item | owns |
//! |---|---|
//! | [`RateLimiterRegistry`] | the per-key locks, the bound and the idle eviction |
//!
//! Every counter lives in memory inside the data-plane instance that resolved
//! it: no cross-instance coordination, no persisted counter state, no external
//! store (`inst-rl-scp-4`, graded deviation 5). The registry is bounded
//! explicitly — at most [`DEFAULT_MAX_COUNTERS`] counters, and a key that has
//! not been consulted for [`DEFAULT_IDLE_EVICTION_SECS`] seconds is dropped —
//! and an evicted key is dropped without being logged and without emitting any
//! metric observation.
//!
//! The unit of synchronization is the per-key lock, so the check-and-deduct of
//! one counter is atomic: two concurrent acquires of the same key are fully
//! serialized and the counter is never over-deducted (`inst-rl-scp-10`,
//! `inst-rl-tb-9`, `inst-rl-sw-9`). Acquires of *different* keys take
//! different locks and never contend.
// @cpt-state:cpt-cf-oagw-state-rate-limiting-counter:p1

use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;

use crate::domain::rate_limit::{
    ClockReading, CounterPhase, CounterSpec, CounterState, RateClock, RateDecision,
    RateLimitResource,
};

// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-1
// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-2
// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-3
// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-4
// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-5
// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-6
// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-7
// @cpt-begin:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-8
/// The bound the registry holds, mirroring the 10,000-entry L1 LRU posture of
/// `cpt-cf-oagw-adr-state-management` (`inst-rl-scp-4`).
pub const DEFAULT_MAX_COUNTERS: usize = 10_000;
//
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-8
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-7
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-6
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-5
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-4
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-3
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-2
// @cpt-end:cpt-cf-oagw-state-rate-limiting-counter:p1:inst-rl-st-cnt-1
//

/// The idle-key eviction interval in seconds (`inst-rl-scp-4`).
pub const DEFAULT_IDLE_EVICTION_SECS: u64 = 900;

/// One stored counter: its specification, its body, its phase, and the instant
/// it was last consulted.
struct CounterEntry {
    spec: CounterSpec,
    state: CounterState,
    phase: CounterPhase,
    last_used_nanos: u64,
}

impl CounterEntry {
    fn fresh(spec: &CounterSpec, reading: ClockReading) -> Self {
        Self {
            spec: spec.clone(),
            state: CounterState::fresh(spec, reading),
            phase: CounterPhase::Active,
            last_used_nanos: reading.monotonic_nanos,
        }
    }

    /// Run one check-and-deduct against the stored body, re-deriving the body
    /// first when the effective limit changed and the stored state no longer
    /// matches the configured capacity (`inst-rl-scp-7`/`-8`).
    fn decide(&mut self, spec: &CounterSpec, reading: ClockReading) -> RateDecision {
        if self.spec != *spec {
            *self = Self::fresh(spec, reading);
        }
        self.last_used_nanos = reading.monotonic_nanos;
        let decision = self.state.check(spec, reading);
        self.phase = if decision.allowed {
            CounterPhase::Active
        } else {
            CounterPhase::Depleted
        };
        decision
    }
}

/// The in-memory counter registry the data plane enforces its rate limits
/// through.
pub struct RateLimiterRegistry {
    counters: DashMap<String, Mutex<CounterEntry>>,
    clock: Arc<dyn RateClock>,
    max_counters: usize,
    idle_eviction_nanos: u64,
    /// The counters that were evicted while still live, i.e. before their idle
    /// interval expired: the only eviction that resets a budget, and therefore
    /// the one a consumer of the limiter needs to see.
    live_evictions: std::sync::atomic::AtomicU64,
}

/// The fraction of the bound `make_room` evicts in one pass, so the O(n)
/// least-recently-used scan is amortized over a batch instead of paid on every
/// miss once the registry sits at capacity.
const EVICTION_BATCH_FRACTION: usize = 10;

impl std::fmt::Debug for RateLimiterRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RateLimiterRegistry")
            .field("counters", &self.counters.len())
            .field("max_counters", &self.max_counters)
            .field("idle_eviction_secs", &(self.idle_eviction_nanos / 1_000_000_000))
            .finish()
    }
}

impl RateLimiterRegistry {
    // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-1
    // `inst-rl-scp-1` .. `-10`: the registry owns the counter space, the bound
    // and the eviction; the release of a resource's counters is delivered here
    // and invoked by the Data Plane flush, whose mechanism entry 2.9 owns.
    /// A registry with the documented bound and eviction interval.
    #[must_use]
    pub fn new(clock: Arc<dyn RateClock>) -> Self {
        Self::with_bounds(clock, DEFAULT_MAX_COUNTERS, DEFAULT_IDLE_EVICTION_SECS)
    }

    /// A registry with an explicit bound and eviction interval, for the tests
    /// that drive both.
    // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-4
    // `inst-rl-scp-4`: every counter is held in memory in this registry, with
    // no cross-instance coordination and no persisted state, under the explicit
    // bound and the idle-eviction interval the caller passes.
    #[must_use]
    pub fn with_bounds(
        clock: Arc<dyn RateClock>,
        max_counters: usize,
        idle_eviction_secs: u64,
    ) -> Self {
        Self {
            counters: DashMap::new(),
            clock,
            max_counters: max_counters.max(1),
            idle_eviction_nanos: idle_eviction_secs.saturating_mul(1_000_000_000),
            live_evictions: std::sync::atomic::AtomicU64::new(0),
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-4

    /// Attempt to acquire `spec.cost` units from the counter of `key`
    /// (`inst-rl-chk-6`).
    ///
    /// A key with no stored counter is created lazily at the effective burst
    /// capacity, so the first request for a key is never refused for missing
    /// state (`inst-rl-st-cnt-1`). Only the insert path allocates the key
    /// string, so a request served by a stored counter allocates nothing
    /// beyond the one composite key the caller already built.
    #[must_use]
    pub fn acquire(&self, key: &str, spec: &CounterSpec) -> RateDecision {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-10
        // `inst-rl-scp-10`: the per-key lock is the unit of synchronization, so
        // the check-and-deduct of one counter is atomic and two concurrent
        // acquires of the same key are fully serialized.
        let reading = self.clock.now();
        if let Some(entry) = self.counters.get(key) {
            let mut guard = entry.lock();
            return guard.decide(spec, reading);
        }
        // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-10
        self.make_room(key, &reading);
        let slot = self
            .counters
            .entry(key.to_owned())
            .or_insert_with(|| Mutex::new(CounterEntry::fresh(spec, reading)));
        // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-9
        let mut guard = slot.lock();
        guard.decide(spec, reading)
        // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-9
    }

    /// The phase the counter of `key` sits in, and `None` when the registry
    /// holds no counter for it.
    #[must_use]
    pub fn phase_of(&self, key: &str) -> Option<CounterPhase> {
        self.counters.get(key).map(|entry| entry.lock().phase)
    }

    /// The number of counters the registry holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.counters.len()
    }

    /// Whether the registry holds no counter at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    /// Drop every counter whose key is not consulted anymore, and report how
    /// many were dropped (`inst-rl-scp-4`). An evicted key is dropped silently.
    pub fn sweep(&self) -> usize {
        let now = self.clock.now().monotonic_nanos;
        let idle = self.idle_eviction_nanos;
        let before = self.counters.len();
        self.counters.retain(|_key, entry| {
            now.saturating_sub(entry.lock().last_used_nanos) < idle
        });
        before - self.counters.len()
    }

    /// Drop every counter keyed under one owning resource, so no stale budget
    /// survives its deletion or a change of its effective limit
    /// (`inst-rl-scp-7`/`-8`, `inst-rl-st-cnt-5` .. `-7`).
    pub fn release_resource(&self, resource: &RateLimitResource) -> usize {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-7
        // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-8
        let prefix = resource.prefix();
        let before = self.counters.len();
        self.counters.retain(|key, _entry| !key.starts_with(&prefix));
        before - self.counters.len()
        // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-8
        // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-7
    }

    /// Make room for the counter of `key`: first drop the idle ones, then evict
    /// the least recently used counters of the *same resource* down to a
    /// low-water mark, and only then the global least recently used ones.
    ///
    /// A counter key is `{resource}|{identity}`, so scoping the first eviction
    /// pass to the resource of the key being inserted keeps a caller that
    /// churns its own key surface from displacing the counters of any other
    /// resource — the budget it resets is always one its own requests draw
    /// from. Evicting in a batch down to the low-water mark also amortizes the
    /// scan over that batch instead of paying it on every miss
    /// (`inst-rl-scp-4`).
    fn make_room(&self, key: &str, reading: &ClockReading) {
        if self.counters.len() < self.max_counters {
            return;
        }
        self.sweep();
        if self.counters.len() < self.max_counters {
            return;
        }
        let target = self
            .max_counters
            .saturating_sub((self.max_counters / EVICTION_BATCH_FRACTION).max(1));
        let resource = key.split('|').next().map(|component| format!("{component}|"));
        if let Some(prefix) = resource {
            self.evict_oldest(Some(&prefix), target, reading);
        }
        if self.counters.len() > target {
            self.evict_oldest(None, target, reading);
        }
    }

    /// Evict the least recently used counters until the registry holds at most
    /// `target`, restricted to the keys starting with `prefix` when one is
    /// given. Returns how many live counters were evicted.
    fn evict_oldest(&self, prefix: Option<&str>, target: usize, reading: &ClockReading) -> usize {
        let now = reading.monotonic_nanos;
        let mut victims: Vec<(String, u64)> = Vec::new();
        for slot in self.counters.iter() {
            if prefix.is_some_and(|prefix| !slot.key().starts_with(prefix)) {
                continue;
            }
            let last_used = slot.value().lock().last_used_nanos;
            victims.push((slot.key().clone(), now.saturating_sub(last_used)));
        }
        victims.sort_by(|left, right| right.1.cmp(&left.1));
        let mut evicted = 0usize;
        for (key, _) in victims {
            if self.counters.len() <= target {
                break;
            }
            if self.counters.remove(&key).is_some() {
                evicted += 1;
            }
        }
        if evicted > 0 {
            self.live_evictions.fetch_add(evicted as u64, std::sync::atomic::Ordering::Relaxed);
        }
        evicted
    }

    /// The counters evicted while still live, i.e. before their idle interval
    /// expired. A live eviction resets the budget of the key it dropped, so it
    /// is the one eviction a consumer of the limiter must be able to see.
    #[must_use]
    pub fn live_evictions(&self) -> u64 {
        self.live_evictions.load(std::sync::atomic::Ordering::Relaxed)
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limiting-counter-scope:p1:inst-rl-scp-1
}
