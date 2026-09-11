//! The plugin contracts (ADR 0002) and the registry that resolves them.
//!
//! A plugin is identified by a string: either a built-in GTS id
//! (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`) or the UUID of a
//! custom plugin definition. Built-ins are resolved in-process by the
//! [`ControlPlane`]; custom plugins are looked up in the control-plane store.
//!
//! Execution order is Auth → Guards → Transform(request) → upstream call →
//! Transform(response/error), with upstream plugins running before route
//! plugins.

use async_trait::async_trait;
use http::{HeaderMap, Method, StatusCode};
use serde_json::Value;
use toolkit_security::SecurityContext;

use crate::credstore_client::SharedCredentialStore;
use crate::error::DomainError;

/// Everything a plugin needs to do its work for one proxied request.
///
/// No credential material is ever stored here — plugins resolve secrets
/// themselves through [`ProxyContext::credential_store`] at request time and
/// inject them straight into the outbound header map.
pub struct ProxyContext {
    /// Authenticated caller.
    pub security: SecurityContext,
    /// Tenant the request was made from.
    pub tenant_id: uuid::Uuid,
    /// Alias the request was addressed to.
    pub alias: String,
    /// Upstream the request resolves to.
    pub upstream_id: uuid::Uuid,
    /// Matched route, when one matched.
    pub route_id: Option<uuid::Uuid>,
    /// Endpoint host the request will be sent to.
    pub endpoint_host: String,
    /// Path on the outbound hop.
    pub outbound_path: String,
    /// Client IP, for scope-`ip` rate limits.
    pub client_ip: String,
}

impl std::fmt::Debug for ProxyContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyContext")
            .field("tenant_id", &self.tenant_id)
            .field("alias", &self.alias)
            .field("upstream_id", &self.upstream_id)
            .field("route_id", &self.route_id)
            .field("endpoint_host", &self.endpoint_host)
            .field("outbound_path", &self.outbound_path)
            .field("client_ip", &self.client_ip)
            .finish()
    }
}

/// Credential injection.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// Inject credentials into `headers`, or — for an api key configured with
    /// `location: query` — into `query`.
    ///
    /// # Errors
    ///
    /// Errors when credentials cannot be resolved or the upstream rejects
    /// them. The error never names the credential value.
    async fn authenticate(
        &self,
        ctx: &ProxyContext,
        config: &Value,
        headers: &mut HeaderMap,
        query: &mut crate::domain::query::OutboundQuery,
    ) -> Result<(), DomainError>;
}

/// Request/response validation.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// Validate the outbound request.
    ///
    /// # Errors
    ///
    /// Errors with a 4xx [`DomainError`] when the request must be rejected.
    async fn check_request(
        &self,
        ctx: &ProxyContext,
        config: &Value,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config, method, headers);
        Ok(())
    }

    /// Validate the upstream response.
    ///
    /// # Errors
    ///
    /// Errors with a 502 [`DomainError`] when the response must be rejected.
    async fn check_response(
        &self,
        ctx: &ProxyContext,
        config: &Value,
        status: StatusCode,
        headers: &HeaderMap,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config, status, headers);
        Ok(())
    }
}

/// Request / response mutation.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// Mutate the outbound request headers.
    ///
    /// # Errors
    ///
    /// Errors with a 4xx [`DomainError`] when the request must be rejected.
    async fn transform_request(
        &self,
        ctx: &ProxyContext,
        config: &Value,
        headers: &mut HeaderMap,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config, headers);
        Ok(())
    }

    /// Mutate the inbound response headers.
    ///
    /// # Errors
    ///
    /// Errors with a 502 [`DomainError`] when the response must be rejected.
    async fn transform_response(
        &self,
        ctx: &ProxyContext,
        config: &Value,
        status: StatusCode,
        headers: &mut HeaderMap,
    ) -> Result<(), DomainError> {
        let _ = (ctx, config, status, headers);
        Ok(())
    }
}

/// A plugin that this release has no implementation for.
///
/// `basic` / `bearer` auth and the `timeout` / `cors` guard and the
/// `logging` / `metrics` transform ids are catalog entries only: their
/// behaviour is core Data-Plane functionality (or intentionally unimplemented),
/// so binding them must not fail the request.
struct CatalogOnlyPlugin {
    id: &'static str,
}

#[async_trait]
impl AuthPlugin for CatalogOnlyPlugin {
    fn id(&self) -> &str {
        self.id
    }

    async fn authenticate(
        &self,
        _ctx: &ProxyContext,
        _config: &Value,
        _headers: &mut HeaderMap,
        _query: &mut crate::domain::query::OutboundQuery,
    ) -> Result<(), DomainError> {
        Err(DomainError::new(
            crate::error::ErrorKind::PluginNotFound,
            "auth plugin exists in the catalog but has no implementation in this release",
        ))
    }
}

