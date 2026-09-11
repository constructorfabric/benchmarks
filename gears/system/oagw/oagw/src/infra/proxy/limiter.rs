//! The per-instance limiter registry of the data plane
//! (`cpt-cf-oagw-feature-rate-limiting`).
//!
//! The registry is an in-memory map keyed by the counter scope key of
//! `cpt-cf-oagw-algo-scope-key`, owned by the data plane per
//! `cpt-cf-oagw-adr-state-management`: no persistence, no TTL, no invalidation
//! path and no cross-instance synchronisation, so the counter state is lost on
//! a process restart and every bucket of a new process begins `Cold`. Entry
//! cardinality is unbounded for the `ip` and `user` scopes and the map grows
//! with distinct keys for the life of the process, with no eviction policy this
//! release: an entry orphaned by a deletion stays in the map and merely stops
//! being resolved.
//!
//! The map is spread over a fixed number of independently locked shards chosen
//! by the hash of the counter key, so the check of one request takes the write
//! lock of the single shard its key resolves into and leaves every other
//! tenant's, subject's, peer's and route's check free to run beside it: no
//! request serialises behind a process-wide registry lock, which is the "no
//! lock beyond the bucket's own entry" cost `cpt-cf-oagw-nfr-low-latency`
//! states for the check. The shards are an implementation detail of the
//! registry: cardinality, growth and the absence of an eviction policy are
//! unchanged by it, and a key always resolves into the same shard for the life
//! of the process.
//!
//! The decision itself is the pure core of `crate::domain::ratelimit`; this
//! module owns the lookup, the `Cold` → `Active` creation and the live states
//! of one entry, and holds no state outside its entries.

// @cpt-begin:cpt-cf-oagw-dod-rate-limit-enforcement:p1:inst-full

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use toolkit_security::SecurityContext;

use crate::domain::ratelimit::{
    EffectiveRateLimit, EnforcementPoint, RateLimitOutcome, ScopeContext, TokenBucket, decide,
    resolve_scope_key,
};
use crate::domain::resolution::EffectiveConfig;

// @cpt-begin:cpt-cf-oagw-dod-scope-key-determinism:p1:inst-full
/// The live states of one bucket (`cpt-cf-oagw-state-bucket-state`).
///
/// `Cold` is deliberately not a variant: it is the state of an ABSENT entry
/// and is not a bucket at all, so the registry represents it as
/// `Option<LimiterEntry>` — a key miss — and only the `Cold` → `Active`
/// transition of `inst-bs-01` puts an entry into the map, every other event
/// leaving an absent entry absent (`inst-bs-02`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketState {
    /// The bucket holds at least the cost of a request.
    Active,
    /// The last check against the key found the refilled balance below the
    /// cost.
    Exhausted,
}

// @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-02
// The `Cold` → `Cold` self-loop: `Cold` is the state of an ABSENT entry, so it
// is unchanged by a lookup that misses and by every other request outcome, and
// only `cold_to_active` puts an entry into the map. The registry expresses the
// state as the absence of the key, so the self-loop costs nothing to take.
// @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-02

/// One bucket of the registry together with the state of
/// `cpt-cf-oagw-state-bucket-state`.
///
/// The entry is the only state the machine holds: there is no state outside
/// the entry, no persistence behind it and no transition other than the eight
/// of §4, any other one leaving the entry unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct LimiterEntry {
    /// The bucket, which carries the window boundary the fixed-window
    /// approximation of FEATURE §1.5 aligns on.
    pub bucket: TokenBucket,
    /// The live state of the bucket.
    pub state: BucketState,
}

impl LimiterEntry {
    /// The `Cold` → `Active` transition (`cpt-cf-oagw-state-bucket-state`):
    /// a request for a key the registry has never seen creates the bucket
    /// full.
    ///
    /// `tokens` starts at `burst.capacity` — the default `sustained.rate` when
    /// the block does not configure a burst allowance — and `last_update` and
    /// the aligned window boundary start at `now`, so the first request a
    /// scope issues is charged against a bucket it did not inherit and is
    /// never refused by state this feature invented.
    #[must_use]
    pub fn cold_to_active(limit: &EffectiveRateLimit, now: Instant) -> Self {
        // @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-01
        // The only transition that creates an entry: a request for a key the
        // registry has never seen puts a full bucket in the map.
        // @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-01
        let capacity = limit.burst_tokens();
        Self {
            bucket: TokenBucket {
                tokens: capacity,
                last_update: now,
                capacity,
                refill_rate: limit.refill_rate(),
                window_start: now,
            },
            state: BucketState::Active,
        }
    }
}

/// The number of independently locked maps the registry spreads its entries
/// over. The check of one request locks the one shard its counter key hashes
/// into, so the buckets of distinct keys — distinct tenants, subjects, peers,
/// routes and upstreams — never queue behind a single process-wide lock, which
/// is the "no lock beyond the bucket's own entry" cost `cpt-cf-oagw-nfr-low-latency`
/// allocates to the check.
const SHARD_COUNT: usize = 64;

/// The index of the shard the counter key belongs to.
///
/// The mapping is total — every key resolves into exactly one of the
/// `SHARD_COUNT` shards — and deterministic for the life of the process, so a
/// key is always resolved into the same shard and therefore into the same
/// bucket, whatever the order the requests that carry it arrive in.
fn shard_index(key: &str) -> usize {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % SHARD_COUNT
}

/// The sharded entry map of the registry.
///
/// The entries are spread over a fixed array of independently locked maps,
/// chosen by the hash of the counter key, so the registry is never behind one
/// lock: a check takes the write lock of the single shard its key resolves to
/// and holds nothing else, and the growth and cardinality of the registry are
/// the sum of its shards exactly as they were the sum of the one map before it.
#[derive(Debug)]
pub(crate) struct ShardedEntries {
    shards: Vec<RwLock<HashMap<String, LimiterEntry>>>,
}

