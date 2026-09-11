// @cpt-begin:cpt-cf-oagw-dod-policy-oauth2-token-cache:p2:inst-oauth2
//! `OAuth2` client-credentials token acquisition and caching.
//!
//! Tokens are cached in process, keyed by the calling subject and the plugin
//! configuration. The cached entry carries its own key so a hash collision is
//! detected on read rather than silently serving another subject's token.
//! A failed fetch is never cached.

use crate::domain::error::{DomainError, DomainResult, ErrorKind};
use dashmap::DashMap;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use toolkit_http::HttpClient;

/// Safety margin subtracted from the token's own lifetime.
const EXPIRY_SAFETY_MARGIN_SECS: u64 = 30;

/// How the client credentials are presented to the token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAuthStyle {
    /// Credentials travel in the form body.
    Form,
    /// Credentials travel in an `Authorization: Basic` header.
    Basic,
}

impl ClientAuthStyle {
    /// Tag used to separate cache entries of the two variants.
    const fn tag(self) -> &'static str {
        match self {
            Self::Form => "oauth2_client_cred",
            Self::Basic => "oauth2_client_cred_basic",
        }
    }
}

/// A cached bearer token.
#[derive(Debug, Clone)]
struct CachedToken {
    /// The key this entry was stored under, re-checked on read.
    key: String,
    token: String,
    expires_at: Instant,
}

/// In-process cache of client-credentials tokens.
#[derive(Debug, Default)]
pub struct TokenCache {
    entries: DashMap<String, CachedToken>,
    capacity: usize,
}

impl TokenCache {
    /// Create a cache bounded to `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: DashMap::new(),
            capacity: capacity.max(1),
        }
    }

    /// Build the cache key for a request.
    #[must_use]
    pub fn cache_key(
        subject_tenant_id: &str,
        subject_id: &str,
        style: ClientAuthStyle,
        config: &BTreeMap<String, serde_json::Value>,
    ) -> String {
        // A stable rendering of the configuration distinguishes two upstreams
        // that share a subject but differ in scope or endpoint.
        let rendered = config
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let hash = simple_hash(&rendered);
        format!(
            "{subject_tenant_id}:{subject_id}:{}:{hash:016x}",
            style.tag()
        )
    }

    /// Read a live token, if one is cached under this exact key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        let entry = self.entries.get(key)?;
        // Defend against a hash collision: the entry names its own key.
        if entry.key != key {
            return None;
        }
        if Instant::now() >= entry.expires_at {
            return None;
        }
        Some(entry.token.clone())
    }

    /// Store a token with the effective time to live.
    pub fn put(&self, key: String, token: String, ttl: Duration) {
        if self.entries.len() >= self.capacity {
            // Drop expired entries before admitting a new one.
            let now = Instant::now();
            self.entries.retain(|_, e| e.expires_at > now);
        }
        let entry = CachedToken {
            key: key.clone(),
            token,
            expires_at: Instant::now() + ttl,
        };
        self.entries.insert(key, entry);
    }

    /// Number of entries currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Effective time to live for a token.
///
/// The configured ceiling and the token's own lifetime, less a safety margin,
/// whichever is shorter.
#[must_use]
pub fn effective_ttl(configured_ttl_secs: u64, expires_in_secs: Option<u64>) -> Duration {
    let from_token = expires_in_secs.map_or(configured_ttl_secs, |e| {
        e.saturating_sub(EXPIRY_SAFETY_MARGIN_SECS)
    });
    Duration::from_secs(configured_ttl_secs.min(from_token).max(1))
}

