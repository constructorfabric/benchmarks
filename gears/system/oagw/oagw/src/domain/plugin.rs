//! Plugin contracts (ADR-0002, DESIGN §3.3 "Plugin System").
//!
//! Three traits with disjoint responsibilities, executed in a deterministic
//! order: **Auth → Guards → Transform(on_request) → upstream →
//! Transform(on_response | on_error)**. Registry lookups key on the plugin's
//! full GTS identifier, and every built-in declares its own id/plugin type so
//! the control plane cannot bind a catalog-only identifier (DESIGN §3.3).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::error::{DomainError, DomainResult};

/// GTS identifiers of the built-in plugins and the catalog-only reserves.
pub mod ids {
    /// Built-in auth plugin: injects nothing.
    pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// Built-in auth plugin: injects a static or credential-store API key.
    pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// Built-in auth plugin: OAuth2 client credentials, `Form` client auth.
    pub const AUTH_OAUTH2_FORM: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// Built-in auth plugin: OAuth2 client credentials, `Basic` client auth.
    pub const AUTH_OAUTH2_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// Built-in guard plugin: required request/response header enforcement.
    pub const GUARD_REQUIRED_HEADERS: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// Built-in transform plugin: request-id propagation.
    pub const TRANSFORM_REQUEST_ID: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

    /// Catalog-only auth identifiers: reserved, never resolvable.
    pub const CATALOG_ONLY_AUTH: [&str; 2] = [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
    ];
    /// Catalog-only guard identifiers.
    pub const CATALOG_ONLY_GUARD: [&str; 2] = [
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
    ];
    /// Catalog-only transform identifiers.
    pub const CATALOG_ONLY_TRANSFORM: [&str; 2] = [
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ];
}

/// Client-facing request data handed to every plugin phase.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    /// Request method (uppercase).
    pub method: String,
    /// Request path.
    pub path: String,
    /// Request headers, keyed by lowercase name.
    pub headers: BTreeMap<String, String>,
    /// Headers the plugin chain may add to the outbound request.
    pub injected_headers: BTreeMap<String, String>,
    /// Headers the plugin chain wants removed from the outbound request.
    pub removed_headers: Vec<String>,
    /// Plugin configuration (`ctx.config`).
    pub config: serde_json::Value,
    /// Identifier of the tenant that owns the resolved upstream.
    pub tenant_id: String,
    /// Authenticated caller subject, when the platform resolved one.
    pub subject_id: Option<String>,
}

/// Client-facing response data handed to the response phases.
#[derive(Debug, Clone, Default)]
pub struct ResponseContext {
    /// Upstream status code.
    pub status: u16,
    /// Response headers, keyed by lowercase name.
    pub headers: BTreeMap<String, String>,
    /// Headers the plugin chain wants added to the client response.
    pub injected_headers: BTreeMap<String, String>,
    /// Plugin configuration.
    pub config: serde_json::Value,
}

/// Verdict of a guard phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The phase found nothing to reject.
    Allow,
    /// Reject the request or response with a status code and problem type.
    Reject {
        /// HTTP status the gateway returns.
        status: u16,
        /// OAGW GTS problem type from the error catalog.
        error_code: &'static str,
        /// Human-readable explanation carried in the problem `detail`.
        message: String,
    },
}

impl GuardDecision {
    /// Builds a rejection with the given status and catalog code.
    #[must_use]
    pub fn reject(status: u16, error_code: &'static str, message: impl Into<String>) -> Self {
        Self::Reject {
            status,
            error_code,
            message: message.into(),
        }
    }
}

/// Credential-injection phase, executed once per request (one per upstream).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Full GTS identifier of the plugin.
    fn id(&self) -> &str;
    /// Plugin category (`auth`, `guard` or `transform`).
    fn plugin_type(&self) -> &str;
    /// Injects credentials into `ctx.injected_headers`.
    ///
    /// # Errors
    ///
    /// Returns `AuthenticationFailed` when credentials cannot be resolved or
    /// exchanged, and `SecretNotFound` for a missing credential reference.
    async fn authenticate(&self, ctx: &mut RequestContext) -> DomainResult<()>;
}

/// Policy-enforcement phase (can reject), multiple per upstream/route.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Full GTS identifier of the plugin.
    fn id(&self) -> &str;
    /// Plugin category.
    fn plugin_type(&self) -> &str;
    /// Validates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns an error only for internal plugin failures; a policy violation
    /// is a `GuardDecision::Reject`, not an error.
    async fn guard_request(&self, ctx: &RequestContext) -> DomainResult<GuardDecision>;
    /// Validates the upstream response.
    ///
    /// # Errors
    ///
    /// Returns an error only for internal plugin failures.
    async fn guard_response(&self, ctx: &ResponseContext) -> DomainResult<GuardDecision>;
}

/// Request/response mutation phase, multiple per upstream/route.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Full GTS identifier of the plugin.
    fn id(&self) -> &str;
    /// Plugin category.
    fn plugin_type(&self) -> &str;
    /// Mutates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin cannot run.
    async fn transform_request(&self, ctx: &mut RequestContext) -> DomainResult<()>;
    /// Mutates the client response.
    ///
    /// # Errors
    ///
    /// Returns an error when the plugin cannot run.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> DomainResult<()>;
}

