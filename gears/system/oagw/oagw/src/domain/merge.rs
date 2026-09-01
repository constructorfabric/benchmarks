//! Hierarchical configuration merge (DESIGN §3.1 "Hierarchical Configuration").
//!
//! The data plane never stores an "effective configuration": it folds the
//! resolved tenant chain and the matched route on every request. Both steps
//! are pure functions over ordered slices so the sharing-mode rules can be
//! unit tested without a store.
//!
//! **Chain direction convention.** Every function here takes the chain ordered
//! *root → leaf*, least specific first, and the *last* element is the most
//! specific one (the selected upstream, then the matched route).
//!
//! Merge rules, per field:
//!
//! | Field | Rule |
//! |---|---|
//! | Auth | Override if `inherit`; forced if `enforce` |
//! | Rate limits | `min(ancestor, descendant)` — stricter always wins |
//! | Plugins | Concatenate: ancestor first, descendant appends |
//! | CORS | Union origins; `enforce` pins the ancestor policy |
//! | Headers | Most specific block wins |
//! | Tags | Add-only union, descendants cannot remove inherited tags |
//!
//! A `private` block is only visible to the tenant that declares it, so an
//! ancestor's `private` block never reaches a descendant request.

use uuid::Uuid;

use crate::domain::models::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, Protocol, RateLimitConfig, Route,
    SharingMode, Upstream,
};

/// Effective data-plane configuration of one proxied request.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    /// Upstream the request is addressed to (the closest shadowing match).
    pub upstream: Upstream,
    /// Auth plugin binding after the chain merge.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules after the chain merge.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain after the merge, ancestor plugins first.
    pub plugins: Vec<String>,
    /// Rate limit after the chain merge.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy after the chain merge.
    pub cors: Option<CorsConfig>,
    /// Effective tag set (add-only union).
    pub tags: Vec<String>,
}

impl EffectiveConfig {
    /// Protocol of the resolved upstream.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.upstream.protocol
    }
}

/// Types carrying a hierarchical sharing mode.
pub trait Scoped {
    /// Visibility of the block towards descendants.
    fn sharing(&self) -> SharingMode;
}

impl Scoped for AuthConfig {
    fn sharing(&self) -> SharingMode {
        self.sharing
    }
}

impl Scoped for PluginsConfig {
    fn sharing(&self) -> SharingMode {
        self.sharing
    }
}

impl Scoped for RateLimitConfig {
    fn sharing(&self) -> SharingMode {
        self.sharing
    }
}

impl Scoped for CorsConfig {
    fn sharing(&self) -> SharingMode {
        self.sharing
    }
}

/// Projects the blocks of the chain that are visible to the requesting tenant,
/// ordered least specific → most specific.
///
/// An ancestor's `private` block is invisible to a descendant request; the
/// most specific level (the selected upstream or the matched route) always
/// contributes its own block.
fn visible<'a, T, F>(chain: &[&'a Upstream], project: F) -> Vec<&'a T>
where
    F: Fn(&'a Upstream) -> Option<&'a T>,
    T: Scoped,
{
    let Some((last, ancestors)) = chain.split_last() else {
        return Vec::new();
    };
    let mut blocks: Vec<&T> = ancestors
        .iter()
        .filter_map(|upstream| project(upstream))
        .filter(|block| block.sharing() != SharingMode::Private)
        .collect();
    if let Some(own) = project(last) {
        blocks.push(own);
    }
    blocks
}

/// Folds the scalar blocks of the chain: an `enforce` block pins the value and
/// cannot be overridden by a more specific level.
fn enforce_first<'a, T: Scoped>(blocks: &[&'a T]) -> Option<&'a T> {
    let mut current = blocks.first()?;
    for next in blocks.iter().skip(1) {
        if current.sharing() == SharingMode::Enforce {
            return Some(current);
        }
        current = next;
    }
    Some(current)
}

/// Merges the CORS blocks of the chain: origins are unioned and `enforce`
/// keeps an ancestor policy active across shadowing.
fn merge_cors_blocks(blocks: &[&CorsConfig]) -> Option<CorsConfig> {
    let last = blocks.last()?;
    let mut merged = (*last).clone();
    merged.enabled = blocks.iter().any(|block| block.enabled);
    merged.allow_credentials = blocks.iter().any(|block| block.allow_credentials);
    merged.allowed_origins = union(blocks.iter().map(|block| &block.allowed_origins));
    merged.allowed_methods = blocks
        .iter()
        .flat_map(|block| block.allowed_methods.iter().copied())
        .collect();
    merged.expose_headers = union(blocks.iter().map(|block| &block.expose_headers));
    Some(merged)
}

/// Order-preserving union of several string collections.
fn union<'a, I>(parts: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a Vec<String>>,
{
    let mut merged: Vec<String> = Vec::new();
    for values in parts {
        for value in values {
            if !merged.contains(value) {
                merged.push(value.clone());
            }
        }
    }
    merged
}

