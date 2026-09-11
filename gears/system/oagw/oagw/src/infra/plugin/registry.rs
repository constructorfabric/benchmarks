//! Registries for the three plugin kinds, holding exactly the resolvable built-ins.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers;
use crate::domain::model::PluginRef;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

/// What a registry hands back: the plugin to run and the configuration to run it with.
type ResolvedPlugin<P> = Result<Option<(Arc<P>, Value)>, DomainError>;

/// Registry of authentication plugins.
pub struct AuthPluginRegistry {
    entries: BTreeMap<&'static str, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Registry pre-populated with the built-in auth plugins, resolving against `resolver`.
    #[must_use]
    pub fn with_builtins(resolver: crate::infra::credentials::SecretResolver) -> Self {
        let mut entries = BTreeMap::new();
        for plugin in [
            Arc::new(super::noop_auth::NoopAuthPlugin) as Arc<dyn AuthPlugin>,
            Arc::new(super::apikey_auth::ApiKeyAuthPlugin::new(Some(Arc::clone(&resolver)))),
            Arc::new(
                super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::form()
                    .with_resolver(Arc::clone(&resolver)),
            ),
            Arc::new(
                super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::basic()
                    .with_resolver(resolver),
            ),
        ] {
            entries.insert(plugin.id(), plugin);
        }
        Self { entries }
    }

    /// Resolve a plugin reference, rejecting the catalog-only identifiers.
    pub fn resolve(&self, r: &PluginRef) -> ResolvedPlugin<dyn AuthPlugin> {
        let (id, config) = ref_parts(r);
        if id.is_none() {
            return Ok(None);
        }
        let id = id.unwrap_or_default().to_string();
        if id.parse::<uuid::Uuid>().is_ok() {
            // Custom plugins have no interpreter in this release (DESIGN §4.7).
            return Ok(None);
        }
        let plugin = self.entries.get(id.as_str()).ok_or_else(|| {
            DomainError::Validation(format!("unknown auth plugin '{id}'"))
        })?;
        Ok(Some((Arc::clone(plugin), config)))
    }

    /// True when `id` is one of the registered, bindable implementations.
    #[must_use]
    pub fn resolvable(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// All registered identifiers.
    #[must_use]
    pub fn registered(&self) -> Vec<&'static str> {
        self.entries.keys().copied().collect()
    }
}

/// Registry of guard plugins.
pub struct GuardPluginRegistry {
    entries: BTreeMap<&'static str, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Registry pre-populated with the built-in guard plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(
            gts_helpers::GUARD_REQUIRED_HEADERS,
            Arc::new(super::required_headers_guard::RequiredHeadersGuardPlugin)
                as Arc<dyn GuardPlugin>,
        );
        Self { entries }
    }

    /// Resolve a plugin reference, ignoring the catalog-only identifiers.
    pub fn resolve(&self, r: &PluginRef) -> ResolvedPlugin<dyn GuardPlugin> {
        let (id, config) = ref_parts(r);
        let Some(id) = id else { return Ok(None) };
        if id.parse::<uuid::Uuid>().is_ok() {
            return Ok(None);
        }
        let plugin = self.entries.get(id).ok_or_else(|| {
            DomainError::Validation(format!("unknown guard plugin '{id}'"))
        })?;
        Ok(Some((Arc::clone(plugin), config)))
    }

    /// True when `id` is one of the registered, bindable implementations.
    #[must_use]
    pub fn resolvable(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// All registered identifiers.
    #[must_use]
    pub fn registered(&self) -> Vec<&'static str> {
        self.entries.keys().copied().collect()
    }
}

/// Registry of transform plugins.
pub struct TransformPluginRegistry {
    entries: BTreeMap<&'static str, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Registry pre-populated with the built-in transform plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(
            gts_helpers::TRANSFORM_REQUEST_ID,
            Arc::new(super::request_id_transform::RequestIdTransformPlugin)
                as Arc<dyn TransformPlugin>,
        );
        Self { entries }
    }

    /// Resolve a plugin reference.
    pub fn resolve(&self, r: &PluginRef) -> ResolvedPlugin<dyn TransformPlugin> {
        let (id, config) = ref_parts(r);
        let Some(id) = id else { return Ok(None) };
        if id.parse::<uuid::Uuid>().is_ok() {
            return Ok(None);
        }
        let plugin = self.entries.get(id).ok_or_else(|| {
            DomainError::Validation(format!("unknown transform plugin '{id}'"))
        })?;
        Ok(Some((Arc::clone(plugin), config)))
    }

    /// True when `id` is one of the registered, bindable implementations.
    #[must_use]
    pub fn resolvable(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// All registered identifiers.
    #[must_use]
    pub fn registered(&self) -> Vec<&'static str> {
        self.entries.keys().copied().collect()
    }
}

fn ref_parts(r: &PluginRef) -> (Option<&str>, Value) {
    match r {
        PluginRef::Bare(s) => (Some(s.as_str()), Value::Null),
        PluginRef::Detailed {
            plugin_ref,
            config,
        } => (
            Some(plugin_ref.as_str()),
            config
                .as_ref()
                .map(|c| Value::Object(c.clone()))
                .unwrap_or(Value::Null),
        ),
    }
}

/// Comma-separated header-name list from a plugin config key, lowercase-normalized.
///
/// `None` when the key is absent or blank, which is the guard's fail-open signal.
#[must_use]
pub fn header_list(config: &Value, key: &str) -> Option<Vec<String>> {
    let raw = config.get(key).and_then(Value::as_str)?;
    let names: Vec<String> = raw_list(raw)
        .iter()
        .filter(|n| !n.is_empty())
        .map(|n| n.to_ascii_lowercase())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

fn raw_list(raw: &str) -> Vec<String> {
    raw.split(',').map(|n| n.trim().to_string()).collect()
}

