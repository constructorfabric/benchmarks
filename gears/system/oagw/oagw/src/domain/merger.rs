//! Hierarchical merge of the rate-limit configuration
//! ([DESIGN.md](../../../docs/DESIGN.md) "Hierarchical Configuration",
//! [ADR-0003](../../../docs/ADR/0003-rate-limiting.md) "Inheritance").
//!
//! The proxy resolves the layers that apply to one request — the upstream, the
//! route that matched it and the tenant — and merges them into the single
//! [`EffectiveRateLimit`] the limiter enforces. Merging is a pure function over
//! the resolved resources: it reads the layers and returns the policy, touching
//! no state and consulting no store.
//!
//! Layers are ordered ancestor → descendant (`upstream < route < tenant`) and
//! the sharing mode declared on a layer decides how its configuration combines
//! with the next layer's:
//!
//! | Layer's `sharing` | Effective policy |
//! |---|---|
//! | `private` | The descendant's configuration when it declares one; a descendant that declares none keeps the layer's own, which stays in force for the resource it configures. |
//! | `inherit` | The layer's configuration, inherited by the descendant as-is. |
//! | `enforce` | `min(layer, descendant)` for every numeric field — the ancestor caps the descendant and is never bypassed. |
//!
//! Tags have no sharing mode: they always merge as an add-only union, and a
//! descendant cannot remove an inherited tag.

use crate::domain::model::{
    RateLimit, RateLimitAlgorithm, RateLimitScope, RateLimitStrategy, RateLimitSustained,
    RateLimitWindow, Route, SharingMode, Tag, Upstream,
};

/// ADR-0003 default of `rate_limit.response_headers`, a schema key the
/// `rate_limit` block of this build does not carry: the `X-RateLimit-*` headers
/// are emitted unless a configuration surface turns them off.
pub const DEFAULT_RESPONSE_HEADERS: bool = true;

/// Seconds in a `rate_limit.sustained.window` unit.
#[must_use]
pub const fn window_secs(window: RateLimitWindow) -> u64 {
    match window {
        RateLimitWindow::Second => 1,
        RateLimitWindow::Minute => 60,
        RateLimitWindow::Hour => 3_600,
        RateLimitWindow::Day => 86_400,
    }
}

/// One layer of the hierarchy: the tags and the `rate_limit` block of one
/// resolved resource.
///
/// The layers of a request are built in ancestor order, so `upstream` comes
/// before `route` and `route` before the tenant.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitLayer<'a> {
    tags: &'a [Tag],
    rate_limit: Option<&'a RateLimit>,
}

impl<'a> RateLimitLayer<'a> {
    /// Builds a layer from its tags and its `rate_limit` block, if any.
    #[must_use]
    pub fn new(tags: &'a [Tag], rate_limit: Option<&'a RateLimit>) -> Self {
        Self { tags, rate_limit }
    }

    /// The upstream layer.
    #[must_use]
    pub fn upstream(upstream: &'a Upstream) -> Self {
        Self::new(&upstream.tags, upstream.rate_limit.as_ref())
    }

    /// The route layer.
    #[must_use]
    pub fn route(route: &'a Route) -> Self {
        Self::new(&route.tags, route.rate_limit.as_ref())
    }

    /// The tenant layer, which this build carries no configuration for.
    #[must_use]
    pub const fn tenant() -> Self {
        Self {
            tags: &[],
            rate_limit: None,
        }
    }
}

/// The rate-limit configuration the proxy enforces, after the layers merged.
///
/// The burst capacity is resolved: an omitted `burst.capacity` means
/// `sustained.rate` (ADR-0003 "Configuration: Dual-Rate"), so the limiter never
/// has to re-apply that default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRateLimit {
    /// Sharing mode of the layer that produced the policy.
    pub sharing: SharingMode,
    /// Algorithm the limiter runs.
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate of the winning layer.
    pub sustained: RateLimitSustained,
    /// Burst capacity: `min` of the merged layers, `sustained.rate` by default.
    pub capacity: u32,
    /// Scope the counters are keyed by.
    pub scope: RateLimitScope,
    /// Behavior when the limit is exceeded.
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request.
    pub cost: u32,
    /// Whether the response carries the `X-RateLimit-*` headers.
    pub response_headers: bool,
    /// Union of the tags of every merged layer, ancestors first.
    pub tags: Vec<Tag>,
}

impl EffectiveRateLimit {
    /// The policy of a single `rate_limit` block.
    #[must_use]
    pub fn of(limit: &RateLimit) -> Self {
        Self {
            sharing: limit.sharing,
            algorithm: limit.algorithm,
            sustained: limit.sustained,
            capacity: limit
                .burst
                .and_then(|burst| burst.capacity)
                .unwrap_or(limit.sustained.rate),
            scope: limit.scope,
            strategy: limit.strategy,
            cost: limit.cost,
            response_headers: DEFAULT_RESPONSE_HEADERS,
            tags: Vec::new(),
        }
    }

