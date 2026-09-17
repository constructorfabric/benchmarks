// Created: 2026-09-03 by Constructor Tech
//! Plugin catalogue, resolution and application.
//!
//! Plugins are declarative: a plugin record names a built-in behaviour and
//! carries its configuration. Three behaviour families exist — credential
//! injection (`auth`), validation (`guard`) and mutation (`transform`).
//! Starlark source plugins are not part of this build, so a plugin is
//! identified by its `name`/`plugin_type` pair rather than by embedded code.

use std::collections::BTreeMap;

use http::header::HeaderName;
use http::HeaderValue;
use serde_json::Value;
use uuid::Uuid;

use crate::error::{ErrorKind, OagwError};
use crate::gts;
use crate::model::{
    AuthConfig, PluginItem, PluginRecord, PluginType, Route, Upstream,
};

/// Built-in auth plugin identifiers.
pub const AUTH_NOOP: &str = "noop";
pub const AUTH_APIKEY: &str = "apikey";
pub const AUTH_OAUTH2: &str = "oauth2_client_cred";
pub const AUTH_OAUTH2_BASIC: &str = "oauth2_client_cred_basic";
/// Built-in guard plugin enforcing required headers (`ADR/0009`).
pub const GUARD_REQUIRED_HEADERS: &str = "required_headers";
/// Built-in transform plugin applying header rewrites.
pub const TRANSFORM_HEADER_TRANSFORM: &str = "header_transform";

/// Auth plugins that are catalogued but have no backing implementation.
const UNSUPPORTED_AUTH_PLUGINS: [&str; 2] = ["basic", "bearer"];

/// Whether `name` is a built-in auth plugin.
#[must_use]
pub fn is_builtin_auth(name: &str) -> bool {
    matches!(
        name,
        AUTH_NOOP | AUTH_APIKEY | AUTH_OAUTH2 | AUTH_OAUTH2_BASIC
    )
}

/// The effective plugin references of an upstream.
#[must_use]
pub fn references_of_upstream(upstream: &Upstream) -> Vec<PluginItem> {
    let mut items = upstream
        .plugins
        .as_ref()
        .map(|plugins| plugins.items.clone())
        .unwrap_or_default();
    if let Some(auth) = &upstream.auth
        && let Some(auth_type) = &auth.auth_type {
            items.insert(0, PluginItem::Ref(auth_type.clone()));
        }
    items
}

/// The effective plugin references of a route.
#[must_use]
pub fn references_of_route(route: &Route) -> Vec<PluginItem> {
    route
        .plugins
        .as_ref()
        .map(|plugins| plugins.items.clone())
        .unwrap_or_default()
}

/// A plugin reference resolved to a concrete behaviour.
#[derive(Debug, Clone)]
pub struct ResolvedPlugin {
    /// Canonical identifier of the plugin (`gts.cf.core.oagw.*.v1~...`).
    pub id: String,
    /// Built-in behaviour name (`apikey`, `required_headers`, ...).
    pub behaviour: String,
    /// Plugin type.
    pub plugin_type: PluginType,
    /// Effective configuration: record configuration overlaid by the binding.
    pub config: Value,
}

/// Parses a plugin reference into either a built-in name or a custom UUID.
///
/// Three shapes are accepted: a named GTS instance
/// (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`), a
/// UUID-backed GTS instance identifying a custom plugin record, and a bare
/// UUID string identifying the same record.
fn parse_reference(reference: &str) -> (Option<String>, Option<Uuid>) {
    if let Some(name) = gts::named_instance(reference) {
        return (Some(name.to_owned()), None);
    }
    if let Some(uuid) = gts::uuid_instance(reference) {
        return (None, Some(uuid));
    }
    let trimmed = reference.trim();
    if let Ok(uuid) = Uuid::parse_str(trimmed) {
        return (None, Some(uuid));
    }
    (Some(trimmed.to_ascii_lowercase()), None)
}

