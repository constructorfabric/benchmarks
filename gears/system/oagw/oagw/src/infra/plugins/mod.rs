//! Builtin plugin registries (ADR 0002).
//!
//! One registry per plugin kind, keyed by GTS instance id, assembled by
//! [`PluginRegistry::with_builtins`]. The facade resolves a
//! [`ResolvedBinding`] — a builtin GTS id or a custom plugin UUID — into a
//! concrete [`ResolvedPlugin`] plus its effective (binding-site merged)
//! configuration, consulting the plugin repository for custom definitions.

pub mod auth;
pub mod guard;
pub mod transform;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::domain::error::{DataPlaneError, ErrorExtensions};
use crate::domain::models::PluginRef;
use crate::domain::plugin::{
    AuthPlugin, GuardDecision, GuardPlugin, PluginContext, PluginError, TransformPlugin,
};
use crate::domain::repo::PluginRepo;
use crate::domain::services::data_plane::ResolvedBinding;
use crate::gts_helpers;

/// A resolved, kind-specific plugin ready to run a phase hook.
#[derive(Clone)]
pub enum ResolvedPlugin {
    Auth(Arc<dyn AuthPlugin>),
    Guard(Arc<dyn GuardPlugin>),
    Transform(Arc<dyn TransformPlugin>),
}

impl ResolvedPlugin {
    /// GTS instance id of the resolved plugin.
    #[must_use]
    pub fn id(&self) -> &'static str {
        match self {
            Self::Auth(p) => p.id(),
            Self::Guard(p) => p.id(),
            Self::Transform(p) => p.id(),
        }
    }

    /// Plugin kind (`auth` / `guard` / `transform`).
    #[must_use]
    pub fn plugin_type(&self) -> &'static str {
        match self {
            Self::Auth(p) => p.plugin_type(),
            Self::Guard(p) => p.plugin_type(),
            Self::Transform(p) => p.plugin_type(),
        }
    }

    /// Run the request-phase hook for this plugin's kind.
    ///
    /// # Errors
    /// `PluginError` when the phase mismatches the kind or the hook fails.
    pub async fn run_request(
        &self,
        ctx: &PluginContext,
        headers: &mut http::HeaderMap,
    ) -> Result<Option<GuardDecision>, PluginError> {
        match self {
            Self::Auth(p) => {
                p.authenticate(ctx, headers).await?;
                Ok(None)
            }
            Self::Guard(p) => Ok(Some(p.guard_request(ctx, headers).await?)),
            Self::Transform(p) => {
                p.transform_request(ctx, headers).await?;
                Ok(None)
            }
        }
    }

    /// Run the response-phase hook for this plugin's kind.
    ///
    /// # Errors
    /// `PluginError` when the phase mismatches the kind or the hook fails.
    pub async fn run_response(
        &self,
        ctx: &PluginContext,
        headers: &mut http::HeaderMap,
    ) -> Result<Option<GuardDecision>, PluginError> {
        match self {
            Self::Guard(p) => Ok(Some(p.guard_response(ctx, headers).await?)),
            Self::Transform(p) => {
                p.transform_response(ctx, headers).await?;
                Ok(None)
            }
            Self::Auth(_) => Ok(None),
        }
    }
}

/// A resolved plugin together with its effective configuration.
#[derive(Clone)]
pub struct ResolvedBuiltin {
    pub plugin: ResolvedPlugin,
    pub config: serde_json::Value,
}

/// Combined plugin registry: one map per kind plus the custom-plugin store.
pub struct PluginRegistry {
    auth: HashMap<&'static str, Arc<dyn AuthPlugin>>,
    guard: HashMap<&'static str, Arc<dyn GuardPlugin>>,
    transform: HashMap<&'static str, Arc<dyn TransformPlugin>>,
    plugin_repo: Arc<dyn PluginRepo>,
}

impl PluginRegistry {
    /// Assemble the registry with every instantiable builtin (ADR 0002,
    /// 0008, 0009). `token_cache_*` bound the `OAuth2` token cache.
    #[must_use]
    pub fn with_builtins(
        token_cache_ttl: Duration,
        token_cache_capacity: usize,
        plugin_repo: Arc<dyn PluginRepo>,
    ) -> Self {
        let mut auth: HashMap<&'static str, Arc<dyn AuthPlugin>> = HashMap::new();
        auth.insert(
            gts_helpers::AUTH_PLUGIN_NOOP,
            Arc::new(auth::NoopAuthPlugin),
        );
        auth.insert(
            gts_helpers::AUTH_PLUGIN_APIKEY,
            Arc::new(auth::ApiKeyAuthPlugin),
        );
        auth.insert(
            gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED,
            Arc::new(auth::OAuth2ClientCredAuthPlugin::new(
                toolkit_auth::oauth2::ClientAuthMethod::Form,
                token_cache_ttl,
                token_cache_capacity,
            )),
        );
        auth.insert(
            gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC,
            Arc::new(auth::OAuth2ClientCredAuthPlugin::new(
                toolkit_auth::oauth2::ClientAuthMethod::Basic,
                token_cache_ttl,
                token_cache_capacity,
            )),
        );

        let mut guard: HashMap<&'static str, Arc<dyn GuardPlugin>> = HashMap::new();
        guard.insert(
            gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS,
            Arc::new(guard::RequiredHeadersGuardPlugin),
        );

        let mut transform: HashMap<&'static str, Arc<dyn TransformPlugin>> = HashMap::new();
        transform.insert(
            gts_helpers::TRANSFORM_PLUGIN_REQUEST_ID,
            Arc::new(transform::RequestIdTransformPlugin),
        );

        Self {
            auth,
            guard,
            transform,
            plugin_repo,
        }
    }

