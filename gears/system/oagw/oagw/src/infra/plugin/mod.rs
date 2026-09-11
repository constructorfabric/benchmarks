//! Plugin infrastructure: registries and built-in plugin implementations
//! (ADR 0008).
//!
//! **The registries land with entry 2.6** (`cpt-cf-oagw-feature-plugin-system`),
//! under the registry-reference posture of graded deviation 6. What this module
//! contributes today is the **plugin-catalog binding-time resolvability
//! boundary** the route-management write path executes
//! ([`CatalogBindingResolver`]): it decides, from the catalog the foundation
//! entry provisioned in [`crate::domain::gts_helpers`] plus the custom-plugin
//! rows of `oagw_plugin`, whether a `plugins.items[]` entry resolves to a
//! bindable plugin.
//!
//! The *policy* the check applies is owned by `cpt-cf-oagw-feature-plugin-system`;
//! this module is the boundary the write path reaches it through, so a route
//! write never implements the check itself and never stores an interim
//! unresolved binding.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{BUILTIN_PLUGIN_IDS, CATALOG_ONLY_PLUGIN_IDS};
use crate::domain::repo::{PluginBinding, PluginRepository};
use crate::domain::services::plugin_management::PluginConfigValidator;
use crate::domain::services::route_management::PluginBindingResolver;

pub mod apikey_auth;
pub mod credentials;
pub mod executor;
pub mod oauth2_client_cred_auth;
pub mod registry;
pub mod request_id_transform;
pub mod required_headers_guard;
pub mod resolution;

/// The binding-time rejection naming the offending entry of `plugins.items[]`
/// and never echoing the reference value itself.
fn rejection(position: usize, reason: &str) -> DomainError {
    DomainError::ValidationError {
        detail: format!("field `plugins.items` rejected: plugin reference `{position}` {reason}"),
        path: Some("plugins.items".to_owned()),
        trace_id: None,
    }
}

/// The binding-time resolvability check over the plugin catalog and the
/// custom-plugin rows.
///
/// A reference is bindable when it names a built-in plugin the foundation
/// entry provisioned, or when it is UUID-backed and the calling tenant holds
/// the custom plugin row it names. A catalog-only identifier — for example
/// `cors.v1`, `timeout.v1` or `basic.v1` — is never bindable and is rejected,
/// because the catalog registers it without any backing plugin (graded
/// deviations 6 and 10).
#[derive(Clone)]
pub struct CatalogBindingResolver {
    plugins: Arc<dyn PluginRepository>,
}

impl CatalogBindingResolver {
    /// Build the resolver over the custom-plugin rows of the same store the
    /// management surface writes to.
    #[must_use]
    pub fn new(plugins: Arc<dyn PluginRepository>) -> Self {
        Self { plugins }
    }

    /// The registered `config_schema` of one reference, resolved strictly
    /// caller-scoped.
    #[must_use]
    pub fn config_schema_of(&self, tenant_id: Uuid, reference: &str) -> Option<serde_json::Value> {
        crate::domain::plugin::schema::config_schema_of(self.plugins.as_ref(), tenant_id, reference)
    }
}