/// The custom plugin identifier a reference points at, if any.
///
/// Both the UUID-backed GTS form and a bare UUID string are accepted, so
/// callers can resolve a reference into a tenant-scoped plugin record.
#[must_use]
pub fn custom_reference(reference: &str) -> Option<Uuid> {
    gts::uuid_instance(reference).or_else(|| Uuid::parse_str(reference.trim()).ok())
}

/// Validates a plugin reference against the catalogue and the custom store.
///
/// # Errors
/// Returns 400 `ValidationError` when the reference names an unknown plugin,
/// and 503 `PluginNotFound` when a custom plugin is referenced but absent.
pub fn validate_reference(
    reference: &str,
    expected: PluginType,
    custom: Option<&PluginRecord>,
) -> Result<(), OagwError> {
    let (name, uuid) = parse_reference(reference);
    if let Some(uuid) = uuid {
        return match custom {
            Some(record) if record.id == uuid => {
                if record.plugin_type == expected {
                    Ok(())
                } else {
                    Err(OagwError::new(
                        ErrorKind::Validation,
                        format!("plugin '{reference}' is not a {expected:?} plugin"),
                    ))
                }
            }
            _ => Err(OagwError::new(
                ErrorKind::Validation,
                format!("plugin '{reference}' does not exist"),
            )),
        };
    }
    let Some(name) = name else {
        return Err(OagwError::new(
            ErrorKind::Validation,
            format!("plugin '{reference}' does not exist"),
        ));
    };
    let known = match expected {
        PluginType::Auth => is_builtin_auth(&name),
        PluginType::Guard => name == GUARD_REQUIRED_HEADERS,
        PluginType::Transform => name == TRANSFORM_HEADER_TRANSFORM,
    };
    if known {
        return Ok(());
    }
    if expected == PluginType::Auth && UNSUPPORTED_AUTH_PLUGINS.contains(&name.as_str()) {
        return Err(OagwError::new(
            ErrorKind::Validation,
            format!("unknown auth plugin '{name}'; use oauth2_client_cred or apikey"),
        ));
    }
    Err(OagwError::new(
        ErrorKind::Validation,
        format!("unknown {expected:?} plugin '{name}'"),
    ))
}

/// Validates a plugin reference of any behaviour family.
///
/// # Errors
/// Returns 400 `ValidationError` when the reference names an unknown plugin.
pub fn validate_reference_any(
    reference: &str,
    custom: Option<&PluginRecord>,
) -> Result<(), OagwError> {
    let (name, uuid) = parse_reference(reference);
    if let Some(uuid) = uuid {
        return match custom {
            Some(record) if record.id == uuid => Ok(()),
            _ => Err(OagwError::new(
                ErrorKind::Validation,
                format!("plugin '{reference}' does not exist"),
            )),
        };
    }
    if is_known_behaviour(name.as_deref().unwrap_or_default()) {
        return Ok(());
    }
    if UNSUPPORTED_AUTH_PLUGINS.contains(&name.as_deref().unwrap_or_default()) {
        return Err(OagwError::new(
            ErrorKind::Validation,
            format!(
                "unknown auth plugin '{}'; use oauth2_client_cred or apikey",
                name.unwrap_or_default()
            ),
        ));
    }
    Err(OagwError::new(
        ErrorKind::Validation,
        format!("unknown plugin '{}'", name.unwrap_or_default()),
    ))
}

/// Whether `name` is a catalogued built-in behaviour.
#[must_use]
pub fn is_known_behaviour(name: &str) -> bool {
    is_builtin_auth(name)
        || name == GUARD_REQUIRED_HEADERS
        || name == TRANSFORM_HEADER_TRANSFORM
}