impl Default for ShardedEntries {
    fn default() -> Self {
        Self {
            shards: (0..SHARD_COUNT)
                .map(|_| RwLock::new(HashMap::new()))
                .collect(),
        }
    }
}

impl ShardedEntries {
    /// The write lock of the shard the key belongs to: the only lock one check
    /// takes, held across that key's lookup, `Cold` creation, refill and
    /// decision and no other key's.
    fn shard(&self, key: &str) -> parking_lot::RwLockWriteGuard<'_, HashMap<String, LimiterEntry>> {
        self.shards[shard_index(key)].write()
    }

    /// The merged read view over every shard, the seam the tests and the
    /// diagnostics read the registry through; the check itself never takes it.
    #[cfg(test)]
    fn read(&self) -> ReadView<'_> {
        ReadView {
            guards: self.shards.iter().map(|shard| shard.read()).collect(),
        }
    }

    /// The merged write view over every shard, the seam a test drives a single
    /// entry's state machine through.
    #[cfg(test)]
    fn write(&self) -> WriteView<'_> {
        WriteView {
            guards: self.shards.iter().map(|shard| shard.write()).collect(),
        }
    }
}

/// A merged read view over every shard of the registry.
///
/// It exists for the test and diagnostic seam only: the check path resolves its
/// key into one shard and holds that shard's lock alone.
#[cfg(test)]
struct ReadView<'a> {
    guards: Vec<parking_lot::RwLockReadGuard<'a, HashMap<String, LimiterEntry>>>,
}

#[cfg(test)]
impl ReadView<'_> {
    /// The entry of one key, wherever its shard holds it.
    fn get(&self, key: &str) -> Option<&LimiterEntry> {
        let index = shard_index(key);
        self.guards.get(index).and_then(|shard| shard.get(key))
    }

    /// Whether the registry holds an entry for the key.
    fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The entries the registry holds over all of its shards.
    fn len(&self) -> usize {
        self.guards.iter().map(|shard| shard.len()).sum()
    }

    /// Whether the registry holds no entry at all.
    fn is_empty(&self) -> bool {
        self.guards.iter().all(|shard| shard.is_empty())
    }
}

/// A merged write view over every shard of the registry, the seam a test drives
/// a single entry's state machine through.
#[cfg(test)]
struct WriteView<'a> {
    guards: Vec<parking_lot::RwLockWriteGuard<'a, HashMap<String, LimiterEntry>>>,
}

#[cfg(test)]
impl WriteView<'_> {
    /// The entry of one key, for in-place mutation of its bucket.
    fn get_mut(&mut self, key: &str) -> Option<&mut LimiterEntry> {
        let index = shard_index(key);
        self.guards
            .get_mut(index)
            .and_then(|shard| shard.get_mut(key))
    }
}

/// The per-instance limiter registry of the data plane.
///
/// Cloning the registry shares the entries, so the handles a gear holds stay
/// consistent with one another. An entry is created only when a request
/// resolves its key, and the map is keyed by that key alone: no persistence,
/// no TTL, no invalidation path, no eviction and no cross-instance sharing.
#[derive(Debug, Default, Clone)]
pub struct RateLimiter {
    entries: Arc<ShardedEntries>,
}

// @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-07
// The `Active` → `Cold` transition: the process restart that empties the map,
// so the next request for that key finds no entry and starts again from a full
// bucket, and the same fresh start a new key produces when a resource replaces
// a deleted one under a different `resource_id`. An entry orphaned by a
// deletion is never removed this release: it stays in the map and merely stops
// being resolved, per §1.5.
// @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-07

// @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-08
// The `Exhausted` → `Cold` transition: the same causes apply — the restart that
// empties the map, or the new key a replacement resource resolves — an
// exhausted balance never being remembered across a restart, and an orphaned
// entry keeping its exhausted balance in the map without ever being resolved
// again.
// @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-08
// @cpt-end:cpt-cf-oagw-dod-scope-key-determinism:p1:inst-full

impl RateLimiter {
    /// Creates the empty registry: a restart begins with every entry `Cold`.
    ///
    /// The construction inside the gear `init()` is the wiring of
    /// `cpt-cf-oagw-feature-proxy-pipeline`; the registry itself owns no
    /// route, no endpoint and no configuration of its own.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The per-request check of `cpt-cf-oagw-flow-rate-limit-check`.
    ///
    /// The caller is the proxy pipeline of
    /// `cpt-cf-oagw-feature-proxy-pipeline`, which invokes it after the auth
    /// plugin and before the guard and transform plugins, and which renders
    /// the outcome this check produces.
    pub async fn check(
        &self,
        effective: &EffectiveConfig,
        point: &EnforcementPoint,
        ctx: &ScopeContext,
    ) -> RateLimitOutcome {
        self.check_at(effective, point, ctx, Instant::now()).await
    }

