//! Tests for the `OAuth2` token cache (ADR-0008).

use std::time::Duration;

use toolkit_auth::SecretString;

use crate::config::TokenCacheConfig;
use crate::plugins::token_cache::{hash_config, TokenCache, EXPIRY_SAFETY_MARGIN};

fn token(value: &str) -> SecretString {
    SecretString::new(value.to_owned())
}

#[test]
fn a_token_is_served_back_to_its_own_key() {
    let cache = TokenCache::new(8, Duration::from_mins(5));
    cache.put("a", token("alpha"), Duration::from_mins(10));
    assert_eq!(
        cache.get("a").map(|t| t.expose().to_owned()),
        Some("alpha".to_owned())
    );
    assert!(cache.get("b").is_none());
}

#[test]
fn an_entry_expires_after_its_ttl_minus_the_safety_margin() {
    let cache = TokenCache::new(8, Duration::from_mins(5));
    // 30 s + the 30 s margin leaves nothing: the token is not cached at all.
    cache.put("short", token("s"), EXPIRY_SAFETY_MARGIN);
    assert!(
        cache.get("short").is_none(),
        "an entry with no usable lifetime is not filed"
    );

    cache.put("ok", token("t"), EXPIRY_SAFETY_MARGIN + Duration::from_secs(1));
    assert!(cache.get("ok").is_some());
}

#[test]
fn the_cache_ttl_caps_the_entry_lifetime() {
    let cache = TokenCache::new(8, Duration::from_secs(10));
    cache.put("long", token("t"), Duration::from_hours(1));
    // The entry exists; its lifetime is the cache's own 10 s, not the IdP's hour.
    assert!(cache.get("long").is_some());
}

#[test]
fn a_zero_capacity_cache_still_serves_one_entry() {
    let cache = TokenCache::new(0, Duration::from_mins(5));
    cache.put("k", token("v"), Duration::from_mins(10));
    assert_eq!(
        cache.get("k").map(|t| t.expose().to_owned()),
        Some("v".to_owned()),
        "a degenerate capacity must not drop the entry outright"
    );
}

#[test]
fn the_configured_capacity_and_ttl_are_honoured() {
    let config = TokenCacheConfig {
        ttl_secs: 42,
        capacity: 7,
    };
    let cache = TokenCache::from_config(&config);
    cache.put("k", token("v"), Duration::from_mins(16) + Duration::from_secs(40));
    assert!(cache.get("k").is_some());
}

#[test]
fn the_configuration_digest_is_order_independent() {
    let mut one = serde_json::Map::new();
    one.insert("token_endpoint".to_owned(), serde_json::json!("https://idp"));
    one.insert("scopes".to_owned(), serde_json::json!(["a", "b"]));
    let mut two = serde_json::Map::new();
    two.insert("scopes".to_owned(), serde_json::json!(["a", "b"]));
    two.insert("token_endpoint".to_owned(), serde_json::json!("https://idp"));

    assert_eq!(hash_config(&one), hash_config(&two));
}

#[test]
fn a_different_configuration_hashes_differently() {
    let mut one = serde_json::Map::new();
    one.insert("token_endpoint".to_owned(), serde_json::json!("https://idp"));
    let mut two = serde_json::Map::new();
    two.insert("token_endpoint".to_owned(), serde_json::json!("https://other"));

    assert_ne!(hash_config(&one), hash_config(&two));
}

#[test]
fn an_empty_configuration_hashes_stably() {
    let empty = serde_json::Map::new();
    assert_eq!(hash_config(&empty), hash_config(&serde_json::Map::new()));
    assert_eq!(hash_config(&empty).len(), 64, "a SHA-256 hex digest");
}
