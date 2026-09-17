//! Plugin registries and the execution-order driver.
//!
//! [`PluginEngine`] is the data plane's single entry point: it resolves a
//! chain of bindings against the three registries and runs them in the order
//! `docs/ADR/0002` pins — auth, then guards, then request transforms, then the
//! upstream call, then response/error transforms. Upstream bindings always run
//! before route bindings, which is a property of how the data plane concatenates
//! the two chains, not of the registries.
use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::model::PluginKind;
use crate::domain::plugin::{
    AuthPlugin, ErrorContext, GuardDecision, GuardPlugin, PLUGIN_NOT_FOUND, PluginError,
    RequestContext, ResponseContext, TransformPlugin, problem_type,
};
use crate::domain::services::management::built_in_plugin;

use super::auth::{
    APIKEY_PLUGIN_TYPE, ApiKeyAuthPlugin, NOOP_PLUGIN_TYPE, NoopAuthPlugin,
    OAUTH2_BASIC_PLUGIN_TYPE, OAUTH2_FORM_PLUGIN_TYPE, OAuth2ClientCredAuthPlugin,
    TokenCacheConfig,
};
use super::guard::RequiredHeadersGuardPlugin;
use super::transform::RequestIdTransformPlugin;

/// An auth plugin bound to a configuration document.
#[derive(Debug, Clone)]
pub struct AuthBinding {
    /// Full GTS plugin id the configuration binds.
    pub plugin_type: String,
    /// Effective configuration document.
    pub config: serde_json::Value,
}

/// A guard or transform binding.
#[derive(Debug, Clone)]
pub struct PluginBinding {
    /// Full GTS plugin id the configuration binds.
    pub plugin_ref: String,
    /// Effective configuration document.
    pub config: serde_json::Value,
}

/// Lookup table of auth plugins.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Build the registry holding the four built-in auth plugins.
    #[must_use]
    pub fn with_builtins(secrets: Arc<dyn crate::domain::plugin::SecretResolver>) -> Self {
        let mut registry = Self::default();
        let cache = TokenCacheConfig::default();
        for plugin in [
            Arc::new(NoopAuthPlugin) as Arc<dyn AuthPlugin>,
            Arc::new(ApiKeyAuthPlugin::new(secrets.clone())),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                secrets.clone(),
                toolkit_auth::oauth2::ClientAuthMethod::Form,
                cache,
            )),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                secrets,
                toolkit_auth::oauth2::ClientAuthMethod::Basic,
                cache,
            )),
        ] {
            registry.register(plugin);
        }
        registry
    }

    /// Register (or replace) a plugin, keyed by its GTS type id.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a binding reference to a plugin.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(reference).cloned().or_else(|| {
            self.plugins
                .values()
                .find(|plugin| {
                    plugin.id() == reference || plugin.plugin_type().ends_with(reference)
                })
                .cloned()
        })
    }

    /// Every registered plugin type id.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Lookup table of guard plugins.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Build the registry holding the built-in guard plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(RequiredHeadersGuardPlugin));
        registry
    }

    /// Register (or replace) a plugin, keyed by its GTS type id.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a binding reference to a plugin.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(reference).cloned().or_else(|| {
            self.plugins
                .values()
                .find(|plugin| {
                    plugin.id() == reference || plugin.plugin_type().ends_with(reference)
                })
                .cloned()
        })
    }

    /// Every registered plugin type id.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Lookup table of transform plugins.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Build the registry holding the built-in transform plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(RequestIdTransformPlugin));
        registry
    }

    /// Register (or replace) a plugin, keyed by its GTS type id.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a binding reference to a plugin.
    #[must_use]
    pub fn resolve(&self, reference: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(reference).cloned().or_else(|| {
            self.plugins
                .values()
                .find(|plugin| {
                    plugin.id() == reference || plugin.plugin_type().ends_with(reference)
                })
                .cloned()
        })
    }

    /// Every registered plugin type id.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// The three registries, held together so the data plane has one handle.
#[derive(Clone)]
pub struct PluginEngine {
    auth: AuthPluginRegistry,
    guard: GuardPluginRegistry,
    transform: TransformPluginRegistry,
}