    /// `check` with the monotonic instant injected: the same body, driven by
    /// the instant the caller supplies instead of the clock of the host.
    ///
    /// This is the test seam of the refill arithmetic, which is why it is
    /// public: the delays and the balances are functions of the injected
    /// instant alone, so a test drives them without sleeping.
    #[allow(clippy::unused_async)] // the seam keeps the signature `check` delegates to
    pub async fn check_at(
        &self,
        effective: &EffectiveConfig,
        point: &EnforcementPoint,
        ctx: &ScopeContext,
        now: Instant,
    ) -> RateLimitOutcome {
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-01
        // The caller hands over the resolved effective configuration — the
        // effective limit value `cpt-cf-oagw-algo-effective-merge` computed at
        // `inst-me-10` and the enforcement parameters of the merged block —
        // together with the enforcement point and the scope context of the
        // request. This feature consumes the limit as a value: it computes no
        // `min()`, walks no tenant chain and applies no sharing mode.
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-01
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-02
        // An ABSENT effective limit — no collected enforced ancestor limit, no
        // selected upstream limit and no route limit — is read as "no
        // limiting": no bucket is consulted, created or touched, no
        // `X-RateLimit-*` header is set and no counter is recorded. No tier's
        // configuration is invented to limit with.
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-02
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-03
        // An ABSENT limit is read as "no limiting" and returns success to the
        // caller without consulting, creating or touching any bucket, setting
        // no `X-RateLimit-*` header and recording no counter.
        let Some(block) = effective.rate_limit.as_ref() else {
            return RateLimitOutcome::NotLimited;
        };
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-03
        // A block that names no enforceable sustained rate is the absent limit
        // by another name: the merge cannot produce one, since it collects
        // only limits with a comparable sustained rate, and there is no limit
        // value to enforce, so the check invents no limit of its own.
        let Ok(limit) = EffectiveRateLimit::from_merged(block) else {
            return RateLimitOutcome::NotLimited;
        };
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-04
        // The counter scope key is resolved once per request that carries an
        // effective limit, from the `scope` of the merged block and the
        // request context.
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-04
        let key = resolve_scope_key(&limit, point, ctx);
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-05
        // One direct in-memory access keyed by that key: no cache layer, no
        // I/O and no cross-instance call.
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-05
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-06
        // The registry either holds the entry for the key or holds nothing at
        // all, which is the `Cold` state of the machine.
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-06
        let outcome = {
            // The one lock the check takes: the write lock of the shard this
            // key hashes into, so the checks of other keys — other tenants,
            // subjects, peers, routes and upstreams — run beside this one
            // instead of behind it, and the bucket's own entry is the only
            // state the lock guards.
            let mut entries = self.entries.shard(&key);
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-07
            // A key the registry has never seen starts from a full bucket, so
            // a first request is never refused by state this feature invented.
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-07
            let entry = entries
                .entry(key)
                .or_insert_with(|| LimiterEntry::cold_to_active(&limit, now));
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-08
            // The acquisition refills the bucket lazily on access and then
            // tests the refilled balance against the cost.
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-08
            let decision = decide(&mut entry.bucket, &limit, now);
            // @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-03
            // An acquisition that succeeds after the lazy refill leaves the
            // entry `Active`, the refill having topped the balance up to at
            // most `capacity` before the one subtraction.
            // @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-03
            // @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-04
            // An acquisition that fails leaves the balance exactly as the
            // refill left it, never negative and never partially spent.
            // @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-04
            // @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-05
            // A later check whose refill or boundary grant raises the balance
            // to at least the cost re-enters `Active` here: the same registry
            // entry refilled in place, with no new allocation and no
            // remembered debt.
            // @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-05
            // @cpt-begin:cpt-cf-oagw-state-bucket-state:p1:inst-bs-06
            // A later check that still finds the balance below the cost stays
            // `Exhausted`, each such refusal reporting the `Retry-After` it
            // computes from the balance it observes.
            // @cpt-end:cpt-cf-oagw-state-bucket-state:p1:inst-bs-06
            entry.state = if decision.acquired {
                BucketState::Active
            } else {
                BucketState::Exhausted
            };
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-09
            // The acquisition succeeded: the request continues with the guard
            // and transform phases and the upstream call, which this check
            // performs neither of.
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-09
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-10
            // The three rate-limit headers were set when `response_headers` is
            // true and none of them when it is false, by
            // `cpt-cf-oagw-algo-token-bucket`.
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-10
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-12
            // The bucket cannot pay the cost: the refilled balance is below it.
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-12
            // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-13
            // The refusal is rendered through the existing `RateLimitExceeded`
            // row of `cpt-cf-oagw-algo-error-mapping` — HTTP 429, GTS type
            // `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`,
            // retriable `Yes` — with the `Retry-After` delay the token bucket
            // computed and the same three headers when `response_headers` is
            // true. The implemented strategy is `reject`, and `queue` and
            // `degrade` resolve to this same outcome. No row is added to the
            // closed table and no HTTP response is rendered here.
            // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-13
            if decision.acquired {
                // RETURN success to the caller, which continues with the guard
                // and transform phases and the upstream call.
                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-11
                RateLimitOutcome::Admitted {
                    headers: decision.headers,
                }
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-11
            } else {
                // RETURN the refusal to the caller for rendering, with no guard
                // or transform phase after the check and no upstream call.
                // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-14
                RateLimitOutcome::Refused {
                    retry_after_secs: decision.retry_after_secs,
                    headers: decision.headers,
                }
                // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-14
            }
        };
        // @cpt-begin:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-15
        // Every path above performed the whole check in memory and without
        // I/O — at most one registry access, one lazy refill and one
        // comparison, with one subtraction only on the acquired path — so the
        // check adds no I/O to the proxy path and leaves the counter state
        // this request produced available to the next request that resolves
        // the same key.
        // @cpt-end:cpt-cf-oagw-flow-rate-limit-check:p1:inst-rl-15
        outcome
    }

    /// The check the proxy pipeline calls with the context it already holds:
    /// the two subject dimensions are extracted from the security context
    /// here, so the domain core never imports the security types.
    pub async fn check_security(
        &self,
        effective: &EffectiveConfig,
        point: &EnforcementPoint,
        security: &SecurityContext,
        peer: IpAddr,
    ) -> RateLimitOutcome {
        let ctx = ScopeContext {
            subject_tenant_id: security.subject_tenant_id(),
            subject_id: security.subject_id(),
            peer,
        };
        self.check(effective, point, &ctx).await
    }
}