/// Resolves a plugin chain into executable plugins.
///
/// Upstream plugins run before route plugins.
///
/// # Errors
/// Returns 503 `PluginNotFound` when a referenced plugin cannot be resolved.
pub fn resolve_chain(
    items: &[PluginItem],
    lookup: &dyn Fn(&str) -> Option<PluginRecord>,
) -> Result<Vec<ResolvedPlugin>, OagwError> {
    let mut resolved = Vec::with_capacity(items.len());
    for item in items {
        let reference = item.plugin_ref();
        let (name, uuid) = parse_reference(reference);
        let (behaviour, plugin_type, mut config) = if let Some(uuid) = uuid {
            let record = lookup(&uuid.to_string()).ok_or_else(|| {
                OagwError::new(
                    ErrorKind::PluginNotFound,
                    format!("plugin '{reference}' is not registered"),
                )
            })?;
            (record.name.clone(), record.plugin_type, record.config.clone().unwrap_or(Value::Null))
        } else {
            let name = name.unwrap_or_else(|| reference.to_owned());
            let plugin_type = if is_builtin_auth(&name) {
                PluginType::Auth
            } else if name == GUARD_REQUIRED_HEADERS {
                PluginType::Guard
            } else if name == TRANSFORM_HEADER_TRANSFORM {
                PluginType::Transform
            } else {
                return Err(OagwError::new(
                    ErrorKind::PluginNotFound,
                    format!("plugin '{reference}' is not registered"),
                ));
            };
            (name, plugin_type, Value::Null)
        };
        if let Some(override_config) = item.config() {
            config = merge_config(config, override_config);
        }
        resolved.push(ResolvedPlugin {
            id: reference.to_owned(),
            behaviour,
            plugin_type,
            config,
        });
    }
    Ok(resolved)
}

/// Shallow-merges a binding configuration over the plugin's own configuration.
#[must_use]
pub fn merge_config(base: Value, overlay: &Value) -> Value {
    match (&base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (key, value) in overlay_map {
                merged.insert(key.clone(), value.clone());
            }
            Value::Object(merged)
        }
        (_, overlay) => overlay.clone(),
    }
}

/// The auth configuration of an upstream, if any.
#[must_use]
pub fn auth_config(upstream: &Upstream, chain: &[ResolvedPlugin]) -> Option<(String, Value)> {
    if let Some(auth) = &upstream.auth
        && let Some(auth_type) = &auth.auth_type {
            let config = auth.config.clone().unwrap_or(Value::Null);
            return Some((normalized_auth_name(auth_type), config));
        }
    chain
        .iter()
        .find(|plugin| plugin.plugin_type == PluginType::Auth)
        .map(|plugin| (plugin.behaviour.clone(), plugin.config.clone()))
}

fn normalized_auth_name(reference: &str) -> String {
    let (name, _) = parse_reference(reference);
    name.unwrap_or_else(|| reference.to_ascii_lowercase())
}