#[async_trait]
impl GuardPlugin for CatalogOnlyPlugin {
    fn id(&self) -> &str {
        self.id
    }
}

#[async_trait]
impl TransformPlugin for CatalogOnlyPlugin {
    fn id(&self) -> &str {
        self.id
    }
}

/// Resolves plugin identifiers to implementations.
///
/// Mirrors the `ControlPlane` shape from ADR 0002: three maps keyed by
/// `plugin.id()`, populated with the built-ins and extended at startup with
/// any external plugins.
#[derive(Default)]
pub struct ControlPlane {
    auth: std::collections::HashMap<String, std::sync::Arc<dyn AuthPlugin>>,
    guards: std::collections::HashMap<String, std::sync::Arc<dyn GuardPlugin>>,
    transforms: std::collections::HashMap<String, std::sync::Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for ControlPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlane")
            .field("auth", &self.auth.keys().collect::<Vec<_>>())
            .field("guards", &self.guards.keys().collect::<Vec<_>>())
            .field("transforms", &self.transforms.keys().collect::<Vec<_>>())
            .finish()
    }
}


impl ControlPlane {
    /// Build a control plane holding every built-in plugin.
    #[must_use]
    pub fn with_builtins(
        credential_store: SharedCredentialStore,
        token_cache: std::sync::Arc<crate::infra::token_cache::TokenCache>,
    ) -> Self {
        let mut this = Self::default();
        this.register_auth(std::sync::Arc::new(crate::infra::plugins::NoopAuthPlugin));
        this.register_auth(std::sync::Arc::new(
            crate::infra::plugins::ApiKeyAuthPlugin::new(credential_store.clone()),
        ));
        this.register_auth(std::sync::Arc::new(
            crate::infra::plugins::OAuth2ClientCredAuthPlugin::new(credential_store, token_cache),
        ));
        this.register_guard(std::sync::Arc::new(
            crate::infra::plugins::RequiredHeadersGuardPlugin,
        ));
        this.register_transform(std::sync::Arc::new(
            crate::infra::plugins::RequestIdTransformPlugin,
        ));

        // Catalog-only identifiers: present so a binding is *recognized*
        // (and therefore not "unknown"), but the auth one has no behaviour.
        for id in crate::ids::CATALOG_ONLY_AUTH_PLUGINS {
            this.register_auth(std::sync::Arc::new(CatalogOnlyPlugin { id }));
        }
        for id in crate::ids::CATALOG_ONLY_GUARD_PLUGINS {
            this.register_guard(std::sync::Arc::new(CatalogOnlyPlugin { id }));
        }
        for id in crate::ids::CATALOG_ONLY_TRANSFORM_PLUGINS {
            this.register_transform(std::sync::Arc::new(CatalogOnlyPlugin { id }));
        }
        this
    }

    /// Register (or replace) an auth plugin.
    pub fn register_auth(&mut self, plugin: std::sync::Arc<dyn AuthPlugin>) {
        self.auth.insert(plugin.id().to_owned(), plugin);
    }

    /// Register (or replace) a guard plugin.
    pub fn register_guard(&mut self, plugin: std::sync::Arc<dyn GuardPlugin>) {
        self.guards.insert(plugin.id().to_owned(), plugin);
    }

    /// Register (or replace) a transform plugin.
    pub fn register_transform(&mut self, plugin: std::sync::Arc<dyn TransformPlugin>) {
        self.transforms.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolve an auth plugin by identifier.
    #[must_use]
    pub fn auth_plugin(&self, id: &str) -> Option<std::sync::Arc<dyn AuthPlugin>> {
        self.auth.get(id).cloned()
    }

    /// Resolve a guard plugin by identifier.
    #[must_use]
    pub fn guard_plugin(&self, id: &str) -> Option<std::sync::Arc<dyn GuardPlugin>> {
        self.guards.get(id).cloned()
    }

    /// Resolve a transform plugin by identifier.
    #[must_use]
    pub fn transform_plugin(&self, id: &str) -> Option<std::sync::Arc<dyn TransformPlugin>> {
        self.transforms.get(id).cloned()
    }

    /// `true` when `plugin_ref` names a known auth plugin.
    #[must_use]
    pub fn has_auth(&self, plugin_ref: &str) -> bool {
        self.auth.contains_key(plugin_ref)
    }

    /// `true` when `plugin_ref` names a known plugin of any type.
    #[must_use]
    pub fn knows(&self, plugin_ref: &str) -> bool {
        self.auth.contains_key(plugin_ref)
            || self.guards.contains_key(plugin_ref)
            || self.transforms.contains_key(plugin_ref)
    }
}
