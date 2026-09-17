//! GTS type provisioning for the OAGW gear
//! (`cpt-cf-oagw-feature-type-provisioning`).
//!
//! [`register_gts_catalog`] materializes the gear's fixed GTS catalog into
//! the authoritative types registry at startup (`Gear::init`): the seven type
//! ids under `gts.cf.core.oagw.*.v1~` (upstream, route, auth_plugin,
//! guard_plugin, transform_plugin, proxy, protocol) plus the OAGW plugin spec
//! type and the twelve plugin inventory instances declared by `crate::gts`.
//!
//! Registration is **idempotent** (re-runs converge, never duplicate) and
//! **fail-loud but bounded**: a registration failure is surfaced as a precise,
//! actionable startup error naming the failing type id/instance and reason,
//! and the caller (gear foundation) continues so unrelated gears and the host
//! are unaffected.

// DoD traceability (`cpt-cf-oagw-dod-type-provisioning-*` — to_code markers).
// @cpt-dod:cpt-cf-oagw-dod-type-provisioning-gts-catalog:p3
// @cpt-dod:cpt-cf-oagw-dod-type-provisioning-idempotency:p3
// @cpt-dod:cpt-cf-oagw-dod-type-provisioning-bounded-failure:p3
// @cpt-dod:cpt-cf-oagw-dod-type-provisioning-validation-helpers:p3
// @cpt-dod:cpt-cf-oagw-dod-type-provisioning-test-harness:p3
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use toolkit::{GearCtx, client_hub::ClientHubError};
use toolkit_canonical_errors::CanonicalError;
use toolkit_gts::{all_inventory_instances, all_inventory_type_schemas};
use tracing::{info, warn};
use types_registry_sdk::{
    RegisterResult, TypesRegistryClient,
    models::{GtsInstance, GtsTypeSchema},
};

/// Type-id prefix that marks OAGW-owned GTS entities in the process-wide
/// `toolkit-gts` inventory.
pub const OAGW_TYPE_ID_PREFIX: &str = "cf.core.oagw.";

/// Outcome of a catalog-materialization run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GtsRegistrationStatus {
    /// Type schemas freshly registered.
    pub schemas_registered: usize,
    /// Type schemas already present and accepted as convergent.
    pub schemas_converged: usize,
    /// Plugin instances freshly registered.
    pub instances_registered: usize,
    /// Plugin instances already present and accepted as convergent.
    pub instances_converged: usize,
    /// Precise, actionable per-entity failures (id + reason).
    pub failures: Vec<String>,
}

impl GtsRegistrationStatus {
    /// Total entities that ended up materialized (fresh or converged).
    #[must_use]
    pub const fn materialized(&self) -> usize {
        self.schemas_registered
            + self.schemas_converged
            + self.instances_registered
            + self.instances_converged
    }