/// Merges the rate-limit blocks of the chain: the strictest sustained rate
/// wins and the bucket capacity is the smallest of all blocks.
///
/// The comparison is exact — `rate_a / window_a < rate_b / window_b` is
/// evaluated as a cross-multiplication so no rounding can silently loosen a
/// limit.
fn merge_rate_limit_blocks(blocks: &[&RateLimitConfig]) -> Option<RateLimitConfig> {
    let mut strictest = blocks.first().copied()?;
    for candidate in blocks.iter().copied().skip(1) {
        if is_stricter(candidate, strictest) {
            strictest = candidate;
        }
    }
    let capacity = blocks
        .iter()
        .map(|block| block.effective_capacity())
        .min()?;
    let mut merged = (*strictest).clone();
    // The strictest block decides whether the limiter publishes its quota; an
    // ancestor that opted out must not be re-enabled by a looser descendant.
    merged.response_headers = strictest.response_headers;
    if let Some(burst) = merged.burst.as_ref() {
        if burst.capacity > capacity {
            merged.burst = Some(crate::domain::models::BurstCapacity { capacity });
        }
    } else if capacity < merged.sustained.rate {
        merged.burst = Some(crate::domain::models::BurstCapacity { capacity });
    }
    Some(merged)
}

/// Whether `candidate` admits fewer requests per second than `current`.
fn is_stricter(candidate: &RateLimitConfig, current: &RateLimitConfig) -> bool {
    let candidate_window = candidate.sustained.window.seconds();
    let current_window = current.sustained.window.seconds();
    candidate.sustained
        .rate
        .saturating_mul(current_window)
        < current.sustained
            .rate
            .saturating_mul(candidate_window)
}

/// Concatenates the plugin chains of the block, dropping duplicates while
/// preserving order (an ancestor plugin is never re-executed because a
/// descendant repeats the reference).
fn merge_plugin_blocks(blocks: &[&PluginsConfig]) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    for block in blocks {
        for reference in &block.items {
            if !chain.contains(reference) {
                chain.push(reference.clone());
            }
        }
    }
    chain
}

/// Folds the tenant chain of an alias into the effective configuration.
///
/// `chain` is ordered **root → leaf**; the last element must be the selected
/// upstream produced by [`crate::domain::routing::resolve_alias`].
#[must_use]
pub fn merge_upstream_chain(chain: &[&Upstream]) -> EffectiveConfig {
    let selected = chain.last().copied().cloned().unwrap_or_else(orphan_upstream);
    EffectiveConfig {
        upstream: selected,
        auth: enforce_first(&visible(chain, |upstream| upstream.auth.as_ref())).cloned(),
        headers: headers_of_chain(chain),
        plugins: merge_plugin_blocks(&visible(chain, |upstream| upstream.plugins.as_ref())),
        rate_limit: merge_rate_limit_blocks(&visible(chain, |upstream| {
            upstream.rate_limit.as_ref()
        })),
        cors: merge_cors_blocks(&visible(chain, |upstream| upstream.cors.as_ref())),
        tags: union(chain.iter().map(|upstream| &upstream.tags)),
    }
}

/// Folds the matched route into the effective configuration.
///
/// The route is more specific than the upstream it belongs to, so its rate
/// limit is folded with `min()`, its plugins are appended and its tags are
/// unioned in.
pub fn apply_route(config: &mut EffectiveConfig, route: &Route) {
    if let Some(route_limit) = route.rate_limit.as_ref() {
        let mut blocks: Vec<&RateLimitConfig> = config.rate_limit.iter().collect();
        blocks.push(route_limit);
        config.rate_limit = merge_rate_limit_blocks(&blocks);
    }
    if let Some(route_plugins) = route.plugins.as_ref() {
        let blocks = [route_plugins];
        for reference in merge_plugin_blocks(blocks.as_slice()) {
            if !config.plugins.contains(&reference) {
                config.plugins.push(reference);
            }
        }
    }
    for tag in &route.tags {
        if !config.tags.contains(tag) {
            config.tags.push(tag.clone());
        }
    }
}

/// Resolves the header rules of the chain: the most specific block wins.
///
/// Header rules are not additive across tenants — an ancestor cannot express
/// "add these headers and let the descendant add more" — so the block closest
/// to the request wins wholesale (DESIGN §3.1 lists no header merge rule).
fn headers_of_chain(chain: &[&Upstream]) -> Option<HeadersConfig> {
    chain
        .iter()
        .rev()
        .find_map(|upstream| upstream.headers.clone())
}

/// Placeholder used when the caller passes an empty chain, which
/// [`crate::domain::routing::resolve_alias`] already prevents.
fn orphan_upstream() -> Upstream {
    Upstream {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        alias: String::new(),
        enabled: false,
        protocol: Protocol::Http,
        server: crate::domain::models::ServerConfig { endpoints: Vec::new() },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