    /// Turns the `X-RateLimit-*` headers off, for a configuration surface that
    /// carries the ADR-0003 `response_headers` key.
    #[must_use]
    pub fn with_response_headers(mut self, enabled: bool) -> Self {
        self.response_headers = enabled;
        self
    }

    /// Seconds in the sustained window.
    #[must_use]
    pub const fn window_secs(&self) -> u64 {
        window_secs(self.sustained.window)
    }
}

/// Merges the layers, most ancestral first: `upstream` → `route` → `tenant`.
///
/// Returns `None` when no layer declares a `rate_limit` block, which is not an
/// error: an unconfigured resource is simply not limited.
#[must_use]
pub fn merge(layers: &[RateLimitLayer<'_>]) -> Option<EffectiveRateLimit> {
    let mut effective: Option<EffectiveRateLimit> = None;
    let mut tags: Vec<Tag> = Vec::new();

    for layer in layers {
        // Tags have no sharing mode: the union is add-only, ancestors first, and
        // a descendant cannot remove an inherited tag.
        union_tags(&mut tags, layer.tags);
        effective = match effective {
            None => layer.rate_limit.map(EffectiveRateLimit::of),
            Some(parent) => match layer.rate_limit {
                Some(child) => Some(merge_child(parent, child)),
                None => Some(parent),
            },
        };
    }

    effective.map(|mut policy| {
        policy.tags = tags;
        policy
    })
}

/// The merged policy of an upstream and of the route that matched it, in the
/// order the proxy resolves them in. The tenant layer joins in [`merge`] once a
/// tenant-scoped configuration surface exists.
#[must_use]
pub fn for_upstream_route(
    upstream: &Upstream,
    route: Option<&Route>,
) -> Option<EffectiveRateLimit> {
    let mut layers = vec![RateLimitLayer::upstream(upstream)];
    if let Some(route) = route {
        layers.push(RateLimitLayer::route(route));
    }
    layers.push(RateLimitLayer::tenant());
    merge(&layers)
}

/// Combines the configuration currently in force with the one the descendant
/// declares, per the sharing mode of the configuration in force.
fn merge_child(parent: EffectiveRateLimit, child: &RateLimit) -> EffectiveRateLimit {
    match parent.sharing {
        // `private`: the block is not inherited, so a descendant that declares
        // its own configuration replaces it outright.
        SharingMode::Private => EffectiveRateLimit::of(child),
        // `inherit`: the parent's configuration is what the descendant gets.
        SharingMode::Inherit => parent,
        // `enforce`: the parent caps the descendant, field by field.
        SharingMode::Enforce => min_merge(parent, child),
    }
}

/// `min(parent, child)` for every numeric field; the descendant keeps the
/// non-numeric ones it declared.
fn min_merge(parent: EffectiveRateLimit, child: &RateLimit) -> EffectiveRateLimit {
    let child = EffectiveRateLimit::of(child);
    EffectiveRateLimit {
        // The stricter sustained rate wins, compared per second so that a
        // `100/minute` parent and a `10/second` child are comparable at all.
        sustained: stricter_sustained(parent.sustained, child.sustained),
        capacity: parent.capacity.min(child.capacity),
        cost: parent.cost.min(child.cost),
        // Both layers have to ask for the headers for them to be emitted.
        response_headers: parent.response_headers && child.response_headers,
        // The cap an ancestor enforces is never bypassed, so it keeps governing
        // the layers below.
        sharing: parent.sharing,
        algorithm: child.algorithm,
        scope: child.scope,
        strategy: child.strategy,
        tags: Vec::new(),
    }
}

/// The stricter of two sustained rates, keeping the (rate, window) pair of the
/// winner: the loser's window units are not comparable with the winner's.
fn stricter_sustained(parent: RateLimitSustained, child: RateLimitSustained) -> RateLimitSustained {
    if allows_fewer_per_second(child, parent) {
        child
    } else {
        parent
    }
}

/// Whether `candidate` admits fewer requests per second than `other`.
///
/// The two (rate, window) pairs are cross-multiplied, so `100/minute` and
/// `10/second` are comparable without division or floating point: a rate is at
/// most `u32::MAX` and a window at most `86_400` seconds long, so each product
/// fits `u128` with room to spare. An exact tie keeps `other`, the ancestor's
/// pair.
fn allows_fewer_per_second(candidate: RateLimitSustained, other: RateLimitSustained) -> bool {
    let candidate_per_second = u128::from(candidate.rate) * u128::from(window_secs(other.window));
    let other_per_second = u128::from(other.rate) * u128::from(window_secs(candidate.window));
    candidate_per_second < other_per_second
}

/// Appends the tags `layer` contributes that `tags` does not carry yet.
fn union_tags(tags: &mut Vec<Tag>, layer: &[Tag]) {
    for tag in layer {
        if !tags.iter().any(|known| known.as_str() == tag.as_str()) {
            tags.push(tag.clone());
        }
    }
}

#[cfg(test)]
#[path = "merger_tests.rs"]
mod tests;