/// A cheap, stable, non-cryptographic hash of the rendered configuration.
fn simple_hash(input: &str) -> u64 {
    // FNV-1a: deterministic across runs, which a cache key requires.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Request a client-credentials token from the configured endpoint.
///
/// # Errors
/// Returns `AuthenticationFailed` when the endpoint rejects the request or the
/// response carries no access token.
pub async fn fetch_token(
    http: &HttpClient,
    token_endpoint: &str,
    client_id: &str,
    client_secret: &str,
    scopes: Option<&str>,
    style: ClientAuthStyle,
) -> DomainResult<(String, Option<u64>)> {
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
    if let Some(scopes) = scopes {
        form.push(("scope", scopes));
    }
    if style == ClientAuthStyle::Form {
        form.push(("client_id", client_id));
        form.push(("client_secret", client_secret));
    }

    let mut builder = http.post(token_endpoint);
    if style == ClientAuthStyle::Basic {
        // The Basic variant carries the pair in the Authorization header.
        let encoded = base64_encode(format!("{client_id}:{client_secret}").as_bytes());
        builder = builder.header("authorization", &format!("Basic {encoded}"));
    }
    let builder = builder.form(&form).map_err(|e| {
        DomainError::new(
            ErrorKind::AuthenticationFailed,
            format!("could not build the token request: {e}"),
        )
    })?;

    let response = builder.send().await.map_err(|_| {
        DomainError::new(
            ErrorKind::AuthenticationFailed,
            "the token endpoint could not be reached",
        )
    })?;
    if !response.status().is_success() {
        return Err(DomainError::new(
            ErrorKind::AuthenticationFailed,
            format!("the token endpoint answered {}", response.status()),
        ));
    }
    let body: serde_json::Value = response.json().await.map_err(|_| {
        DomainError::new(
            ErrorKind::AuthenticationFailed,
            "the token endpoint returned a body that is not JSON",
        )
    })?;
    let token = body
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DomainError::new(
                ErrorKind::AuthenticationFailed,
                "the token response carried no access_token",
            )
        })?
        .to_owned();
    let expires_in = body.get("expires_in").and_then(serde_json::Value::as_u64);
    Ok((token, expires_in))
}

/// Encode bytes as standard base64.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |b| u32::from(*b));
        let b2 = chunk.get(2).map_or(0, |b| u32::from(*b));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
// @cpt-end:cpt-cf-oagw-dod-policy-oauth2-token-cache:p2:inst-oauth2

#[cfg(test)]
mod tests {
    use super::{ClientAuthStyle, TokenCache, base64_encode, effective_ttl};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn config(pairs: &[(&str, &str)]) -> BTreeMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String((*v).to_owned())))
            .collect()
    }

    #[test]
    fn the_effective_ttl_takes_the_shorter_of_the_two_bounds() {
        // The token expires sooner than the configured ceiling.
        assert_eq!(effective_ttl(300, Some(90)), Duration::from_mins(1));
        // The configured ceiling is the tighter bound.
        assert_eq!(effective_ttl(120, Some(3_600)), Duration::from_mins(2));
        // No stated lifetime falls back to the configured value.
        assert_eq!(effective_ttl(300, None), Duration::from_mins(5));
        // A lifetime inside the safety margin still yields a live entry.
        assert_eq!(effective_ttl(300, Some(10)), Duration::from_secs(1));
    }

    #[test]
    fn the_cache_key_separates_subjects_configs_and_variants() {
        let cfg = config(&[("token_endpoint", "https://idp.test/token")]);
        let other = config(&[("token_endpoint", "https://other.test/token")]);
        let baseline = TokenCache::cache_key("t1", "s1", ClientAuthStyle::Form, &cfg);
        let other_tenant = TokenCache::cache_key("t2", "s1", ClientAuthStyle::Form, &cfg);
        let other_subject = TokenCache::cache_key("t1", "s2", ClientAuthStyle::Form, &cfg);
        let basic_variant = TokenCache::cache_key("t1", "s1", ClientAuthStyle::Basic, &cfg);
        let other_config = TokenCache::cache_key("t1", "s1", ClientAuthStyle::Form, &other);
        let keys = [
            &baseline,
            &other_tenant,
            &other_subject,
            &basic_variant,
            &other_config,
        ];
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                assert_ne!(keys[i], keys[j], "keys {i} and {j} must differ");
            }
        }
    }

    #[test]
    fn the_cache_key_is_stable_for_the_same_inputs() {
        let cfg = config(&[("token_endpoint", "https://idp.test/token")]);
        let first = TokenCache::cache_key("t1", "s1", ClientAuthStyle::Form, &cfg);
        let second = TokenCache::cache_key("t1", "s1", ClientAuthStyle::Form, &cfg);
        assert_eq!(first, second);
    }

    #[test]
    fn a_cached_token_is_served_until_it_expires() {
        let cache = TokenCache::new(10);
        cache.put("k".to_owned(), "tok".to_owned(), Duration::from_mins(1));
        assert_eq!(cache.get("k").as_deref(), Some("tok"));
        assert_eq!(cache.get("other"), None);
    }

    #[test]
    fn an_expired_entry_is_not_served() {
        let cache = TokenCache::new(10);
        cache.put("k".to_owned(), "tok".to_owned(), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.get("k"), None);
    }

    #[test]
    fn the_cache_starts_empty() {
        let cache = TokenCache::new(4);
        assert!(cache.is_empty());
        cache.put("k".to_owned(), "t".to_owned(), Duration::from_secs(5));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b"id:secret"), "aWQ6c2VjcmV0");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }
}
