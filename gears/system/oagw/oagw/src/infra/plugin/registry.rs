// Created: 2026-08-31 by Constructor Tech
//! The three plugin registries (ADR-0002 "Plugin Loading", ADR-0008, ADR-0009).
//!
//! A registry is keyed by the **full GTS id** a binding spells
//! (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`). The short
//! spelling (`apikey`) is accepted as well: `crate::domain::plugin::PluginRef`
//! classifies the reference and the canonical id is looked up. The catalog is
//! not duplicated here — what is bindable comes from the catalog, what runs
//! comes from a registry.
//!
//! # Degradation (ADR-0008 "Registry Integration")
//!
//! The auth registry is built around a credential store. A deployment that
//! wires none (the `ClientHub` lookup fails) still gets a working gateway:
//! `with_builtins` is called without one, the no-op plugin is the only auth
//! plugin that resolves, and an upstream whose binding needs a credential
//! fails its request with 503 `link.unavailable.v1` — never a silent forward
//! without credentials.

use std::collections::HashMap;
use std::sync::Arc;

use toolkit_http::HttpClientConfig;

use crate::domain::model::PluginKind;
use crate::domain::plugin::PluginRef;
use crate::error::{OagwError, OagwErrorKind};
use crate::infra::plugin::apikey_auth::{APIKEY_AUTH_PLUGIN_ID, ApiKeyAuthPlugin};
use crate::infra::plugin::noop_auth::{NOOP_AUTH_PLUGIN_ID, NoopAuthPlugin};
use crate::infra::plugin::oauth2_client_cred_auth::with_builtins as oauth2_builtins;
use crate::infra::plugin::request_id_transform::{
    REQUEST_ID_TRANSFORM_PLUGIN_ID, RequestIdTransformPlugin,
};
use crate::infra::plugin::required_headers_guard::{
    REQUIRED_HEADERS_GUARD_PLUGIN_ID, RequiredHeadersGuardPlugin,
};
use crate::infra::plugin::secrets::CredStore;
use crate::infra::plugin::traits::{AuthPlugin, GuardPlugin, TransformPlugin};

/// Registry of the credential-injection plugins.
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

/// Registry of the policy-enforcement plugins.
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

/// Registry of the mutation plugins.
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

/// The three registries of one data plane.
#[derive(Clone)]
pub struct PluginRegistries {
    auth: Arc<AuthPluginRegistry>,
    guard: Arc<GuardPluginRegistry>,
    transform: Arc<TransformPluginRegistry>,
}

impl PluginRegistries {
    /// The built-in registries of one deployment (ADR-0008, ADR-0009).
    ///
    /// `credstore` is the credential store the auth plugins resolve their
    /// `cred://` references through; `token_http_config` is the HTTP client
    /// configuration the `OAuth2` token exchange uses — the same one the proxy
    /// dials upstreams with; `token_cache` carries the cache ceiling and
    /// capacity.
    #[must_use]
    pub fn with_builtins(
        credstore: Option<CredStore>,
        token_http_config: Option<HttpClientConfig>,
        token_cache: crate::config::TokenCacheConfig,
    ) -> Self {
        Self {
            auth: Arc::new(AuthPluginRegistry::with_builtins(
                credstore,
                token_http_config,
                token_cache,
            )),
            guard: Arc::new(GuardPluginRegistry::with_builtins()),
            transform: Arc::new(TransformPluginRegistry::with_builtins()),
        }
    }

    /// Registry of the auth plugins.
    #[must_use]
    pub fn auth(&self) -> &AuthPluginRegistry {
        &self.auth
    }

    /// Registry of the guard plugins.
    #[must_use]
    pub fn guard(&self) -> &GuardPluginRegistry {
        &self.guard
    }

    /// Registry of the transform plugins.
    #[must_use]
    pub fn transform(&self) -> &TransformPluginRegistry {
        &self.transform
    }
}