/// Lookup result for an unresolvable plugin reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownPlugin {
    /// The unresolvable reference.
    pub reference: String,
    /// The category the caller asked for.
    pub kind: &'static str,
}

impl UnknownPlugin {
    /// Classifies a failed lookup into a `503` domain error.
    #[must_use]
    pub fn into_error(self) -> DomainError {
        let Self { reference, kind } = &self;
        if ids::CATALOG_ONLY_AUTH
            .iter()
            .chain(ids::CATALOG_ONLY_GUARD.iter())
            .chain(ids::CATALOG_ONLY_TRANSFORM.iter())
            .any(|catalog| *catalog == reference)
        {
            return DomainError::Validation(format!(
                "{reference} is catalogued only: {kind} plugins of this id have no backing \
                 implementation and cannot be bound",
            ));
        }
        DomainError::PluginNotFound(self.reference)
    }
}

/// Registry of `AuthPlugin` implementations.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Creates a registry from the given plugins.
    #[must_use]
    pub fn new(plugins: Vec<Arc<dyn AuthPlugin>>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|plugin| (plugin.id().to_owned(), plugin))
                .collect(),
        }
    }

    /// Looks a plugin up by reference.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(reference).cloned()
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

/// Registry of `GuardPlugin` implementations.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Creates a registry from the given plugins.
    #[must_use]
    pub fn new(plugins: Vec<Arc<dyn GuardPlugin>>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|plugin| (plugin.id().to_owned(), plugin))
                .collect(),
        }
    }

    /// Looks a plugin up by reference.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(reference).cloned()
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

/// Registry of `TransformPlugin` implementations.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Creates a registry from the given plugins.
    #[must_use]
    pub fn new(plugins: Vec<Arc<dyn TransformPlugin>>) -> Self {
        Self {
            plugins: plugins
                .into_iter()
                .map(|plugin| (plugin.id().to_owned(), plugin))
                .collect(),
        }
    }

    /// Looks a plugin up by reference.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(reference).cloned()
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;

    #[async_trait]
    impl AuthPlugin for Stub {
        fn id(&self) -> &str {
            ids::AUTH_NOOP
        }
        fn plugin_type(&self) -> &str {
            "auth"
        }
        async fn authenticate(&self, _ctx: &mut RequestContext) -> DomainResult<()> {
            Ok(())
        }
    }

    #[test]
    fn catalog_only_identifiers_are_not_resolvable() {
        // The built-in set excludes the catalog-only reserves (DESIGN §3.3).
        assert!(!ids::CATALOG_ONLY_AUTH.contains(&ids::AUTH_NOOP));
        assert!(!ids::CATALOG_ONLY_AUTH.contains(&ids::AUTH_APIKEY));
        assert!(!ids::CATALOG_ONLY_GUARD.contains(&ids::GUARD_REQUIRED_HEADERS));
        assert!(!ids::CATALOG_ONLY_TRANSFORM.contains(&ids::TRANSFORM_REQUEST_ID));
        // The reserves are distinct from every built-in id.
        let builtins = [
            ids::AUTH_NOOP,
            ids::AUTH_APIKEY,
            ids::AUTH_OAUTH2_FORM,
            ids::AUTH_OAUTH2_BASIC,
            ids::GUARD_REQUIRED_HEADERS,
            ids::TRANSFORM_REQUEST_ID,
        ];
        for catalog in ids::CATALOG_ONLY_AUTH
            .iter()
            .chain(ids::CATALOG_ONLY_GUARD.iter())
            .chain(ids::CATALOG_ONLY_TRANSFORM.iter())
        {
            assert!(!builtins.contains(catalog));
        }
    }

    #[test]
    fn registries_index_by_id() {
        let registry = AuthPluginRegistry::new(vec![Arc::new(Stub)]);
        assert_eq!(
            registry.get(ids::AUTH_NOOP).expect("registered").id(),
            ids::AUTH_NOOP
        );
        assert!(registry.get(ids::AUTH_APIKEY).is_none());
        assert_eq!(registry.ids(), vec![ids::AUTH_NOOP]);
    }

    #[test]
    fn unknown_plugin_classifies_catalog_only_references() {
        let error = UnknownPlugin {
            reference: ids::CATALOG_ONLY_AUTH[0].to_owned(),
            kind: "auth",
        }
        .into_error();
        assert_eq!(error.status_code(), 400);

        let error = UnknownPlugin {
            reference: "gts.cf.core.oagw.auth_plugin.v1~not-registered.v1".to_owned(),
            kind: "auth",
        }
        .into_error();
        assert!(matches!(error, DomainError::PluginNotFound(_)));
        assert_eq!(error.status_code(), 503);
    }

    #[test]
    fn guard_decision_carries_status_and_code() {
        let decision =
            GuardDecision::reject(400, crate::domain::error::codes::ROUTE_REJECTED, "no");
        assert!(matches!(
            decision,
            GuardDecision::Reject { status: 400, .. }
        ));
        assert_eq!(GuardDecision::Allow, GuardDecision::Allow);
    }
}
