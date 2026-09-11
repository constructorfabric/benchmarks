//! The OAuth2 token cache: four-component keys, verified hits, margin-aware
//! TTLs.
//!
//! Realizes `cpt-cf-oagw-algo-token-cache` and the cache half of
//! `cpt-cf-oagw-flow-oauth2-token-cache`. The cache is plugin-internal: no
//! endpoint reaches it, nothing it holds is persisted, and no background task
//! refreshes it — the one-shot exchange returns and ends, and a revoked or
//! rotated token stays served until its entry expires, which is the staleness
//! window ADR 0008 accepts.
//!
//! Every entry carries the key it was stored under, and every hit verifies
//! that key against the key being looked up. A mismatch is a miss, so a hash
//! collision can never hand one tenant's token to another. Realizes
//! `cpt-cf-oagw-dod-token-cache`.

// @cpt-dod:cpt-cf-oagw-dod-token-cache:p1

use std::time::Duration;

use pingora_memory_cache::MemoryCache;
use serde_json::Value;
use toolkit_auth::SecretString;
use uuid::Uuid;

/// The lifetime an entry loses before it is served: a token the IdP reports as
/// living 30 seconds or fewer is injected once and never cached, so no entry is
/// ever served that is already at expiry.
pub const TOKEN_CACHE_SAFETY_MARGIN_SECS: u64 = 30;

/// The configured ceilings of the cache, threaded to the plugin constructors
/// through `AuthPluginRegistry::with_builtins`.
///
/// The values come from the gear configuration the foundation validated:
/// `token_cache_ttl_secs` and `token_cache_capacity`, whose defaults are
/// ADR 0008's 300 and 10000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// Ceiling of one entry's lifetime.
    pub ttl: Duration,
    /// Maximum number of entries; eviction at the ceiling is the cache's own
    /// policy, and the ceiling never causes a request to fail.
    pub capacity: usize,
}

impl TokenCacheConfig {
    /// Builds the ceilings the two OAuth2 variants are constructed with.
    #[must_use]
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self { ttl, capacity }
    }
}

/// One cached entry: the bearer value and the key it was stored under.
///
/// The key travels with the material so the verification on hit can compare it
/// against the key being looked up — the defence ADR 0008 records against the
/// cache hashing its keys to `u64` and never comparing them again.
#[derive(Clone)]
struct CachedToken {
    /// The full lookup key the entry was stored under.
    key: String,
    /// The bearer value, wrapped so eviction zeroes it.
    token: SecretString,
}

/// The cache the two OAuth2 Client Credentials variants share the shape of.
///
/// One instance per plugin, in memory, sized to `capacity` and bounded by
/// `ttl`. A lookup that finds no entry, finds an expired entry, or finds an
/// entry whose stored key differs is a miss, and a miss sends the caller
/// through the fetch path with no material read and no reference resolved.
pub struct TokenCache {
    entries: MemoryCache<String, CachedToken>,
    ttl: Duration,
}

impl TokenCache {
    /// Builds the cache over the configured ceilings.
    #[must_use]
    pub fn new(config: TokenCacheConfig) -> Self {
        Self {
            entries: MemoryCache::new(config.capacity),
            ttl: config.ttl,
        }
    }

    /// Reads the entry at `key`, answering the material only when the entry's
    /// stored key equals the key being looked up.
    ///
    /// An expired entry and a mismatched entry are both a miss; the expired
    /// one is dropped by the cache's own policy.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<SecretString> {
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-return
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-get-try
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-get
        let (entry, _status) = self.entries.get(&String::from(key));
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-get
        // The cache is an in-process structure with no failure mode of its
        // own, so the unavailable-cache catch the artifact carries has no
        // exceptional path to guard: a lookup that reads nothing is the miss
        // the `?` below answers, and never a request failure.
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-catch
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-catch-handle
        let cached = entry?;
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-catch-handle
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-catch
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-verify-if
        if cached.key != key {
            // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-miss-else
            // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-miss
            return None;
            // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-miss
            // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-miss-else
        }
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-verify
        Some(cached.token)
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-verify
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-verify-if
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-get-try
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-return
    }

    /// Stores one entry, answering whether it was stored.
    ///
    /// The entry's lifetime is the minimum of the configured ceiling and the
    /// reported lifetime less the safety margin. A token whose reported
    /// lifetime is at or below the margin is not stored: it is injected once
    /// and never served from the cache. A failed fetch never reaches this
    /// call, so a failed fetch is never cached.
    pub fn store(&self, key: &str, token: SecretString, reported_lifetime: Duration) -> bool {
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-put-if
        let Some(ttl) = entry_ttl(self.ttl, reported_lifetime) else {
            // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-noput-else
            // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-noput
            return false;
            // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-noput
            // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-noput-else
        };
        // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-put
        self.entries.put(
            &String::from(key),
            CachedToken {
                key: String::from(key),
                token,
            },
            Some(ttl),
        );
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-put
        true
        // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-put-if
    }
}