impl AuthPluginRegistry {
    /// The built-in auth plugins (ADR-0002, ADR-0008).
    ///
    /// `noop` and `apikey` need no credential store; the two `OAuth2` variants
    /// do. Without one they are not registered, which is what makes an
    /// upstream that binds them fail closed (503 `link.unavailable.v1`).
    #[must_use]
    pub fn with_builtins(
        credstore: Option<CredStore>,
        token_http_config: Option<HttpClientConfig>,
        token_cache: crate::config::TokenCacheConfig,
    ) -> Self {
        let mut plugins: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        plugins.insert(NOOP_AUTH_PLUGIN_ID.to_owned(), Arc::new(NoopAuthPlugin));
        if let Some(credstore) = credstore {
            plugins.insert(
                APIKEY_AUTH_PLUGIN_ID.to_owned(),
                Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&credstore))),
            );
            for (id, plugin) in oauth2_builtins(
                credstore,
                token_http_config,
                token_cache.ttl,
                token_cache.capacity,
            ) {
                plugins.insert(id, plugin);
            }
        } else {
            tracing::warn!(
                "credential store is not wired; only the '{}' auth plugin is available",
                NOOP_AUTH_PLUGIN_ID
            );
        }
        Self { plugins }
    }

    /// Resolve a reference to the plugin that implements it.
    ///
    /// `None` means the reference names no implementation this registry has:
    /// either it is a built-in that needs a credential store that is not wired,
    /// or it is catalogued without an implementation, or it is unknown.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins
            .get(reference)
            .cloned()
            .or_else(|| canonical_id(reference).and_then(|id| self.plugins.get(&id).cloned()))
    }
}

impl GuardPluginRegistry {
    /// The built-in guard plugins (ADR-0009).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        plugins.insert(
            REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    /// Resolve a reference to the plugin that implements it.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins
            .get(reference)
            .cloned()
            .or_else(|| canonical_id(reference).and_then(|id| self.plugins.get(&id).cloned()))
    }
}

impl TransformPluginRegistry {
    /// The built-in transform plugins (ADR-0002).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        plugins.insert(
            REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );
        Self { plugins }
    }

    /// Resolve a reference to the plugin that implements it.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins
            .get(reference)
            .cloned()
            .or_else(|| canonical_id(reference).and_then(|id| self.plugins.get(&id).cloned()))
    }
}

/// Canonical GTS id of a built-in reference, when the catalog knows it.
///
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` and the short
/// `apikey` both name the same plugin. A short spelling is not classified by
/// `PluginRef`, so every family is tried; the catalog names are unique across
/// families. A reference the catalog does not classify as a *bindable* built-in
/// has no canonical id.
fn canonical_id(reference: &str) -> Option<String> {
    let trimmed = reference.trim();
    [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform]
        .into_iter()
        .find_map(|kind| match PluginRef::parse(reference) {
            PluginRef::BuiltIn {
                kind: declared,
                name,
                resolvable,
                ..
            } if declared == kind && resolvable => Some(kind.built_in_id(&name)),
            _ => crate::domain::plugin::lookup_built_in(kind, trimmed)
                .filter(|built_in| built_in.resolvable)
                .map(|built_in| kind.built_in_id(built_in.name)),
        })
}

/// 503 `link.unavailable.v1` for an auth binding the data plane cannot resolve.
///
/// The one failure that must never degrade into a silent forward: a credential
/// the gateway cannot inject is a link that is not available, not an
/// unauthenticated request.
#[must_use]
pub fn unresolved_auth_plugin(reference: &str) -> OagwError {
    OagwError::new(
        OagwErrorKind::LinkUnavailable,
        format!("auth plugin '{reference}' is not available in this deployment"),
    )
}

/// 503 `plugin.not_found.v1` for a chain reference without an implementation.
#[must_use]
pub fn unresolved_plugin(reference: &str) -> OagwError {
    OagwError::new(
        OagwErrorKind::PluginNotFound,
        format!("plugin '{reference}' cannot be resolved in this deployment"),
    )
}
