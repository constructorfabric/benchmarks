// Updated: 2026-09-01 by Constructor Tech
//! Registration of OAGW's GTS type-schemas with the types registry.
//!
//! Every base type the gear either owns or accepts on the wire is announced at
//! startup. A failure is logged and swallowed: the registry is a *discovery*
//! service, not an authority, and a gear that cannot be described in it should
//! still serve the traffic it can resolve locally.

use toolkit_security::SecurityContext;
use types_registry_sdk::TypesRegistryClient;

use crate::gts;

/// The base types OAGW owns or accepts on the wire.
#[must_use]
pub fn declared_types() -> Vec<&'static str> {
    vec![
        gts::UPSTREAM_TYPE,
        gts::ROUTE_TYPE,
        gts::PROXY_TYPE,
        gts::AUTH_PLUGIN_TYPE,
        gts::GUARD_PLUGIN_TYPE,
        gts::TRANSFORM_PLUGIN_TYPE,
    ]
}

/// The schema dialect the types-registry stores under. A payload without a
/// non-empty `$schema` is classified as an *instance*, which then makes the
/// registry's ready-mode validation fail on the base type's own id — so the
/// field is not optional here.
const SCHEMA_DIALECT: &str = "http://json-schema.org/draft-07/schema#";

/// The base-type schema as the registry wants it: `$id` in its `gts://` URI
/// form, the dialect declared, and an object body.
#[must_use]
pub fn base_type_schema(base_type: &str) -> serde_json::Value {
    serde_json::json!({
        "$id": format!("gts://{base_type}"),
        "$schema": SCHEMA_DIALECT,
        "type": "object",
    })
}

/// Announce the gear's base types. Best effort by design.
pub async fn provision(client: &dyn TypesRegistryClient) {
    let schemas: Vec<serde_json::Value> = declared_types()
        .iter()
        .map(|t| base_type_schema(t))
        .collect();
    if schemas.is_empty() {
        return;
    }
    match client.register_type_schemas(schemas).await {
        Ok(results) => {
            for result in results {
                match result {
                    types_registry_sdk::RegisterResult::Ok { gts_id } => {
                        tracing::debug!(%gts_id, "oagw type registered");
                    }
                    types_registry_sdk::RegisterResult::Err { gts_id, error } => {
                        tracing::warn!(?gts_id, %error, "oagw type registration skipped");
                    }
                }
            }
        }
        Err(err) => tracing::warn!(%err, "oagw type registration skipped"),
    }
}

/// The anonymous identity the registration is attributed to: types are
/// registry-global, so they carry no tenant.
#[must_use]
pub fn registration_context() -> SecurityContext {
    SecurityContext::anonymous()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_type_is_a_gts_base_type() {
        for t in declared_types() {
            assert!(t.starts_with("gts."), "{t} must be a GTS identifier");
            assert!(t.ends_with('~'), "{t} must be a base type, not an instance");
        }
    }

    #[test]
    fn the_declared_set_is_complete() {
        let declared = declared_types();
        assert!(declared.contains(&gts::UPSTREAM_TYPE));
        assert!(declared.contains(&gts::ROUTE_TYPE));
        assert!(declared.contains(&gts::PROXY_TYPE));
        for kind in [
            gts::AUTH_PLUGIN_TYPE,
            gts::GUARD_PLUGIN_TYPE,
            gts::TRANSFORM_PLUGIN_TYPE,
        ] {
            assert!(declared.contains(&kind));
        }
    }

    #[test]
    fn registration_uses_an_anonymous_context() {
        let ctx = registration_context();
        assert_eq!(ctx.subject_tenant_id(), uuid::Uuid::nil());
    }

    #[test]
    fn the_payload_is_a_schema_and_not_an_instance() {
        // A `$schema`-less document is stored as an instance of its own base
        // type, and the registry's ready-mode validation then rejects the base
        // type's id outright. These two fields are what make it a schema.
        let payload = base_type_schema(gts::UPSTREAM_TYPE);
        let obj = payload.as_object().unwrap();
        assert_eq!(
            obj.get("$id").and_then(serde_json::Value::as_str),
            Some("gts://gts.cf.core.oagw.upstream.v1~")
        );
        assert!(
            obj.get("$schema")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty())
        );
        assert_eq!(
            obj.get("type").and_then(serde_json::Value::as_str),
            Some("object")
        );
    }
}