/// The lifetime one entry is held for: the configured ceiling or the reported
/// lifetime less the margin, whichever is shorter — or nothing at all when the
/// reported lifetime is at or below the margin.
fn entry_ttl(ceiling: Duration, reported_lifetime: Duration) -> Option<Duration> {
    let margin = Duration::from_secs(TOKEN_CACHE_SAFETY_MARGIN_SECS);
    if reported_lifetime <= margin {
        return None;
    }
    Some(ceiling.min(reported_lifetime - margin))
}

/// The component separator of a cache key.
///
/// A control character no tenant identifier, no subject identifier, no auth
/// method tag, and no hash spelling carries, so four components stay four
/// components.
const KEY_SEPARATOR: char = '\u{1f}';

/// Builds the cache key of one authentication: the subject tenant, the
/// subject, the auth method tag of the variant, and the hash of the plugin
/// configuration.
///
/// Each of the four components is present because its absence would break an
/// isolation boundary: the tenant keeps one tenant's token out of another
/// tenant's reach, the subject keeps the credential store's `private` sharing
/// mode meaningful, the method tag keeps the `Form` and `Basic` variants from
/// colliding over one configuration, and the hash keeps two upstreams whose
/// configurations differ — in the scopes as in anything else — on separate
/// entries.
#[must_use]
pub fn cache_key(
    tenant: Uuid,
    subject_id: Option<Uuid>,
    auth_method_tag: &str,
    config: &Value,
) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-token-cache:p1:inst-cache-key
    let components = [
        component(&tenant.to_string()),
        component(&subject_id.unwrap_or_default().to_string()),
        component(auth_method_tag),
        component(&hash_config(config)),
    ];
    components.join(&KEY_SEPARATOR.to_string())
    // @cpt-end:cpt-cf-oagw-algo-token-cache:p1:inst-cache-key
}

/// One key component, stripped of the separator it must never carry.
fn component(value: &str) -> String {
    value.replace(KEY_SEPARATOR, "")
}

/// Hashes a plugin configuration deterministically: every key, in sorted
/// order, nested values included.
///
/// The same configuration hashes to the same value in every process and after
/// every restart, so two upstreams whose configurations agree share an entry
/// and two whose configurations differ never do.
#[must_use]
pub fn hash_config(config: &Value) -> String {
    format!("{:016x}", fnv1a_64(canonical_form(config).as_bytes()))
}

/// Renders a configuration value as the string its hash is taken over.
fn canonical_form(config: &Value) -> String {
    match config {
        Value::Object(fields) => {
            let mut entries: Vec<(String, String)> = fields
                .iter()
                .map(|(key, value)| (key.clone(), canonical_form(value)))
                .collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            entries
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(",")
        }
        Value::Array(items) => items
            .iter()
            .map(canonical_form)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

/// The 64-bit FNV-1a of one byte string.
///
/// A fixed, published hash with no random seed, so the same configuration
/// hashes identically in every process.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lifetime_at_the_margin_is_not_stored() {
        assert_eq!(entry_ttl(Duration::from_secs(300), Duration::from_secs(30)), None);
    }

    #[test]
    fn a_lifetime_above_the_margin_loses_the_margin() {
        assert_eq!(
            entry_ttl(Duration::from_secs(300), Duration::from_secs(45)),
            Some(Duration::from_secs(15))
        );
    }

    #[test]
    fn a_long_lifetime_is_capped_by_the_ceiling() {
        assert_eq!(
            entry_ttl(Duration::from_secs(300), Duration::from_secs(3_600)),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn an_absent_subject_and_the_nil_subject_are_the_same_component() {
        let tenant = Uuid::from_u128(0x01);
        let config = serde_json::json!({});
        assert_eq!(
            cache_key(tenant, None, "form", &config),
            cache_key(tenant, Some(Uuid::nil()), "form", &config)
        );
    }
}