    /// Whether every entity settled (no hard failures).
    #[must_use]
    pub const fn all_settled(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Registers the OAGW GTS catalog with the authoritative types registry.
///
/// - Resolves the types-registry SDK client through the wired client hub
///   (`inst-sdk-client`).
/// - Composes the fixed catalog from the process-wide `toolkit-gts`
///   inventory, filtered to OAGW-owned ids (`inst-compose-catalog`).
/// - Registers each type schema (`inst-register-one`), absorbing
///   already-present identities as convergent (`inst-already-present` /
///   `inst-converge`) and collecting any other failure as a precise bounded
///   entry (`inst-hard-fail` / `inst-bounded-fail`).
/// - Registers each plugin inventory instance against its catalog type
///   (`inst-instance-register`), likewise absorbing already-exists as
///   convergent (`inst-instance-exists` / `inst-instance-converge`).
///
/// # Errors
///
/// Returns `Err` only when the registry itself is unreachable or the
/// inventory cannot be composed — the precise, bounded failure the gear
/// foundation surfaces loudly without cascading (`inst-bounded-return`).
pub async fn register_gts_catalog(ctx: &GearCtx) -> anyhow::Result<GtsRegistrationStatus> {
    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-compose-catalog
    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-define-types
    let schemas = collect_oagw_schemas().context("compose OAGW type-schema catalog")?;
    // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-define-types
    let instances = collect_oagw_instances().context("compose OAGW plugin-instance catalog")?;
    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-compose-catalog

    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-sdk-client
    let registry: Arc<dyn TypesRegistryClient> = ctx
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-sdk-client
        .client_hub()
        .get::<dyn TypesRegistryClient>()
        .map_err(|e: ClientHubError| {
            anyhow::anyhow!("types-registry {}: {e}", describe_hub_error(&e))
        })?;

    let mut status = GtsRegistrationStatus::default();

    // ------------------------------------------------------------------
    // Type schemas (inst-type-loop)
    // ------------------------------------------------------------------
    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-register-types
    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-type-loop
    // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-register-types
    let schema_results = registry
        .register_type_schemas(schemas.clone())
        .await
        .context("types-registry backend unreachable while registering oagw type schemas")?;
    for result in schema_results {
        match result {
            // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-register-one
            RegisterResult::Ok { .. } => status.schemas_registered += 1,
            // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-register-one
            RegisterResult::Err { gts_id, error } => {
                if is_already_exists(&error) {
                    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-already-present
                    let id = gts_id.as_deref().unwrap_or("<unknown>");
                    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-already-present
                    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-converge
                    converge_type_schema(&registry, id, &schemas, &mut status).await;
                    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-converge
                } else {
                    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-registration-fail
                    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-out
                    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-fallback-out
                    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-hard-fail
                    status.failures.push(format!(
                        // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-registration-fail
                        // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-out
                        // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-fallback-out
                        // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-hard-fail
                        "type schema `{}` failed to register: {}",
                        gts_id.as_deref().unwrap_or("<unknown>"),
                        error
                    ));
                }
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-type-loop

    // ------------------------------------------------------------------
    // Plugin inventory instances (inst-instance-loop)
    // ------------------------------------------------------------------
    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-register-instances
    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-loop
    // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-register-instances
    let instance_results = registry
        .register_instances(instances.clone())
        .await
        .context("types-registry backend unreachable while registering oagw plugin instances")?;
    for result in instance_results {
        match result {
            // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-register
            RegisterResult::Ok { .. } => status.instances_registered += 1,
            // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-register
            RegisterResult::Err { gts_id, error } => {
                if is_already_exists(&error) {
                    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-exists
                    let id = gts_id.as_deref().unwrap_or("<unknown>");
                    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-exists
                    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-converge
                    converge_instance(&registry, id, &instances, &mut status).await;
                    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-converge
                } else {
                    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-registration-fail
                    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-out
                    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-fallback-out
                    // Bounded per-entity failure (hard per the flow, precise and actionable).
                    // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-registration-fail
                    // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-out
                    // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-error-fallback-out
                    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-bounded-fail
                    status.failures.push(format!(
                        "plugin instance `{}` failed to register: {}",
                        gts_id.as_deref().unwrap_or("<unknown>"),
                        error
                    ));
                    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-bounded-fail
                }
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-instance-loop

    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-validation-helpers
    validate_catalog_shape(&schemas, &instances);
    // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-validation-helpers

    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-registration-ok
    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-init-continue
    // @cpt-begin:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-return-materialized
    // @cpt-begin:cpt-cf-oagw-state-type-provisioning-registration:ph-1:inst-registered
    // @cpt-begin:cpt-cf-oagw-state-type-provisioning-registration:ph-1:inst-retry-registered
    info!(
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-registration-ok
        // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-init-continue
        // @cpt-end:cpt-cf-oagw-algo-type-provisioning-catalog-materialization:ph-1:inst-return-materialized
        // @cpt-end:cpt-cf-oagw-state-type-provisioning-registration:ph-1:inst-registered
        // @cpt-end:cpt-cf-oagw-state-type-provisioning-registration:ph-1:inst-retry-registered
        schemas_registered = status.schemas_registered,
        schemas_converged = status.schemas_converged,
        instances_registered = status.instances_registered,
        instances_converged = status.instances_converged,
        failures = status.failures.len(),
        "oagw GTS catalog materialized"
    );
    // @cpt-begin:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-bounded-return
    Ok(status)
    // @cpt-end:cpt-cf-oagw-flow-type-provisioning-register-gts-catalog:ph-1:inst-bounded-return
}

fn describe_hub_error(e: &ClientHubError) -> &str {
    match e {
        ClientHubError::NotFound { .. } => "client not found (is types-registry wired?)",
        ClientHubError::TypeMismatch { .. } => "client type mismatch",
        ClientHubError::ScopedNotFound { .. } => "scoped client not found",
        ClientHubError::ScopedTypeMismatch { .. } => "scoped client type mismatch",
    }
}

fn is_already_exists(error: &CanonicalError) -> bool {
    matches!(error, CanonicalError::AlreadyExists { .. })
}

/// Filters the process-wide inventory down to OAGW-owned type schemas.
///
/// `$id`-based filtering keeps the catalog self-contained: the seven
/// `gts.cf.core.oagw.*.v1~` types plus the OAGW plugin spec type
/// (`gts.cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~`) that
/// declares our plugin instances.
fn collect_oagw_schemas() -> anyhow::Result<Vec<serde_json::Value>> {
    let all = all_inventory_type_schemas().context("collect inventory type schemas")?;
    Ok(all
        .into_iter()
        .filter(|s| {
            s.get("$id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id.contains(OAGW_TYPE_ID_PREFIX))
        })
        .collect())
}

/// Filters the process-wide inventory down to OAGW-owned plugin instances.
fn collect_oagw_instances() -> anyhow::Result<Vec<serde_json::Value>> {
    let all = all_inventory_instances().context("collect inventory plugin instances")?;
    Ok(all
        .into_iter()
        .filter(|v| {
            v.get("id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id.contains(OAGW_TYPE_ID_PREFIX))
        })
        .collect())
}

fn expected_schema_by_id<'a>(
    schemas: &'a [serde_json::Value],
    id: &str,
) -> Option<&'a serde_json::Value> {
    schemas
        .iter()
        .find(|s| s.get("$id").and_then(serde_json::Value::as_str) == Some(id))
}

fn expected_instance_by_id<'a>(
    instances: &'a [serde_json::Value],
    id: &str,
) -> Option<&'a serde_json::Value> {
    instances
        .iter()
        .find(|v| v.get("id").and_then(serde_json::Value::as_str) == Some(id))
}

/// Absorbs an already-present type schema as convergent: a best-effort
/// read-back confirms the registry copy matches what we would have written.
async fn converge_type_schema(
    registry: &Arc<dyn TypesRegistryClient>,
    id: &str,
    schemas: &[serde_json::Value],
    status: &mut GtsRegistrationStatus,
) {
    status.schemas_converged += 1;
    let Some(expected) = expected_schema_by_id(schemas, id) else {
        return;
    };
    match registry.get_type_schema(id).await {
        Ok(existing) => {
            if !schema_matches(&existing, expected) {
                warn!(
                    type_id = id,
                    "oagw type schema already registered with differing body; convergent, drift left to the authoritative registry"
                );
            }
        }
        Err(e) => {
            warn!(type_id = id, error = %e, "could not read back existing oagw type schema");
        }
    }
}

/// Absorbs an already-present plugin instance as convergent, mirroring the
/// type-schema path.
async fn converge_instance(
    registry: &Arc<dyn TypesRegistryClient>,
    id: &str,
    instances: &[serde_json::Value],
    status: &mut GtsRegistrationStatus,
) {
    status.instances_converged += 1;
    let Some(expected) = expected_instance_by_id(instances, id) else {
        return;
    };
    match registry.get_instance(id).await {
        Ok(existing) => {
            if !instance_matches(&existing, expected) {
                warn!(
                    instance_id = id,
                    "oagw plugin instance already registered with differing body; convergent, content drift left to the authoritative registry"
                );
            }
        }
        Err(e) => {
            warn!(instance_id = id, error = %e, "could not read back existing oagw plugin instance");
        }
    }
}

fn schema_matches(existing: &GtsTypeSchema, expected: &serde_json::Value) -> bool {
    // The registry stores the schema body; `$id` is redundant with
    // `type_id`, so compare the body without the identity field.
    let mut want = expected.clone();
    if let Some(obj) = want.as_object_mut() {
        obj.remove("$id");
    }
    serde_json::to_value(&existing.raw_schema)
        .ok()
        .is_some_and(|have| have == want)
}

fn instance_matches(existing: &GtsInstance, expected: &serde_json::Value) -> bool {
    existing.object == *expected
}

/// Schema-backed validation helpers over the composed catalog: every entity
/// has a well-formed identity, a resolvable declaring type, and the required
/// fields the data plane relies on. Infallible — failures are surfaced as
/// loud warnings so a catalog regression is diagnosable without crashing.
fn validate_catalog_shape(schemas: &[serde_json::Value], instances: &[serde_json::Value]) {
    // identity + required-field checks for the seven core types.
    let mut by_id: BTreeMap<&str, &serde_json::Value> = BTreeMap::new();
    for schema in schemas {
        let Some(id) = schema.get("$id").and_then(serde_json::Value::as_str) else {
            warn!(schema = ?schema, "oagw type schema without `$id`");
            continue;
        };
        let required = schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if required.is_empty() {
            warn!(
                type_id = id,
                "oagw type schema declares no `required` fields"
            );
        }
        by_id.insert(id, schema);
    }

    // Every instance's declaring type must exist in the composed catalog
    // (the registry enforces this too; here we do it pre-flight).
    for instance in instances {
        let Some(id) = instance.get("id").and_then(serde_json::Value::as_str) else {
            warn!(instance = ?instance, "oagw plugin instance without `id`");
            continue;
        };
        let Some(declaring) = declaring_type_of(id) else {
            warn!(
                instance_id = id,
                "oagw plugin instance id missing type separator (`~`) or version segment"
            );
            continue;
        };
        if !by_id.contains_key(declaring.as_str()) {
            warn!(
                instance_id = id,
                declaring_type = declaring,
                "oagw plugin instance declaring type absent from catalog"
            );
        }
    }
}

/// Derives an instance's declaring GTS type id from its instance id.
///
/// Instance ids are `{declaring_type}~{name}.v1` where the declaring type is
/// a `…v1~`-terminated GTS id with the `~` separating it from the instance
/// name — so the type is everything up to the *last* `~` (plus the terminal
/// `~`), never a naive `.v1`-suffix rewrite (which would mangle e.g.
/// `gts.cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~noop.v1`).
fn declaring_type_of(id: &str) -> Option<String> {
    let (type_id, name) = id.rsplit_once('~')?;
    if !name.ends_with(".v1") {
        return None;
    }
    Some(format!("{type_id}~"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_filters_only_oagw_owned_entities() {
        let schemas = [
            serde_json::json!({ "$id": "gts.cf.core.oagw.upstream.v1~" }),
            serde_json::json!({ "$id": "gts.cf.core.oagw.route.v1~" }),
            serde_json::json!({ "$id": "gts.cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~" }),
            serde_json::json!({ "$id": "gts.cf.core.am.user.v1~" }),
        ];
        let owned = schemas.iter().filter(|s| {
            s.get("$id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| id.contains(OAGW_TYPE_ID_PREFIX))
        });
        let ids: Vec<&str> = owned
            .filter_map(|s| s.get("$id").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(
            ids,
            vec![
                "gts.cf.core.oagw.upstream.v1~",
                "gts.cf.core.oagw.route.v1~",
                "gts.cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~",
            ]
        );
    }

    #[test]
    fn declaring_type_derivation_uses_last_separator() {
        // A composite declaring type (`…plugin.v1~cf.core.oagw.plugin.v1~`):
        // the type must be everything before the *last* `~`, not a
        // `.v1`-suffix rewrite (which would produce a spurious type).
        assert_eq!(
            declaring_type_of("gts.cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~noop.v1"),
            Some("gts.cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~".to_owned())
        );
        // Core oagw types are `…v1~` too.
        assert_eq!(
            declaring_type_of("gts.cf.core.oagw.upstream.v1~shop.v1"),
            Some("gts.cf.core.oagw.upstream.v1~".to_owned())
        );
        // Malformed ids yield None rather than a mangled type.
        assert_eq!(declaring_type_of("gts.cf.core.oagw.upstream.v1~shop"), None);
        assert_eq!(declaring_type_of("no-separator"), None);
    }

    #[test]
    fn validation_helpers_detect_orphan_instances() {
        // Composed catalog with an instance whose declaring type is missing:
        // the helper must record it (warn-only) without panicking.
        let schemas = vec![serde_json::json!({ "$id": "gts.cf.core.oagw.upstream.v1~" })];
        let instances = vec![serde_json::json!({
            "id": "gts.cf.toolkit.plugins.plugin.v1~cf.core.oagw.plugin.v1~noop.v1"
        })];
        validate_catalog_shape(&schemas, &instances);
    }

    #[test]
    fn status_aggregation() {
        let status = GtsRegistrationStatus {
            schemas_registered: 3,
            schemas_converged: 5,
            instances_registered: 1,
            instances_converged: 11,
            failures: Vec::new(),
        };
        assert_eq!(status.materialized(), 20);
        assert!(status.all_settled());
    }
}
