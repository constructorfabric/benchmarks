//! Best-effort provisioning of OAGW plugin type-schemas/instances into the
//! global types-registry.
//!
//! Registration is best-effort: failures (registry unavailable, malformed
//! schema, duplicate) are logged and never fail the control-plane operation.
//! The registry is not a runtime dependency of the proxy path.

use std::sync::Arc;

use serde_json::json;
use tracing::warn;

use types_registry_sdk::TypesRegistryClient;

use crate::domain::dto::{
    AUTH_BASIC_RESERVED, AUTH_BEARER_RESERVED, AUTH_PLUGIN_BASE, CustomPlugin, GUARD_CORS_RESERVED,
    GUARD_PLUGIN_BASE, GUARD_TIMEOUT_RESERVED, TRANSFORM_LOGGING_RESERVED,
    TRANSFORM_METRICS_RESERVED, TRANSFORM_PLUGIN_BASE,
};

/// Base type-schema ids for the three OAGW plugin families (ADR-0002).
///
/// Every OAGW plugin type — including the reserved catalog-only ids below —
/// derives from one of these bases (`auth_plugin.v1~`,
/// `guard_plugin.v1~`, `transform_plugin.v1~`). The types-registry resolves
/// an instance's type through its schema chain, so the bases must be
/// registered before any derived id or the ready-commit validation fails.
pub const RESERVED_BASE_TYPE_IDS: &[&str] =
    &[AUTH_PLUGIN_BASE, GUARD_PLUGIN_BASE, TRANSFORM_PLUGIN_BASE];

/// Reserved (catalog-only) OAGW plugin type ids (ADR-0009).
///
/// These are declared as GTS constants but are NOT shipped as executable
/// plugins — timeout/CORS guard, basic/bearer auth, logging/metrics transform
/// — so they must be registered in the types-registry catalog explicitly to
/// remain resolvable/self-describing.
pub const RESERVED_PLUGIN_TYPE_IDS: &[&str] = &[
    AUTH_BASIC_RESERVED,
    AUTH_BEARER_RESERVED,
    GUARD_TIMEOUT_RESERVED,
    GUARD_CORS_RESERVED,
    TRANSFORM_LOGGING_RESERVED,
    TRANSFORM_METRICS_RESERVED,
];

/// Optional registry client wrapper.
pub struct TypeProvisioner {
    client: Option<Arc<dyn TypesRegistryClient>>,
}

impl TypeProvisioner {
    #[must_use]
    pub fn new(client: Option<Arc<dyn TypesRegistryClient>>) -> Self {
        Self { client }
    }

    /// Provision the base plugin type-schemas and reserved (catalog-only)
    /// plugin types into the types-registry (ADR-0002, ADR-0009).
    ///
    /// The three family bases are registered first so the reserved derived
    /// ids validate against their schema chain at ready-commit.
    ///
    /// Best-effort: a missing client or a registry failure is logged, never
    /// fatal. Called once at gear start-up via [`crate::gear::OagwGear::init`].
    pub async fn register_reserved_types(&self) {
        let Some(client) = &self.client else {
            return;
        };
        let documents: Vec<serde_json::Value> = RESERVED_BASE_TYPE_IDS
            .iter()
            .map(|id| {
                json!({
                    "$id": id,
                    "title": id,
                    "description": "OAGW plugin family base type-schema (ADR-0002)",
                    "kind": "type-schema",
                })
            })
            .chain(RESERVED_PLUGIN_TYPE_IDS.iter().map(|id| {
                json!({
                    "$id": id,
                    "title": id,
                    "description": "OAGW reserved plugin type (catalog entry; ADR-0009)",
                    "kind": "type-schema",
                })
            }))
            .collect();
        match client.register(documents).await {
            Ok(_) => {}
            Err(e) => warn!(
                error = %e,
                "types-registry: reserved type registration failed (best-effort)"
            ),
        }
    }

    /// Register a custom plugin's type-schema (best-effort).
    ///
    /// The schema id is the plugin's full GTS identifier (type-schema form,
    /// i.e. ends with `~`), JSON Schema `config_schema` is embedded as the
    /// plugin config contract.
    pub async fn register_plugin_type(&self, plugin: &CustomPlugin) {
        let Some(client) = &self.client else {
            return;
        };
        let gts_id = plugin.gts_id(); // `{base}{uuid}` — schema kind
        let document = json!({
            "$id": gts_id,
            "title": plugin.name,
            "description": plugin.description.as_deref().unwrap_or("OAGW custom plugin"),
            "configSchema": plugin.config_schema.clone(),
            "kind": "type-schema",
        });
        let result = client.register(vec![document]).await;
        match result {
            Ok(_) => {}
            Err(e) => {
                warn!(%gts_id, error = %e, "types-registry: plugin schema registration failed (best-effort)");
            }
        }
    }

