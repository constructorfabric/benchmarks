//! Resolving [`PluginValidator`] (DESIGN "Resolution Algorithm").
//!
//! Validates plugin references during management writes: UUID-backed
//! references resolve against the control-plane store (family must match the
//! binding), named references resolve against the in-process built-in
//! registries. Catalog-only identifiers are rejected.

use uuid::Uuid;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::models::plugin_gts;
use crate::domain::plugin::PluginError;
use crate::domain::services::PluginValidator;
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry,
};
use crate::infra::storage::SharedStore;

/// [`PluginValidator`] over the store + built-in registries.
pub struct ResolvingPluginValidator {
    store: SharedStore,
    auth: AuthPluginRegistry,
    guard: GuardPluginRegistry,
    transform: TransformPluginRegistry,
}

impl ResolvingPluginValidator {
    /// Create the validator.
    #[must_use]
    pub fn new(
        store: SharedStore,
        auth: AuthPluginRegistry,
        guard: GuardPluginRegistry,
        transform: TransformPluginRegistry,
    ) -> Self {
        Self {
            store,
            auth,
            guard,
            transform,
        }
    }
}

impl PluginValidator for ResolvingPluginValidator {
    fn validate_ref(&self, tenant_id: Uuid, plugin_ref: &str) -> OagwResult<()> {
        let Some(family) = family_of(plugin_ref) else {
            return Err(OagwError::validation(format!(
                "unknown plugin reference {plugin_ref:?}"
            )));
        };

        if plugin_gts::is_uuid_backed(plugin_ref) {
            let uuid =
                Uuid::parse_str(plugin_gts::instance_of(plugin_ref)).expect("uuid plugin ref");
            let Some(stored) = self.store.plugin_by_id(tenant_id, uuid) else {
                return Err(OagwError::validation(format!(
                    "unknown custom plugin reference {plugin_ref:?}"
                )));
            };
            if stored.record.plugin_type != family {
                return Err(OagwError::validation(format!(
                    "plugin reference {plugin_ref:?} is used in a {family} binding but is a {} plugin",
                    stored.record.plugin_type
                )));
            }
            return Ok(());
        }

        // Named built-in plugin in the matching family registry.
        let result = match family {
            "auth" => self.auth.resolve(plugin_ref).map(|_| ()),
            "guard" => self.guard.resolve(plugin_ref).map(|_| ()),
            _ => self.transform.resolve(plugin_ref).map(|_| ()),
        };
        result.map_err(|e: PluginError| match e {
            PluginError::Config { message }
            | PluginError::Internal { message }
            | PluginError::Rejected { message, .. } => OagwError::validation(message),
        })
    }
}

/// The plugin family encoded in a GTS reference prefix.
#[must_use]
pub fn family_of(plugin_ref: &str) -> Option<&'static str> {
    if plugin_ref.starts_with(plugin_gts::AUTH) {
        Some("auth")
    } else if plugin_ref.starts_with(plugin_gts::GUARD) {
        Some("guard")
    } else if plugin_ref.starts_with(plugin_gts::TRANSFORM) {
        Some("transform")
    } else {
        None
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn family_detection() {
        assert_eq!(
            family_of("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
            Some("auth")
        );
        assert_eq!(
            family_of("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
            Some("guard")
        );
        assert_eq!(
            family_of("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
            Some("transform")
        );
        assert_eq!(family_of("gts.cf.core.oagw.whatever.v1~x"), None);
    }
}