/// Reads a string member out of a configuration object.
#[must_use]
pub fn config_str(config: &Value, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Reads a comma-separated or array-of-strings member as a list of names.
#[must_use]
pub fn config_names(config: &Value, key: &str) -> Vec<String> {
    match config.get(key) {
        Some(Value::String(raw)) => raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_ascii_lowercase)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|item| item.trim().to_ascii_lowercase())
            .filter(|item| !item.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Reads a string-to-string map member.
#[must_use]
pub fn config_map(config: &Value, key: &str) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    if let Some(Value::Object(entries)) = config.get(key) {
        for (name, value) in entries {
            let rendered = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            result.insert(name.clone(), rendered);
        }
    }
    result
}

/// Reads a list-of-strings member.
#[must_use]
pub fn config_list(config: &Value, key: &str) -> Vec<String> {
    match config.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(raw)) => raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// Applies the `set` rules of a header map to a header container.
pub fn apply_set(map: &BTreeMap<String, String>, mut set: impl FnMut(&str, &str)) {
    for (name, value) in map {
        set(name, value);
    }
}

/// Normalizes a header name, returning `None` when it is not a valid name.
#[must_use]
pub fn header_name(name: &str) -> Option<HeaderName> {
    HeaderName::from_bytes(name.as_bytes()).ok()
}

/// Renders a header value, returning `None` when it is not visible ASCII.
#[must_use]
pub fn header_value(value: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(value).ok()
}

/// Validates the auth configuration of an upstream at create time.
///
/// # Errors
/// Returns 400 `ValidationError` for unknown auth plugins or a configuration
/// that is not an object.
pub fn validate_auth(auth: &AuthConfig) -> Result<(), OagwError> {
    let Some(auth_type) = &auth.auth_type else {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "auth.type is required when auth is configured",
        ));
    };
    let name = normalized_auth_name(auth_type);
    validate_reference(&name, PluginType::Auth, None)?;
    if auth.config.as_ref().is_some_and(|value| !value.is_object() && !value.is_null()) {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "auth.config must be a JSON object",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugin(name: &str, plugin_type: PluginType, config: Value) -> PluginRecord {
        PluginRecord {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            name: name.to_owned(),
            plugin_type,
            config: Some(config),
            created_at: 0,
            last_used_at: None,
            gc_eligible_at: None,
        }
    }

    #[test]
    fn unknown_auth_plugin_is_rejected() {
        assert!(validate_reference("basic", PluginType::Auth, None).is_err());
        assert!(validate_reference("bearer", PluginType::Auth, None).is_err());
        assert!(validate_reference("apikey", PluginType::Auth, None).is_ok());
        assert!(validate_reference("oauth2_client_cred", PluginType::Auth, None).is_ok());
    }

    #[test]
    fn named_references_are_parsed() {
        let id = gts::named_plugin_id(gts::GUARD_PLUGIN_TYPE, GUARD_REQUIRED_HEADERS);
        assert!(validate_reference(&id, PluginType::Guard, None).is_ok());
        assert!(validate_reference(&id, PluginType::Auth, None).is_err());
    }

    #[test]
    fn custom_plugin_reference_requires_a_record() {
        let id = Uuid::new_v4();
        assert!(validate_reference(&id.to_string(), PluginType::Guard, None).is_err());
        let mut record = plugin("my-guard", PluginType::Guard, Value::Null);
        assert!(validate_reference(&id.to_string(), PluginType::Guard, Some(&record)).is_err());
        record.id = id;
        assert!(validate_reference(&id.to_string(), PluginType::Guard, Some(&record)).is_ok());
        assert!(validate_reference(&id.to_string(), PluginType::Auth, Some(&record)).is_err());
    }

    #[test]
    fn chain_resolution_merges_binding_config() {
        let record = plugin("mine", PluginType::Guard, json!({ "a": 1, "b": 2 }));
        let binding = PluginItem::Binding {
            plugin_ref: record.id.to_string(),
            config: Some(json!({ "b": 3 })),
        };
        let chain = resolve_chain(&[binding], &|_| Some(record.clone())).expect("resolved");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].config["a"], json!(1));
        assert_eq!(chain[0].config["b"], json!(3));
    }

    #[test]
    fn config_helpers_parse_both_shapes() {
        let config = json!({ "names": "A, B", "list": ["c"], "map": { "x": "y" } });
        assert_eq!(config_names(&config, "names"), vec!["a", "b"]);
        assert_eq!(config_list(&config, "list"), vec!["c".to_owned()]);
        assert_eq!(config_map(&config, "map")["x"], "y");
        assert_eq!(config_str(&config, "missing"), None);
    }

    #[test]
    fn upstream_auth_yields_the_behaviour_name() {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "api.example.com".to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: crate::model::ServerConfig { endpoints: Vec::new() },
            protocol: gts::PROTOCOL_HTTP.to_owned(),
            auth: Some(AuthConfig {
                auth_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
                sharing: crate::model::SharingMode::Private,
                config: Some(json!({ "key": "secret" })),
            }),
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        };
        let (behaviour, config) = auth_config(&upstream, &[]).expect("auth");
        assert_eq!(behaviour, AUTH_APIKEY);
        assert_eq!(config["key"], "secret");
    }
}