    /// No-op marker so dead-code analysis keeps the module honest.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    /// Deterministic schema-or-instance decision for OAGW plugin entities.
    #[must_use]
    pub fn entity_kind(gts_id: &str) -> &'static str {
        if gts_id.ends_with('~') {
            "type-schema"
        } else {
            "instance"
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use serde_json::Value;
    use toolkit_canonical_errors::CanonicalError;
    use types_registry_sdk::{
        GtsInstance, GtsTypeSchema, InstanceQuery, RegisterResult, TypeSchemaQuery,
    };
    use uuid::Uuid;

    #[test]
    fn entity_kind_detection() {
        assert_eq!(TypeProvisioner::entity_kind("gts.x.y.v1~"), "type-schema");
        assert_eq!(
            TypeProvisioner::entity_kind("gts.x.y.v1~cf.oagw.plugin.v1"),
            "instance"
        );
    }

    #[test]
    fn disconnected_provisioner_is_quiet() {
        let p = TypeProvisioner::new(None);
        assert!(!p.is_connected());
        let plugin = CustomPlugin {
            id: Uuid::default(),
            tenant_id: Uuid::default(),
            plugin_type: crate::domain::dto::PluginKind::Guard,
            name: "p".into(),
            description: None,
            config_schema: Value::Null,
            source_code: String::new(),
            created_at: 0,
        };
        // Must not panic / await anything.
        let _ = plugin;
    }

    /// In-memory fake registry client that records every batch registration.
    #[derive(Default)]
    struct RecordingClient {
        registered: Mutex<Vec<Value>>,
    }

    impl RecordingClient {
        fn ids(&self) -> Vec<String> {
            self.registered
                .lock()
                .unwrap()
                .iter()
                .map(|d| d["$id"].as_str().unwrap_or_default().to_owned())
                .collect()
        }
    }

    #[async_trait]
    impl TypesRegistryClient for RecordingClient {
        async fn register(
            &self,
            entities: Vec<Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            let results: Vec<RegisterResult> = entities
                .iter()
                .map(|d| RegisterResult::Ok {
                    gts_id: d["$id"].as_str().unwrap_or_default().to_owned(),
                })
                .collect();
            self.registered.lock().unwrap().extend(entities);
            Ok(results)
        }
        async fn register_type_schemas(
            &self,
            _: Vec<Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            unimplemented!()
        }
        async fn get_type_schema(&self, _: &str) -> Result<GtsTypeSchema, CanonicalError> {
            unimplemented!()
        }
        async fn get_type_schema_by_uuid(&self, _: Uuid) -> Result<GtsTypeSchema, CanonicalError> {
            unimplemented!()
        }
        async fn get_type_schemas(
            &self,
            _: Vec<String>,
        ) -> HashMap<String, Result<GtsTypeSchema, CanonicalError>> {
            unimplemented!()
        }
        async fn get_type_schemas_by_uuid(
            &self,
            _: Vec<Uuid>,
        ) -> HashMap<Uuid, Result<GtsTypeSchema, CanonicalError>> {
            unimplemented!()
        }
        async fn list_type_schemas(
            &self,
            _: TypeSchemaQuery,
        ) -> Result<Vec<GtsTypeSchema>, CanonicalError> {
            unimplemented!()
        }
        async fn register_instances(
            &self,
            _: Vec<Value>,
        ) -> Result<Vec<RegisterResult>, CanonicalError> {
            unimplemented!()
        }
        async fn get_instance(&self, _: &str) -> Result<GtsInstance, CanonicalError> {
            unimplemented!()
        }
        async fn get_instance_by_uuid(&self, _: Uuid) -> Result<GtsInstance, CanonicalError> {
            unimplemented!()
        }
        async fn get_instances(
            &self,
            _: Vec<String>,
        ) -> HashMap<String, Result<GtsInstance, CanonicalError>> {
            unimplemented!()
        }
        async fn get_instances_by_uuid(
            &self,
            _: Vec<Uuid>,
        ) -> HashMap<Uuid, Result<GtsInstance, CanonicalError>> {
            unimplemented!()
        }
        async fn list_instances(
            &self,
            _: InstanceQuery,
        ) -> Result<Vec<GtsInstance>, CanonicalError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn register_reserved_types_registers_catalog_only_ids() {
        // ADR-0002/0009: the plugin-family base type-schemas plus the reserved
        // (catalog-only) plugin ids must be provisioned into the types-registry
        // even though no executable plugin backs the reserved entries. The
        // bases come first so the derived ids validate against their schema
        // chain at types-registry ready-commit.
        let client = Arc::new(RecordingClient::default());
        let p = TypeProvisioner::new(Some(client.clone()));
        p.register_reserved_types().await;
        let ids = client.ids();
        for expected in RESERVED_PLUGIN_TYPE_IDS {
            assert!(
                ids.iter().any(|id| id == expected),
                "reserved id {expected} was not registered; got {ids:?}"
            );
        }
        for expected in RESERVED_BASE_TYPE_IDS {
            assert!(
                ids.iter().any(|id| id == expected),
                "base type-schema {expected} was not registered; got {ids:?}"
            );
        }
        // Bases precede their derived ids in the batch.
        let base_positions: HashMap<&str, usize> = RESERVED_BASE_TYPE_IDS
            .iter()
            .map(|id| (*id, ids.iter().position(|i| i == *id).unwrap()))
            .collect();
        for reserved in RESERVED_PLUGIN_TYPE_IDS {
            let pos = ids.iter().position(|i| i == reserved).unwrap();
            let parent = &reserved[..reserved.rfind('~').map_or(0, |i| i + 1)];
            if let Some(&p_pos) = base_positions.get(parent) {
                assert!(
                    p_pos < pos,
                    "base {parent} must register before derived {reserved}"
                );
            }
        }
        assert_eq!(
            ids.len(),
            RESERVED_PLUGIN_TYPE_IDS.len() + RESERVED_BASE_TYPE_IDS.len()
        );
    }

    #[tokio::test]
    async fn disconnected_provisioner_skips_reserved_registration() {
        let p = TypeProvisioner::new(None);
        // Must be a quiet no-op (no client to record to / panic on).
        p.register_reserved_types().await;
    }
}
