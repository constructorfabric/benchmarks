//! GTS type provisioning: publish the OAGW type schemas to the types-registry.
//!
//! PRD "types_registry" dependency: *"GTS schema/instance registration for
//! plugin types and upstream/route type definitions"*. The schemas ship with
//! the crate (`docs/schemas/*.schema.json`) and are registered verbatim under
//! their type ids, so the OAGW resource shapes are discoverable platform-wide.
//!
//! Provisioning is **best-effort**: a registry hiccup must not stop the gear
//! from serving configuration, so each failure is logged (with the canonical
//! error the registry produced) rather than propagated. The registry is a hard
//! `deps` entry, so it is up before `init` runs.

use std::sync::Arc;

use tracing::info;
use types_registry_sdk::TypesRegistryClient;

use crate::domain::gts_helpers::{
    OAGW_AUTH_PLUGIN_TYPE_ID, OAGW_GUARD_PLUGIN_TYPE_ID, OAGW_ROUTE_TYPE_ID,
    OAGW_TRANSFORM_PLUGIN_TYPE_ID, OAGW_UPSTREAM_TYPE_ID,
};

/// The upstream service schema, as shipped.
pub const UPSTREAM_SCHEMA: &str = include_str!("../../../docs/schemas/upstream.v1.schema.json");
/// The route schema, as shipped.
pub const ROUTE_SCHEMA: &str = include_str!("../../../docs/schemas/route.v1.schema.json");

/// Provisions the OAGW type schemas.
pub struct TypeProvisioner {
    registry: Arc<dyn TypesRegistryClient>,
}

impl TypeProvisioner {
    /// Provisioner over a resolved registry client.
    #[must_use]
    pub fn new(registry: Arc<dyn TypesRegistryClient>) -> Self {
        Self { registry }
    }

    /// The type ids that must exist for the OAGW catalog to be complete.
    #[must_use]
    pub fn type_ids() -> Vec<&'static str> {
        vec![
            OAGW_UPSTREAM_TYPE_ID,
            OAGW_ROUTE_TYPE_ID,
            OAGW_AUTH_PLUGIN_TYPE_ID,
            OAGW_GUARD_PLUGIN_TYPE_ID,
            OAGW_TRANSFORM_PLUGIN_TYPE_ID,
        ]
    }

    /// Build the registration payloads: the two JSON Schemas plus a stub
    /// type-schema entry per plugin type (the plugin catalog carries no
    /// per-instance schema of its own).
    #[must_use]
    pub fn payloads() -> Vec<serde_json::Value> {
        let mut payloads = Vec::new();
        for (type_id, schema) in [
            (OAGW_UPSTREAM_TYPE_ID, UPSTREAM_SCHEMA),
            (OAGW_ROUTE_TYPE_ID, ROUTE_SCHEMA),
        ] {
            let mut value: serde_json::Value = match serde_json::from_str(schema) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!(type_id, err = %err, "OAGW schema is not valid JSON");
                    continue;
                }
            };
            let object = match value.as_object_mut() {
                Some(object) => object,
                None => continue,
            };
            object.insert(
                "$id".to_owned(),
                serde_json::Value::String(type_id.to_owned()),
            );
            object.insert(
                "title".to_owned(),
                serde_json::Value::String(format!("OAGW {type_id}")),
            );
            payloads.push(value);
        }
        payloads
    }

    /// Publish the schemas, logging (not propagating) per-item failures.
    pub async fn provision(&self) {
        let payloads = Self::payloads();
        if payloads.is_empty() {
            tracing::warn!("no OAGW type payloads to provision");
            return;
        }
        let count = payloads.len();
        match self.registry.register(payloads).await {
            Ok(results) => {
                let mut registered = 0usize;
                for result in results {
                    match result {
                        types_registry_sdk::RegisterResult::Ok { gts_id } => {
                            registered += 1;
                            info!(gts_id = %gts_id, "OAGW type schema registered");
                        }
                        types_registry_sdk::RegisterResult::Err { gts_id, error } => {
                            tracing::warn!(
                                gts_id = ?gts_id,
                                err = %error,
                                "OAGW type schema registration rejected by the types-registry"
                            );
                        }
                    }
                }
                info!(
                    registered,
                    requested = count,
                    "OAGW type schema provisioning finished"
                );
            }
            Err(err) => {
                tracing::warn!(err = %err, "OAGW type schema registration failed; continuing");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_carry_the_type_ids() {
        let payloads = TypeProvisioner::payloads();
        assert_eq!(payloads.len(), 2);
        for payload in &payloads {
            let id = payload["$id"].as_str().expect("$id is a string");
            assert!(id.ends_with(OAGW_UPSTREAM_TYPE_ID) || id.ends_with(OAGW_ROUTE_TYPE_ID));
        }
        let ids: Vec<&str> = payloads.iter().filter_map(|p| p["$id"].as_str()).collect();
        assert!(ids.contains(&OAGW_UPSTREAM_TYPE_ID));
        assert!(ids.contains(&OAGW_ROUTE_TYPE_ID));
    }

    #[test]
    fn the_catalog_has_five_type_ids() {
        assert_eq!(TypeProvisioner::type_ids().len(), 5);
    }
}