    /// Resolve a binding (builtin id or custom-uuid reference) to a
    /// concrete plugin plus its effective merged configuration.
    ///
    /// # Errors
    /// `PluginNotFound` when the reference is unknown or catalog-only.
    pub async fn resolve(
        &self,
        binding: &ResolvedBinding,
    ) -> Result<ResolvedBuiltin, DataPlaneError> {
        match &binding.plugin_ref {
            PluginRef::BuiltinId(id) => self.resolve_builtin(id, binding.config.clone()),
            PluginRef::Custom(uuid) => {
                let plugin = self
                    .plugin_repo
                    .get_any_tenant(*uuid)
                    .await
                    .ok_or_else(|| DataPlaneError::PluginNotFound {
                        detail: format!("custom plugin '{uuid}' does not exist"),
                        extensions: ErrorExtensions::default(),
                    })?;
                let config = merge_config(plugin.config.clone(), binding.config.clone());
                self.resolve_builtin(&plugin.builtin_type, config)
            }
        }
    }

    /// Resolve an `auth` block (upstream `auth.type` + `auth.config`).
    ///
    /// # Errors
    /// `PluginNotFound` when the auth type is unknown or catalog-only.
    #[allow(clippy::result_large_err)] // rich RFC 9457 error carrier by design
    pub fn resolve_auth(
        &self,
        plugin_type: &str,
        config: serde_json::Value,
    ) -> Result<ResolvedBuiltin, DataPlaneError> {
        self.resolve_builtin(plugin_type, config)
    }

    /// Resolve a builtin reference directly.
    #[allow(clippy::result_large_err)] // rich RFC 9457 error carrier by design
    fn resolve_builtin(
        &self,
        id: &str,
        config: serde_json::Value,
    ) -> Result<ResolvedBuiltin, DataPlaneError> {
        let not_found = |detail: String| DataPlaneError::PluginNotFound {
            detail,
            extensions: ErrorExtensions::default(),
        };
        if let Some(p) = self.auth.get(id) {
            return Ok(ResolvedBuiltin {
                plugin: ResolvedPlugin::Auth(p.clone()),
                config,
            });
        }
        if let Some(p) = self.guard.get(id) {
            return Ok(ResolvedBuiltin {
                plugin: ResolvedPlugin::Guard(p.clone()),
                config,
            });
        }
        if let Some(p) = self.transform.get(id) {
            return Ok(ResolvedBuiltin {
                plugin: ResolvedPlugin::Transform(p.clone()),
                config,
            });
        }
        Err(not_found(format!(
            "plugin '{id}' is not resolvable (unknown or catalog-only)"
        )))
    }
}

/// Merge a custom plugin definition's config with the binding-site config:
/// binding-site keys win.
fn merge_config(definition: serde_json::Value, binding: serde_json::Value) -> serde_json::Value {
    match (definition, binding) {
        (serde_json::Value::Object(mut def), serde_json::Value::Object(bind)) => {
            for (k, v) in bind {
                def.insert(k, v);
            }
            serde_json::Value::Object(def)
        }
        (_, bind) => bind,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn registry() -> PluginRegistry {
        let store = Arc::new(crate::infra::storage::MemoryStore::new());
        PluginRegistry::with_builtins(
            Duration::from_mins(5),
            10,
            Arc::new(store.plugins()),
        )
    }

    #[tokio::test]
    async fn resolves_all_builtin_ids() {
        let reg = registry();
        for (id, kind) in [
            (gts_helpers::AUTH_PLUGIN_NOOP, "auth"),
            (gts_helpers::AUTH_PLUGIN_APIKEY, "auth"),
            (gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED, "auth"),
            (gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC, "auth"),
            (gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS, "guard"),
            (gts_helpers::TRANSFORM_PLUGIN_REQUEST_ID, "transform"),
        ] {
            let r = reg
                .resolve(&ResolvedBinding {
                    plugin_ref: PluginRef::BuiltinId(id.to_owned()),
                    config: serde_json::json!({}),
                })
                .await
                .expect("resolves");
            assert_eq!(r.plugin.plugin_type(), kind, "plugin '{id}'");
        }
    }

    #[tokio::test]
    async fn catalog_only_ids_are_not_resolvable() {
        let reg = registry();
        for id in [
            gts_helpers::AUTH_PLUGIN_BASIC_CATALOG,
            gts_helpers::GUARD_PLUGIN_CORS_CATALOG,
            gts_helpers::TRANSFORM_PLUGIN_METRICS_CATALOG,
        ] {
            let err = reg
                .resolve(&ResolvedBinding {
                    plugin_ref: PluginRef::BuiltinId(id.to_owned()),
                    config: serde_json::json!({}),
                })
                .await
                .err()
                .expect("catalog-only ids must not resolve");
            assert!(matches!(err, DataPlaneError::PluginNotFound { .. }));
        }
    }

    #[test]
    fn binding_config_overrides_definition_config() {
        let merged = merge_config(
            serde_json::json!({ "a": 1, "b": 2 }),
            serde_json::json!({ "b": 3 }),
        );
        assert_eq!(merged["a"], 1);
        assert_eq!(merged["b"], 3);
    }
}
