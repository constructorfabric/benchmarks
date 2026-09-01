//! Configuration for the Types Registry gear.

use std::time::Duration;

use serde::Deserialize;

use crate::infra::cache::{CacheConfig, DEFAULT_CACHE_CAPACITY, DEFAULT_CACHE_TTL};

/// Configuration for the Types Registry gear.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TypesRegistryConfig {
    /// Fields to check for GTS entity ID (in order of priority).
    /// Default: `["$id", "gtsId", "id"]`
    pub entity_id_fields: Vec<String>,

    /// Fields to check for schema ID reference (in order of priority).
    /// Default: `["$schema", "gtsTid", "type"]`
    pub schema_id_fields: Vec<String>,

    /// Raw GTS entity JSON values to register at startup.
    ///
    /// Each entry must be a valid GTS entity with at least an `$id` (or
    /// `gtsId`/`id`) field. Entities are registered in order.
    #[serde(default)]
    pub entities: Vec<serde_json::Value>,

    /// Tuning for the in-process [`TypesRegistryLocalClient`](crate::domain::local_client::TypesRegistryLocalClient).
    ///
    /// Currently only carries cache settings, but lives under its own
    /// section so future local-client knobs (resolver pools, retry
    /// policies, etc.) don't crowd the top level.
    #[serde(default)]
    pub local_client: LocalClientSettings,
}

/// Settings for the in-process local client adapter.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct LocalClientSettings {
    /// Per-kind cache tuning. Defaults match
    /// [`DEFAULT_CACHE_CAPACITY`] / [`DEFAULT_CACHE_TTL`].
    pub cache: CacheSettings,
}

/// Per-kind cache settings.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct CacheSettings {
    /// Cache settings for the type-schema cache.
    pub type_schemas: SingleCacheSettings,
    /// Cache settings for the instance cache.
    pub instances: SingleCacheSettings,
}

/// Settings for a single LRU cache.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SingleCacheSettings {
    /// Maximum number of entries before LRU eviction. Clamped to `1` if `0`.
    pub capacity: usize,
    /// Maximum age of an entry before it's treated as a miss. Accepts a
    /// human-readable duration string (e.g. `"60s"`, `"2m"`); explicit
    /// `null` disables TTL entirely. Omitting the field falls back to
    /// [`DEFAULT_CACHE_TTL`].
    #[serde(with = "toolkit_utils::humantime_serde::option")]
    pub ttl: Option<Duration>,
}

impl Default for SingleCacheSettings {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CACHE_CAPACITY,
            ttl: Some(DEFAULT_CACHE_TTL),
        }
    }
}

impl SingleCacheSettings {
    /// Converts to the infra-layer [`CacheConfig`].
    #[must_use]
    pub const fn to_cache_config(&self) -> CacheConfig {
        CacheConfig {
            capacity: self.capacity,
            ttl: self.ttl,
        }
    }
}

impl Default for TypesRegistryConfig {
    fn default() -> Self {
        Self {
            entity_id_fields: vec!["$id".to_owned(), "gtsId".to_owned(), "id".to_owned()],
            schema_id_fields: vec!["$schema".to_owned(), "gtsTid".to_owned(), "type".to_owned()],
            entities: Vec::new(),
            local_client: LocalClientSettings::default(),
        }
    }
}

impl TypesRegistryConfig {
    /// Converts this config to a `gts::GtsConfig`.
    #[must_use]
    pub fn to_gts_config(&self) -> gts::GtsConfig {
        gts::GtsConfig {
            entity_id_fields: self.entity_id_fields.clone(),
            type_id_fields: self.schema_id_fields.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = TypesRegistryConfig::default();
        assert_eq!(cfg.entity_id_fields, vec!["$id", "gtsId", "id"]);
        assert_eq!(cfg.schema_id_fields, vec!["$schema", "gtsTid", "type"]);
        assert!(cfg.entities.is_empty());
    }

    #[test]
    fn test_to_gts_config() {
        let cfg = TypesRegistryConfig::default();
        let gts_cfg = cfg.to_gts_config();
        assert_eq!(gts_cfg.entity_id_fields, cfg.entity_id_fields);
        assert_eq!(gts_cfg.type_id_fields, cfg.schema_id_fields);
    }

    #[test]
    fn test_default_cache_settings_match_infra_constants() {
        let cfg = TypesRegistryConfig::default();
        assert_eq!(
            cfg.local_client.cache.type_schemas.capacity,
            DEFAULT_CACHE_CAPACITY
        );
        assert_eq!(
            cfg.local_client.cache.type_schemas.ttl,
            Some(DEFAULT_CACHE_TTL)
        );
        assert_eq!(
            cfg.local_client.cache.instances.capacity,
            DEFAULT_CACHE_CAPACITY
        );
        assert_eq!(
            cfg.local_client.cache.instances.ttl,
            Some(DEFAULT_CACHE_TTL)
        );
    }

    #[test]
    fn test_cache_settings_with_explicit_values() {
        // JSON shape matches YAML 1:1 for the fields we care about (humantime
        // accepts duration strings via Visitor::visit_str regardless of the
        // input format).
        let json = serde_json::json!({
            "local_client": {
                "cache": {
                    "type_schemas": { "capacity": 2048, "ttl": "2m" },
                    "instances":    { "capacity": 512,  "ttl": "30s" },
                }
            }
        });
        let cfg: TypesRegistryConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.local_client.cache.type_schemas.capacity, 2048);
        assert_eq!(
            cfg.local_client.cache.type_schemas.ttl,
            Some(std::time::Duration::from_mins(2))
        );
        assert_eq!(cfg.local_client.cache.instances.capacity, 512);
        assert_eq!(
            cfg.local_client.cache.instances.ttl,
            Some(std::time::Duration::from_secs(30))
        );
    }

    #[test]
    fn test_cache_settings_null_ttl_disables() {
        let json = serde_json::json!({
            "local_client": {
                "cache": {
                    "type_schemas": { "capacity": 100, "ttl": null },
                    "instances":    { "capacity": 100, "ttl": null },
                }
            }
        });
        let cfg: TypesRegistryConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.local_client.cache.type_schemas.ttl, None);
        assert_eq!(cfg.local_client.cache.instances.ttl, None);
    }

    #[test]
    fn test_cache_settings_omitted_falls_back_to_default() {
        // Whole `cache` block missing — defaults must come from
        // SingleCacheSettings::default(), keeping parity with InMemoryCache's
        // hardcoded defaults.
        let cfg: TypesRegistryConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(
            cfg.local_client.cache.type_schemas.capacity,
            DEFAULT_CACHE_CAPACITY
        );
        assert_eq!(
            cfg.local_client.cache.type_schemas.ttl,
            Some(DEFAULT_CACHE_TTL)
        );
    }

    #[test]
    fn test_to_cache_config_round_trip() {
        let settings = SingleCacheSettings {
            capacity: 7,
            ttl: Some(std::time::Duration::from_secs(11)),
        };
        let cache_cfg = settings.to_cache_config();
        assert_eq!(cache_cfg.capacity, 7);
        assert_eq!(cache_cfg.ttl, Some(std::time::Duration::from_secs(11)));
    }
}