impl PluginEngine {
    /// Build an engine holding only the built-in plugins.
    #[must_use]
    pub fn with_builtins(secrets: Arc<dyn crate::domain::plugin::SecretResolver>) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(secrets),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }

    /// The auth registry.
    #[must_use]
    pub const fn auth(&self) -> &AuthPluginRegistry {
        &self.auth
    }

    /// The guard registry.
    #[must_use]
    pub const fn guard(&self) -> &GuardPluginRegistry {
        &self.guard
    }

    /// The transform registry.
    #[must_use]
    pub const fn transform(&self) -> &TransformPluginRegistry {
        &self.transform
    }

    /// GTS ids of the plugins of `kind` this engine can resolve.
    #[must_use]
    pub fn resolvable(&self, kind: PluginKind) -> Vec<String> {
        match kind {
            PluginKind::Auth => self.auth.ids(),
            PluginKind::Guard => self.guard.ids(),
            PluginKind::Transform => self.transform.ids(),
        }
    }

    /// Run the auth phase: exactly one auth plugin, then the guards, then the
    /// request transforms.
    ///
    /// # Errors
    ///
    /// The first [`PluginError`] any plugin raises, with the offending
    /// binding's reference in the detail.
    pub async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        auth: Option<AuthBinding>,
        guards: &[PluginBinding],
        transforms: &[PluginBinding],
    ) -> Result<(), PluginError> {
        if let Some(binding) = auth {
            let plugin = self.auth.resolve(&binding.plugin_type).ok_or_else(|| {
                PluginError::new(
                    503,
                    problem_type(PLUGIN_NOT_FOUND),
                    format!(
                        "auth plugin '{}' is not available on this gateway",
                        binding.plugin_type
                    ),
                )
            })?;
            ctx.config = binding.config;
            plugin.authenticate(ctx).await?;
        }
        self.guard_request(ctx, guards).await?;
        self.transform_request(ctx, transforms).await
    }

    /// Run the request-side guard phase.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when a guard itself fails.
    pub async fn guard_request(
        &self,
        ctx: &mut RequestContext,
        guards: &[PluginBinding],
    ) -> Result<(), PluginError> {
        for binding in guards {
            let plugin = resolve_guard(&self.guard, &binding.plugin_ref)?;
            ctx.config = binding.config.clone();
            match plugin.guard_request(ctx).await? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    code,
                    detail,
                } => return Err(PluginError::new(status, code, detail)),
            }
        }
        Ok(())
    }

    /// Run the response-side guard phase.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when a guard itself fails.
    pub async fn guard_response(
        &self,
        ctx: &mut ResponseContext,
        guards: &[PluginBinding],
    ) -> Result<(), PluginError> {
        for binding in guards {
            let plugin = resolve_guard(&self.guard, &binding.plugin_ref)?;
            ctx.config = binding.config.clone();
            match plugin.guard_response(ctx).await? {
                GuardDecision::Allow => {}
                GuardDecision::Reject {
                    status,
                    code,
                    detail,
                } => return Err(PluginError::new(status, code, detail)),
            }
        }
        Ok(())
    }

    /// Run the request transform phase.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when a transform fails.
    pub async fn transform_request(
        &self,
        ctx: &mut RequestContext,
        transforms: &[PluginBinding],
    ) -> Result<(), PluginError> {
        for binding in transforms {
            let plugin = resolve_transform(&self.transform, &binding.plugin_ref)?;
            ctx.config = binding.config.clone();
            plugin.transform_request(ctx).await?;
        }
        Ok(())
    }

    /// Run the response transform phase.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when a transform fails.
    pub async fn transform_response(
        &self,
        ctx: &mut ResponseContext,
        transforms: &[PluginBinding],
    ) -> Result<(), PluginError> {
        for binding in transforms {
            let plugin = resolve_transform(&self.transform, &binding.plugin_ref)?;
            ctx.config = binding.config.clone();
            plugin.transform_response(ctx).await?;
        }
        Ok(())
    }

    /// Run the error transform phase.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when a transform fails.
    pub async fn transform_error(
        &self,
        ctx: &mut ErrorContext,
        transforms: &[PluginBinding],
    ) -> Result<(), PluginError> {
        for binding in transforms {
            let plugin = resolve_transform(&self.transform, &binding.plugin_ref)?;
            plugin.transform_error(ctx).await?;
        }
        Ok(())
    }
}

fn resolve_guard(
    registry: &GuardPluginRegistry,
    reference: &str,
) -> Result<Arc<dyn GuardPlugin>, PluginError> {
    registry.resolve(reference).ok_or_else(|| {
        PluginError::new(
            503,
            problem_type(PLUGIN_NOT_FOUND),
            format!("guard plugin '{reference}' is not available on this gateway"),
        )
    })
}

fn resolve_transform(
    registry: &TransformPluginRegistry,
    reference: &str,
) -> Result<Arc<dyn TransformPlugin>, PluginError> {
    registry.resolve(reference).ok_or_else(|| {
        PluginError::new(
            503,
            problem_type(PLUGIN_NOT_FOUND),
            format!("transform plugin '{reference}' is not available on this gateway"),
        )
    })
}

/// Whether a built-in GTS id is a bindable plugin of `kind`.
#[must_use]
pub fn is_bindable_built_in(gts_id: &str, kind: PluginKind) -> bool {
    built_in_plugin(gts_id).is_some_and(|entry| entry.kind == kind && entry.bindable)
}

/// The four built-in auth plugin type ids, in registration order.
#[must_use]
pub fn built_in_auth_plugin_types() -> Vec<String> {
    vec![
        NOOP_PLUGIN_TYPE.to_owned(),
        APIKEY_PLUGIN_TYPE.to_owned(),
        OAUTH2_FORM_PLUGIN_TYPE.to_owned(),
        OAUTH2_BASIC_PLUGIN_TYPE.to_owned(),
    ]
}