// @cpt-end:cpt-cf-oagw-dod-rate-limit-enforcement:p1:inst-full

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::*;
    use crate::domain::model::{
        ALGORITHM_SLIDING_WINDOW, RateLimitConfig, Route, SCOPE_GLOBAL, SCOPE_ROUTE, SCOPE_USER,
        STRATEGY_DEGRADE, STRATEGY_QUEUE, STRATEGY_REJECT, SustainedRate, WINDOW_MINUTE,
        WINDOW_SECOND,
    };
    use crate::domain::ratelimit::{
        HEADER_LIMIT, HEADER_REMAINING, HEADER_RESET, RateLimitHeaders,
    };
    use crate::domain::resolution::EffectiveConfig;
    use uuid::Uuid;

    const UPSTREAM: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0001);
    const OTHER_UPSTREAM: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0002);
    const ROUTE: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0003);
    const TENANT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0004);
    const OTHER_TENANT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0005);
    const SUBJECT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0006);
    const OTHER_SUBJECT: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0007);
    const THIRD_UPSTREAM: Uuid = Uuid::from_u128(0x0a11_ce5a_0000_0008);
    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));

    /// The instant every test anchors its durations on: the registry itself
    /// never reads the clock, so the injected instant is the only time the
    /// arithmetic knows.
    fn start() -> Instant {
        Instant::now()
    }

    /// A block whose every optional field is omitted.
    fn block(rate: i64, window: &str) -> RateLimitConfig {
        RateLimitConfig {
            sustained: Some(SustainedRate {
                rate: Some(rate),
                window: window.to_owned(),
            }),
            ..RateLimitConfig::default()
        }
    }

    fn effective(block: Option<RateLimitConfig>) -> EffectiveConfig {
        EffectiveConfig {
            auth: None,
            auth_forced: false,
            rate_limit: block,
            plugins: Vec::new(),
            cors: None,
            cors_forced: false,
            tags: BTreeSet::new(),
        }
    }

    fn point(upstream: Uuid) -> EnforcementPoint {
        EnforcementPoint {
            upstream_id: upstream,
            matched_route: None,
        }
    }

    fn route(id: Uuid, upstream: Uuid, block: Option<RateLimitConfig>) -> Route {
        Route {
            id: Some(id),
            upstream_id: Some(upstream),
            rate_limit: block,
            ..Route::default()
        }
    }

    fn ctx(tenant: Uuid) -> ScopeContext {
        ScopeContext {
            subject_tenant_id: tenant,
            subject_id: SUBJECT,
            peer: PEER,
        }
    }

    fn key_of(upstream: Uuid, scope: &str, window: &str) -> String {
        format!("oagw:ratelimit:upstream:{upstream}:{scope}:{TENANT}:{window}")
    }

    async fn check(
        limiter: &RateLimiter,
        configured: &RateLimitConfig,
        point: &EnforcementPoint,
        tenant: Uuid,
        now: Instant,
    ) -> RateLimitOutcome {
        limiter
            .check_at(
                &effective(Some(configured.clone())),
                point,
                &ctx(tenant),
                now,
            )
            .await
    }

    async fn admitted(limiter: &RateLimiter, upstream: Uuid, now: Instant) -> RateLimitHeaders {
        let outcome = limiter
            .check_at(
                &effective(Some(block(10, WINDOW_SECOND))),
                &point(upstream),
                &ctx(TENANT),
                now,
            )
            .await;
        match outcome {
            RateLimitOutcome::Admitted { headers } => {
                headers.expect("the ADR default sets the headers")
            }
            other => panic!("expected an admission, got {other:?}"),
        }
    }

    async fn refused(
        limiter: &RateLimiter,
        upstream: Uuid,
        now: Instant,
    ) -> (u64, Option<RateLimitHeaders>) {
        let outcome = limiter
            .check_at(
                &effective(Some(block(10, WINDOW_SECOND))),
                &point(upstream),
                &ctx(TENANT),
                now,
            )
            .await;
        match outcome {
            RateLimitOutcome::Refused {
                retry_after_secs,
                headers,
            } => (retry_after_secs, headers),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_absent_effective_limit_proceeds_without_touching_the_registry() {
        let limiter = RateLimiter::new();
        let outcome = limiter
            .check_at(&effective(None), &point(UPSTREAM), &ctx(TENANT), start())
            .await;
        assert_eq!(outcome, RateLimitOutcome::NotLimited);
        assert!(limiter.entries.read().is_empty(), "no bucket was created");
    }

    #[tokio::test]
    async fn an_unenforceable_block_is_read_as_no_limiting() {
        let limiter = RateLimiter::new();
        let absent = RateLimitConfig {
            sustained: None,
            ..RateLimitConfig::default()
        };
        let outcome = limiter
            .check_at(
                &effective(Some(absent)),
                &point(UPSTREAM),
                &ctx(TENANT),
                start(),
            )
            .await;
        assert_eq!(outcome, RateLimitOutcome::NotLimited);
        assert!(limiter.entries.read().is_empty());
    }

    #[tokio::test]
    async fn the_first_request_for_a_key_is_charged_against_a_full_bucket() {
        let limiter = RateLimiter::new();
        let now = start();
        let headers = admitted(&limiter, UPSTREAM, now).await;
        assert_eq!(headers.limit, 10);
        assert_eq!(headers.remaining, 9);
        let entry = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .cloned();
        let entry = entry.expect("the check created the entry");
        assert_eq!(entry.state, BucketState::Active);
    }

    #[tokio::test]
    async fn an_admitted_response_carries_the_three_headers() {
        let limiter = RateLimiter::new();
        let now = start();
        let headers = admitted(&limiter, UPSTREAM, now).await;
        let pairs = crate::domain::ratelimit::header_pairs(&headers);
        assert_eq!(pairs[0].0, HEADER_LIMIT);
        assert_eq!(pairs[0].1, "10");
        assert_eq!(pairs[1].0, HEADER_REMAINING);
        assert_eq!(pairs[1].1, "9");
        assert_eq!(pairs[2].0, HEADER_RESET);
        assert_eq!(pairs[2].1, "1");
    }

    #[tokio::test]
    async fn an_exhausted_bucket_is_refused_with_retry_after_and_the_three_headers() {
        let limiter = RateLimiter::new();
        let now = start();
        for _ in 0..10 {
            admitted(&limiter, UPSTREAM, now).await;
        }
        let (retry_after, headers) = refused(&limiter, UPSTREAM, now).await;
        assert_eq!(retry_after, 1, "one second at 10 tokens per second");
        let headers = headers.expect("the ADR default sets the headers");
        assert_eq!(headers.limit, 10);
        assert_eq!(headers.remaining, 0, "the tokens the bucket still holds");
        let entry = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .cloned();
        assert_eq!(
            entry.expect("the entry stays").state,
            BucketState::Exhausted
        );
    }

    #[tokio::test]
    async fn the_retry_after_is_never_later_than_the_reset_header_of_a_refusal() {
        let limiter = RateLimiter::new();
        let now = start();
        for _ in 0..10 {
            admitted(&limiter, UPSTREAM, now).await;
        }
        let (retry_after, headers) = refused(&limiter, UPSTREAM, now).await;
        assert!(retry_after <= headers.expect("the headers").reset);
    }

    #[tokio::test]
    async fn a_refusal_reports_the_retry_after_it_computes_from_the_balance_it_observes() {
        let limiter = RateLimiter::new();
        let now = start();
        let configured = block(1, WINDOW_MINUTE);
        let point = point(UPSTREAM);
        let outcome = check(&limiter, &configured, &point, TENANT, now).await;
        assert!(matches!(outcome, RateLimitOutcome::Admitted { .. }));
        let first = check(&limiter, &configured, &point, TENANT, now).await;
        let second = check(
            &limiter,
            &configured,
            &point,
            TENANT,
            now + Duration::from_secs(10),
        )
        .await;
        let first = match first {
            RateLimitOutcome::Refused {
                retry_after_secs, ..
            } => retry_after_secs,
            other => panic!("expected a refusal, got {other:?}"),
        };
        let second = match second {
            RateLimitOutcome::Refused {
                retry_after_secs, ..
            } => retry_after_secs,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert_eq!(first, 60, "the delay the empty balance implies");
        assert_eq!(
            second, 50,
            "the delay the balance the later check observed implies"
        );
        let entry = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_MINUTE))
            .cloned();
        assert_eq!(
            entry.expect("the entry stays").state,
            BucketState::Exhausted
        );
    }

    #[tokio::test]
    async fn a_refilled_bucket_returns_to_active() {
        let limiter = RateLimiter::new();
        let now = start();
        for _ in 0..10 {
            admitted(&limiter, UPSTREAM, now).await;
        }
        assert_eq!(
            refused(&limiter, UPSTREAM, now).await.0,
            1,
            "the entry is Exhausted"
        );
        // Half a second later the refill has raised the balance to 5 tokens,
        // which pays the cost and re-enters Active in the same entry.
        let headers = admitted(&limiter, UPSTREAM, now + Duration::from_millis(500)).await;
        assert_eq!(headers.remaining, 4);
        let entry = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .cloned();
        let entry = entry.expect("no new allocation was made");
        assert_eq!(entry.state, BucketState::Active);
        assert!(limiter.entries.read().len() == 1, "one entry for one key");
    }

    #[tokio::test]
    async fn the_adr_level_false_branch_still_refuses_and_sets_no_header() {
        let limiter = RateLimiter::new();
        let now = start();
        let configured = block(10, WINDOW_SECOND);
        let mut effective_block =
            crate::domain::ratelimit::EffectiveRateLimit::from_merged(&configured)
                .expect("the fixture carries a sustained rate");
        effective_block.response_headers = false;
        // The registry reads the block, so the switch-off is exercised through
        // the decision core: the ADR-level branch is unreachable from a
        // payload of this release.
        let outcome = limiter
            .check_at(
                &effective(Some(configured)),
                &point(UPSTREAM),
                &ctx(TENANT),
                now,
            )
            .await;
        assert!(matches!(
            outcome,
            RateLimitOutcome::Admitted { headers: Some(_) }
        ));
        // ... and the pure branch itself:
        let mut entries = limiter.entries.write();
        let entry = entries
            .get_mut(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .expect("the entry");
        entry.bucket.tokens = 0.0;
        let decision = crate::domain::ratelimit::decide(&mut entry.bucket, &effective_block, now);
        assert!(!decision.acquired);
        assert!(decision.headers.is_none());
        assert!(decision.retry_after_secs > 0);
    }

    #[tokio::test]
    async fn a_sliding_window_block_is_enforced_through_the_same_registry() {
        let limiter = RateLimiter::new();
        let now = start();
        let mut configured = block(10, WINDOW_MINUTE);
        configured.algorithm = ALGORITHM_SLIDING_WINDOW.to_owned();
        let point = point(UPSTREAM);
        for _ in 0..10 {
            let outcome = limiter
                .check_at(
                    &effective(Some(configured.clone())),
                    &point,
                    &ctx(TENANT),
                    now,
                )
                .await;
            assert!(matches!(outcome, RateLimitOutcome::Admitted { .. }));
        }
        let outcome = limiter
            .check_at(&effective(Some(configured)), &point, &ctx(TENANT), now)
            .await;
        match outcome {
            RateLimitOutcome::Refused {
                retry_after_secs,
                headers,
            } => {
                assert!(retry_after_secs > 0);
                assert_eq!(headers.expect("the headers").remaining, 0);
            }
            other => panic!("expected the same 429-shaped refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_check_performs_one_subtraction_only_on_the_acquired_path() {
        let limiter = RateLimiter::new();
        let now = start();
        admitted(&limiter, UPSTREAM, now).await;
        let entry = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .cloned();
        let entry = entry.expect("the entry");
        assert_eq!(
            entry.bucket.tokens, 9.0,
            "the balance minus exactly one cost"
        );
        assert_eq!(entry.bucket.last_update, now);
    }

    #[tokio::test]
    async fn a_check_that_limits_nothing_leaves_the_entry_unchanged() {
        let limiter = RateLimiter::new();
        let now = start();
        admitted(&limiter, UPSTREAM, now).await;
        let before = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .cloned();
        for _ in 0..3 {
            let outcome = limiter
                .check_at(&effective(None), &point(UPSTREAM), &ctx(TENANT), now)
                .await;
            assert_eq!(outcome, RateLimitOutcome::NotLimited);
        }
        let after = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .cloned();
        assert_eq!(before, after, "no transition other than the eight listed");
    }

    #[tokio::test]
    async fn a_restarted_registry_remembers_nothing() {
        let limiter = RateLimiter::new();
        let now = start();
        for _ in 0..10 {
            admitted(&limiter, UPSTREAM, now).await;
        }
        assert_eq!(limiter.entries.read().len(), 1);
        // A restart is a new registry: no balance, no exhaustion and no debt.
        let restarted = RateLimiter::new();
        assert!(
            restarted.entries.read().is_empty(),
            "every entry of a new process begins Cold"
        );
        let headers = admitted(&restarted, UPSTREAM, now).await;
        assert_eq!(headers.remaining, 9, "the first request starts full again");
    }

    #[tokio::test]
    async fn a_replacement_resource_resolves_a_new_key_and_starts_cold() {
        let limiter = RateLimiter::new();
        let now = start();
        for _ in 0..10 {
            admitted(&limiter, UPSTREAM, now).await;
        }
        let headers = admitted(&limiter, OTHER_UPSTREAM, now).await;
        assert_eq!(headers.remaining, 9, "a new resource id resolves a new key");
        assert_eq!(limiter.entries.read().len(), 2);
    }

    #[tokio::test]
    async fn an_entry_orphaned_by_a_deletion_is_never_removed() {
        let limiter = RateLimiter::new();
        let now = start();
        admitted(&limiter, UPSTREAM, now).await;
        // The resource is deleted, so its key stops being resolved; the entry
        // stays in the map unresolved, there being no eviction policy.
        let orphaned = limiter
            .entries
            .read()
            .get(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
            .cloned();
        assert!(orphaned.is_some());
        admitted(&limiter, OTHER_UPSTREAM, now).await;
        assert!(
            limiter
                .entries
                .read()
                .contains_key(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
        );
    }

    #[tokio::test]
    async fn the_registry_grows_with_distinct_keys_and_never_evicts() {
        let limiter = RateLimiter::new();
        let now = start();
        for index in 0..5 {
            let upstream = Uuid::from_u128(index);
            admitted(&limiter, upstream, now).await;
        }
        assert_eq!(limiter.entries.read().len(), 5, "unbounded cardinality");
    }

    #[tokio::test]
    async fn two_tenants_resolve_two_entries_under_the_tenant_scope() {
        let limiter = RateLimiter::new();
        let now = start();
        admitted(&limiter, UPSTREAM, now).await;
        let other = ScopeContext {
            subject_tenant_id: OTHER_TENANT,
            ..ctx(TENANT)
        };
        let outcome = limiter
            .check_at(
                &effective(Some(block(10, WINDOW_SECOND))),
                &point(UPSTREAM),
                &other,
                now,
            )
            .await;
        assert!(matches!(outcome, RateLimitOutcome::Admitted { .. }));
        assert_eq!(limiter.entries.read().len(), 2, "one bucket per tenant");
    }

    #[tokio::test]
    async fn two_peers_resolve_two_entries_under_the_ip_scope() {
        let limiter = RateLimiter::new();
        let now = start();
        let mut configured = block(10, WINDOW_SECOND);
        configured.scope = "ip".to_owned();
        let point = point(UPSTREAM);
        limiter
            .check_at(
                &effective(Some(configured.clone())),
                &point,
                &ctx(TENANT),
                now,
            )
            .await;
        let other = ScopeContext {
            peer: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8)),
            ..ctx(TENANT)
        };
        limiter
            .check_at(&effective(Some(configured)), &point, &other, now)
            .await;
        assert_eq!(limiter.entries.read().len(), 2, "one bucket per address");
    }

    #[tokio::test]
    async fn the_route_s_own_block_is_counted_under_the_route_resource() {
        let limiter = RateLimiter::new();
        let now = start();
        let route_block = block(10, WINDOW_SECOND);
        let route = route(ROUTE, UPSTREAM, Some(route_block.clone()));
        let point = EnforcementPoint {
            upstream_id: UPSTREAM,
            matched_route: Some(route),
        };
        limiter
            .check_at(&effective(Some(route_block)), &point, &ctx(TENANT), now)
            .await;
        let expected = format!("oagw:ratelimit:route:{ROUTE}:tenant:{TENANT}:{WINDOW_SECOND}");
        assert!(limiter.entries.read().contains_key(&expected));
    }

    #[tokio::test]
    async fn the_upstream_block_is_counted_under_the_upstream_resource() {
        let limiter = RateLimiter::new();
        let now = start();
        let upstream_block = block(10, WINDOW_SECOND);
        let route_block = block(50, WINDOW_SECOND);
        let route = route(ROUTE, UPSTREAM, Some(route_block));
        let point = EnforcementPoint {
            upstream_id: UPSTREAM,
            matched_route: Some(route),
        };
        limiter
            .check_at(&effective(Some(upstream_block)), &point, &ctx(TENANT), now)
            .await;
        let expected =
            format!("oagw:ratelimit:upstream:{UPSTREAM}:tenant:{TENANT}:{WINDOW_SECOND}");
        assert!(limiter.entries.read().contains_key(&expected));
    }

    #[tokio::test]
    async fn two_limits_differing_only_in_window_resolve_two_entries() {
        let limiter = RateLimiter::new();
        let now = start();
        let per_second = block(10, WINDOW_SECOND);
        let per_minute = block(10, WINDOW_MINUTE);
        limiter
            .check_at(
                &effective(Some(per_second)),
                &point(UPSTREAM),
                &ctx(TENANT),
                now,
            )
            .await;
        limiter
            .check_at(
                &effective(Some(per_minute)),
                &point(UPSTREAM),
                &ctx(TENANT),
                now,
            )
            .await;
        assert_eq!(
            limiter.entries.read().len(),
            2,
            "the window is a key component"
        );
    }

    #[tokio::test]
    async fn the_security_convenience_resolves_the_same_key_as_the_scope_context() {
        let limiter = RateLimiter::new();
        let security = toolkit_security::SecurityContext::builder()
            .subject_id(SUBJECT)
            .subject_tenant_id(TENANT)
            .build()
            .expect("a test context carries a subject and a tenant");
        limiter
            .check_security(
                &effective(Some(block(10, WINDOW_SECOND))),
                &point(UPSTREAM),
                &security,
                PEER,
            )
            .await;
        assert_eq!(limiter.entries.read().len(), 1);
        assert!(
            limiter
                .entries
                .read()
                .contains_key(&key_of(UPSTREAM, "tenant", WINDOW_SECOND))
        );
        let outcome = limiter
            .check(
                &effective(Some(block(10, WINDOW_SECOND))),
                &point(UPSTREAM),
                &ScopeContext {
                    subject_tenant_id: TENANT,
                    subject_id: SUBJECT,
                    peer: PEER,
                },
            )
            .await;
        assert!(
            matches!(outcome, RateLimitOutcome::Admitted { .. }),
            "the same bucket, reached through both seams"
        );
    }

    /// The accepted strategy values resolve to the one behaviour this release
    /// implements: a `queue` and a `degrade` block refuse exactly as the
    /// `reject` block does, with the same `Retry-After` and the same headers,
    /// and no queued or reduced-functionality outcome exists to produce.
    #[tokio::test]
    async fn a_queue_and_a_degrade_block_refuse_exactly_as_the_reject_one() {
        let limiter = RateLimiter::new();
        let now = start();
        let mut refusals = Vec::new();
        for (strategy, upstream) in [
            (STRATEGY_REJECT, UPSTREAM),
            (STRATEGY_QUEUE, OTHER_UPSTREAM),
            (STRATEGY_DEGRADE, THIRD_UPSTREAM),
        ] {
            let mut configured = block(10, WINDOW_SECOND);
            configured.strategy = strategy.to_owned();
            let point = point(upstream);
            for _ in 0..10 {
                let outcome = check(&limiter, &configured, &point, TENANT, now).await;
                assert!(
                    matches!(outcome, RateLimitOutcome::Admitted { .. }),
                    "{strategy} admits the first ten requests"
                );
            }
            let outcome = check(&limiter, &configured, &point, TENANT, now).await;
            match outcome {
                RateLimitOutcome::Refused {
                    retry_after_secs,
                    headers,
                } => refusals.push((strategy, retry_after_secs, headers)),
                other => panic!("expected the 429-shaped refusal of {strategy}, got {other:?}"),
            }
        }

        let (reject, retry_after, headers) = &refusals[0];
        assert_eq!(*reject, STRATEGY_REJECT);
        assert!(*retry_after > 0, "the refusal carries a delay");
        assert!(
            headers.is_some(),
            "the ADR default sets the three rate-limit headers"
        );
        for (strategy, other_retry_after, other_headers) in &refusals[1..] {
            assert_eq!(
                other_retry_after, retry_after,
                "{strategy} refuses with the delay {reject} refuses with"
            );
            assert_eq!(
                other_headers, headers,
                "{strategy} refuses with the headers {reject} refuses with"
            );
        }
    }

    #[tokio::test]
    async fn two_subjects_of_one_tenant_resolve_two_entries_under_the_user_scope() {
        let limiter = RateLimiter::new();
        let now = start();
        let mut configured = block(10, WINDOW_SECOND);
        configured.scope = SCOPE_USER.to_owned();
        let point = point(UPSTREAM);

        limiter
            .check_at(
                &effective(Some(configured.clone())),
                &point,
                &ctx(TENANT),
                now,
            )
            .await;
        let other_subject = ScopeContext {
            subject_id: OTHER_SUBJECT,
            ..ctx(TENANT)
        };
        limiter
            .check_at(
                &effective(Some(configured.clone())),
                &point,
                &other_subject,
                now,
            )
            .await;
        assert_eq!(
            limiter.entries.read().len(),
            2,
            "one bucket per subject of one tenant"
        );

        let recurring = ScopeContext {
            subject_tenant_id: OTHER_TENANT,
            ..ctx(TENANT)
        };
        limiter
            .check_at(&effective(Some(configured)), &point, &recurring, now)
            .await;
        assert_eq!(
            limiter.entries.read().len(),
            3,
            "a subject identifier that recurs in another tenant resolves another key"
        );
    }

    #[tokio::test]
    async fn every_subject_of_every_tenant_shares_one_entry_under_the_global_scope() {
        let limiter = RateLimiter::new();
        let now = start();
        let mut configured = block(1, WINDOW_SECOND);
        configured.scope = SCOPE_GLOBAL.to_owned();
        let point = point(UPSTREAM);

        let first = limiter
            .check_at(
                &effective(Some(configured.clone())),
                &point,
                &ctx(TENANT),
                now,
            )
            .await;
        assert!(
            matches!(first, RateLimitOutcome::Admitted { .. }),
            "the shared bucket pays the first request"
        );
        let other = ScopeContext {
            subject_tenant_id: OTHER_TENANT,
            subject_id: OTHER_SUBJECT,
            peer: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8)),
        };
        let second = limiter
            .check_at(&effective(Some(configured)), &point, &other, now)
            .await;
        assert!(
            matches!(second, RateLimitOutcome::Refused { .. }),
            "the counter another subject spent is the one this request is counted into"
        );
        assert_eq!(
            limiter.entries.read().len(),
            1,
            "one counter per enforcement point and window, shared by every subject and tenant"
        );
    }

    #[tokio::test]
    async fn two_routes_of_one_upstream_resolve_two_entries_under_the_route_scope() {
        let limiter = RateLimiter::new();
        let now = start();
        let mut configured = block(10, WINDOW_SECOND);
        configured.scope = SCOPE_ROUTE.to_owned();
        let first_route = Uuid::from_u128(0x0a11_ce5a_0000_00a1);
        let second_route = Uuid::from_u128(0x0a11_ce5a_0000_00a2);
        let routed = |route_id: Uuid| EnforcementPoint {
            upstream_id: UPSTREAM,
            matched_route: Some(route(route_id, UPSTREAM, None)),
        };

        limiter
            .check_at(
                &effective(Some(configured.clone())),
                &routed(first_route),
                &ctx(TENANT),
                now,
            )
            .await;
        limiter
            .check_at(
                &effective(Some(configured.clone())),
                &routed(second_route),
                &ctx(TENANT),
                now,
            )
            .await;
        // The upstream's own counter, the pair degenerating to the id, is a
        // counter of its own and is shared with neither route.
        limiter
            .check_at(
                &effective(Some(configured)),
                &point(UPSTREAM),
                &ctx(TENANT),
                now,
            )
            .await;

        let entries = limiter.entries.read();
        assert_eq!(
            entries.len(),
            3,
            "one bucket per route of one upstream, and one for the upstream itself"
        );
        for route_id in [first_route, second_route] {
            let expected = format!(
                "oagw:ratelimit:upstream:{UPSTREAM}:route:{UPSTREAM}/{route_id}:{WINDOW_SECOND}"
            );
            assert!(
                entries.contains_key(&expected),
                "route {route_id} is counted under the (upstream, route) pair"
            );
        }
        let own = format!("oagw:ratelimit:upstream:{UPSTREAM}:route:{UPSTREAM}:{WINDOW_SECOND}");
        assert!(
            entries.contains_key(&own),
            "the upstream's own counter is the pair without a route id"
        );
    }

    /// The registry holds no single lock the whole tenant set queues behind:
    /// two checks of two distinct keys run to completion beside one another,
    /// each holding only the write lock of the shard its key resolved into, and
    /// each counting into its own bucket.
    #[tokio::test]
    async fn two_distinct_keys_are_checked_beside_one_another_without_one_lock() {
        use std::sync::Arc as TestArc;

        let limiter = TestArc::new(RateLimiter::new());
        let now = start();

        let first = tokio::spawn({
            let limiter = TestArc::clone(&limiter);
            async move { admitted(&limiter, UPSTREAM, now).await }
        });
        let second = tokio::spawn({
            let limiter = TestArc::clone(&limiter);
            async move { admitted(&limiter, OTHER_UPSTREAM, now).await }
        });
        let (first, second) = (
            first.await.expect("the check ran to completion"),
            second.await.expect("the check ran to completion"),
        );

        assert_eq!(first.remaining, 9, "each key started from its own bucket");
        assert_eq!(second.remaining, 9);
        assert_eq!(
            limiter.entries.read().len(),
            2,
            "one bucket per key, none of them shared or lost"
        );
    }

    /// The shard selection the check relies on is deterministic — a counter key
    /// always resolves into the same shard, and therefore into the same bucket,
    /// whatever the order the requests that carry it arrive in — and total:
    /// every key resolves into one of the declared shards, and distinct keys
    /// spread over more than one of them.
    #[test]
    fn the_shard_selection_is_deterministic_and_total() {
        use std::collections::HashSet;

        for key in [
            key_of(UPSTREAM, "tenant", WINDOW_SECOND),
            format!("oagw:ratelimit:route:{ROUTE}:tenant:{TENANT}:{WINDOW_MINUTE}"),
            String::new(),
        ] {
            assert_eq!(
                shard_index(&key),
                shard_index(&key.clone()),
                "one key, one shard, for {key}"
            );
            assert!(
                shard_index(&key) < SHARD_COUNT,
                "the key resolves into a declared shard"
            );
        }

        let spread: HashSet<usize> = (0..256)
            .map(|index| shard_index(&format!("oagw:ratelimit:key:{index}")))
            .collect();
        assert!(
            spread.iter().all(|shard| *shard < SHARD_COUNT),
            "no key resolves outside the declared shards"
        );
        assert!(
            spread.len() > 1,
            "the keys spread over the shards instead of piling into one"
        );
    }
}