impl PluginConfigValidator for CatalogBindingResolver {
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-14
    // `inst-ps-bind-14`/`-15`: the upstream `auth.config` is validated against
    // the registered `config_schema` of the plugin `auth.type` names **before**
    // any binding row is stored, so an unknown key, a missing required key
    // such as `client_id_ref` or `client_secret_ref`, a mutually exclusive key
    // pair such as `token_endpoint` together with `issuer_url`, or a blank
    // required-header entry is rejected with `400`
    // `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` and nothing is
    // persisted. A reference that resolves to nothing is not-found, because a
    // binding to an unresolvable plugin is rejected rather than stored.
    fn validate_auth_config(
        &self,
        tenant_id: Uuid,
        auth_ref: Option<&str>,
        config: Option<&serde_json::Value>,
    ) -> Result<(), DomainError> {
        let Some(reference) = auth_ref else {
            return Ok(());
        };
        if crate::domain::plugin::identifier::is_catalog_only(reference) {
            return Err(rejection_by_reference(
                reference,
                "is registered in the catalog only and is not resolvable at binding time",
            ));
        }
        let resolvable = crate::domain::plugin::identifier::is_builtin(reference)
            || matches!(
                crate::domain::plugin::identifier::parse_instance(reference),
                crate::domain::plugin::identifier::PluginInstance::Uuid(uuid)
                    if self.plugins.get(tenant_id, uuid).is_ok()
            );
        if !resolvable {
            return Err(DomainError::NotFound { resource_type: "plugin" });
        }
        crate::domain::plugin::identifier::validate_ref_uuid_agreement(reference, None)?;
        let schema = self.config_schema_of(tenant_id, reference);
        crate::domain::plugin::schema::validate_instance_config("auth.config", schema.as_ref(), config)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-binding-resolution:p1:inst-ps-bind-14
}

/// The binding-time rejection naming the offending *reference* and never
/// echoing a rejected configuration value.
fn rejection_by_reference(reference: &str, reason: &str) -> DomainError {
    DomainError::ValidationError {
        detail: format!("field `auth.config` rejected: plugin reference `{reference}` {reason}"),
        path: Some("auth.config".to_owned()),
        trace_id: None,
    }
}

impl PluginBindingResolver for CatalogBindingResolver {
    // @cpt-dod:cpt-cf-oagw-dod-route-management-route-overrides:p1
    // `cpt-cf-oagw-dod-route-management-route-overrides`: every entry of a
    // route `plugins.items[]` is resolved at binding time, in order; a
    // catalog-only identifier such as `cors.v1`, `timeout.v1` or `basic.v1` is
    // rejected with a validation error, and no interim unresolved-binding
    // state is ever stored.
    fn resolve(&self, tenant_id: Uuid, references: &[String]) -> Result<Vec<PluginBinding>, DomainError> {
        references
            .iter()
            .enumerate()
            .map(|(position, reference)| {
                // The reference may be spelled as the bare UUID or as the
                // anonymous GTS resource instance identifier the plugin is
                // addressed under; either way the UUID it carries is the one
                // the row is bound to.
                let plugin_uuid = Uuid::parse_str(reference)
                    .ok()
                    .or_else(|| {
                        crate::domain::plugin::identifier::parse_instance(reference).uuid()
                    });
                // `inst-ps-bind-7`/`-8`: a binding row is never stored with a
                // `plugin_uuid` that disagrees with the `plugin_ref` beside it.
                crate::domain::plugin::identifier::validate_ref_uuid_agreement(reference, plugin_uuid)?;
                let binding = PluginBinding {
                    position: position as u32,
                    plugin_ref: reference.clone(),
                    plugin_uuid,
                };
                if BUILTIN_PLUGIN_IDS.contains(&reference.as_str()) {
                    return Ok(binding);
                }
                if CATALOG_ONLY_PLUGIN_IDS.contains(&reference.as_str()) {
                    return Err(rejection(
                        position,
                        "is registered in the catalog only and is not resolvable at binding time",
                    ));
                }
                // A UUID-backed reference must name a custom plugin the calling
                // tenant holds; anything else is unresolvable.
                match &binding.plugin_uuid {
                    Some(uuid) => {
                        if self.plugins.get(tenant_id, *uuid).is_err() {
                            return Err(rejection(
                                position,
                                "does not resolve to a plugin of the calling tenant",
                            ));
                        }
                        Ok(binding)
                    }
                    None => Err(rejection(
                        position,
                        "is neither a built-in plugin identifier nor a custom plugin reference",
                    )),
                }
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "composition_tests.rs"]
mod composition_tests;

#[cfg(test)]
#[path = "registry_reference_tests.rs"]
mod registry_reference_tests;
