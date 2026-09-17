//! Type provisioning: registers the OAGW GTS type schemas with the
//! `types-registry` dependency.
//!
//! Registration is best-effort: if the dependency is absent, or a registration
//! call fails, the gear still boots and logs a warning. The control plane does
//! not need the registry to answer requests — the GTS ids are statically known.

use std::sync::Arc;

use serde_json::json;
use types_registry_sdk::TypesRegistryClient;

use crate::domain::gts_helpers;

/// Register the OAGW resource type schemas.
pub async fn register_types(hub: Arc<toolkit::client_hub::ClientHub>) {
    let Some(registry) = hub.try_get::<dyn TypesRegistryClient>() else {
        tracing::warn!("types-registry client is not wired; OAGW types were not registered");
        return;
    };

    let entities = vec![
        type_schema(gts_helpers::UPSTREAM_TYPE, "OagwUpstream"),
        type_schema(gts_helpers::ROUTE_TYPE, "OagwRoute"),
        type_schema(gts_helpers::PLUGIN_TYPE, "OagwPlugin"),
        type_schema(gts_helpers::AUTH_PLUGIN_TYPE, "OagwAuthPlugin"),
        type_schema(gts_helpers::GUARD_PLUGIN_TYPE, "OagwGuardPlugin"),
        type_schema(gts_helpers::TRANSFORM_PLUGIN_TYPE, "OagwTransformPlugin"),
        type_schema(gts_helpers::PROTOCOL_TYPE, "OagwProtocol"),
    ];

    match registry.register(entities).await {
        Ok(results) => {
            for result in results {
                match result {
                    types_registry_sdk::RegisterResult::Ok { gts_id } => {
                        tracing::debug!(gts_id = %gts_id, "OAGW type registered");
                    }
                    types_registry_sdk::RegisterResult::Err { gts_id, error } => {
                        tracing::warn!(
                            gts_id = ?gts_id,
                            error = %error,
                            "OAGW type registration failed"
                        );
                    }
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "types-registry rejected the OAGW type batch");
        }
    }
}

/// Build a minimal JSON-Schema payload for an OAGW type schema.
fn type_schema(gts_id: &'static str, title: &'static str) -> serde_json::Value {
    json!({
        "$id": gts_id,
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "title": title,
        "description": concat!("OAGW resource type registered by the oagw gear (", env!("CARGO_PKG_NAME"), ")"),
        "additionalProperties": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_registry_is_non_fatal() {
        // No client registered on the hub: the call must return instead of
        // panicking, and log only a warning.
        register_types(Arc::new(toolkit::client_hub::ClientHub::new())).await;
    }

    #[test]
    fn type_schemas_carry_the_gts_id() {
        let schema = type_schema(gts_helpers::UPSTREAM_TYPE, "OagwUpstream");
        assert_eq!(schema["$id"], gts_helpers::UPSTREAM_TYPE);
        assert_eq!(schema["type"], "object");
    }
}
