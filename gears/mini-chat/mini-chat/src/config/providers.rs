//! Provider registry configuration (`providers.<id>`) and its validation.
//!
//! Implements `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{Deserializer, MapAccess, Visitor};

use toolkit::var_expand::expand_env_vars;
use uuid::Uuid;

use super::{ConfigError, SecretValue};

/// Insertion-ordered string-keyed map (configuration order is significant for
/// alias grouping). Duplicate keys are a parse error.
#[derive(Debug, Clone)]
pub struct OrderedMap<V>(Vec<(String, V)>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> OrderedMap<V> {
    /// Entries in configuration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Looks up an entry by key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = OrderedMap<V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries: Vec<(String, V)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    if entries.iter().any(|(k, _)| *k == key) {
                        return Err(serde::de::Error::custom(format!("duplicate key `{key}`")));
                    }
                    entries.push((key, value));
                }
                Ok(OrderedMap(entries))
            }
        }

        deserializer.deserialize_map(MapVisitor(PhantomData))
    }
}

/// OAGW auth plugin configuration (`header`, `prefix`, `secret_ref`, ...).
/// Every value is a secret: redacted in `Debug`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct AuthConfig(BTreeMap<String, SecretValue>);

impl<'de> Deserialize<'de> for AuthConfig {
    /// Accepts only a map of strings; a type mismatch fails with a value-free
    /// message (serde's default message would echo the offending input).
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AuthConfigVisitor;

        impl<'de> serde::de::Visitor<'de> for AuthConfigVisitor {
            type Value = AuthConfig;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of strings")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                while let Some((key, value)) = access.next_entry::<String, SecretValue>()? {
                    entries.insert(key, value);
                }
                Ok(AuthConfig(entries))
            }

            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Err(E::custom("auth_config must be a map"))
            }

            reject_scalars_without_echo!("auth_config must be a map");
        }

        deserializer.deserialize_map(AuthConfigVisitor)
    }
}

impl AuthConfig {
    /// Builds an auth config from key-value pairs.
    #[must_use]
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), SecretValue::new(v)))
                .collect(),
        )
    }

    /// Returns the value of `key`. Callers must never log it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(SecretValue::expose)
    }

    /// Keys in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Whether the config has no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Expands `${VAR}` in every value. On failure returns the key of the value
    /// that could not be expanded (never the value).
    fn expanded(&self) -> Result<Self, String> {
        let mut out = BTreeMap::new();
        for (key, value) in &self.0 {
            let expanded = expand_env_vars(value.expose()).map_err(|_| key.clone())?;
            out.insert(key.clone(), SecretValue::new(expanded));
        }
        Ok(Self(out))
    }
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for key in self.0.keys() {
            map.entry(key, &"[REDACTED]");
        }
        map.finish()
    }
}

/// Adapter kind of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// `OpenAI` Responses API.
    OpenaiResponses,
    /// `OpenAI` Chat Completions API.
    OpenaiChatCompletions,
    /// vLLM Responses API.
    VllmResponses,
    /// Anthropic Messages API.
    AnthropicMessages,
}

/// File / vector-store implementation of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    /// `OpenAI` storage.
    Openai,
    /// Azure `OpenAI` storage (requires `api_version`).
    Azure,
}

/// Raw `providers.<id>.tenant_overrides.<tenant_id>` entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverrideConfig {
    /// Override host (`${VAR}` expanded).
    pub host: Option<String>,
    /// Override alias; hostname endpoints always use the OAGW-derived alias.
    pub upstream_alias: Option<String>,
    /// Override auth plugin; falls back to the provider entry.
    pub auth_plugin_type: Option<String>,
    /// Override auth config (`${VAR}` expanded); falls back to the provider entry.
    pub auth_config: Option<AuthConfig>,
}

/// Raw `providers.<id>` entry. Unknown keys (including the removed
/// `supports_file_search_filters`) are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Adapter kind (required).
    pub kind: ProviderKind,
    /// Host (required, `${VAR}` expanded).
    pub host: String,
    /// Port; defaults to 443, or 80 when `use_http`.
    #[serde(default)]
    pub port: Option<u16>,
    /// Use plain HTTP.
    #[serde(default)]
    pub use_http: bool,
    /// OAGW alias; hostname endpoints always use the OAGW-derived alias.
    #[serde(default)]
    pub upstream_alias: Option<String>,
    /// Chat endpoint path; defaults to `/v1/responses`.
    #[serde(default)]
    pub api_path: Option<String>,
    /// OAGW auth plugin type.
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    /// OAGW auth plugin config (`${VAR}` expanded).
    #[serde(default)]
    pub auth_config: Option<AuthConfig>,
    /// File / vector-store implementation (required).
    pub storage_kind: StorageKind,
    /// Storage label; defaults to the provider id.
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Azure `api-version` (required for `storage_kind = azure`).
    #[serde(default)]
    pub api_version: Option<String>,
    /// Provider used for file / vector-store operations.
    #[serde(default)]
    pub rag_provider: Option<String>,
    /// Per-tenant overrides.
    #[serde(default)]
    pub tenant_overrides: OrderedMap<TenantOverrideConfig>,
}

/// Provider entries in configuration order.
pub type ProviderMap = OrderedMap<ProviderConfig>;

/// Validated tenant override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTenantOverride {
    /// Tenant id (map key).
    pub tenant_id: String,
    /// Effective host (override host, or the entry host).
    pub host: String,
    /// Effective OAGW alias (derived from the override host and the entry endpoint).
    pub upstream_alias: String,
    /// Effective auth plugin type (override, or the entry's).
    pub auth_plugin_type: Option<String>,
    /// Effective auth config (override, or the entry's).
    pub auth_config: AuthConfig,
}

/// Validated provider entry with defaults applied and `${VAR}` expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    /// Provider id (map key).
    pub id: String,
    /// Adapter kind.
    pub kind: ProviderKind,
    /// Expanded host.
    pub host: String,
    /// Port (443, 80 when `use_http`, or explicit).
    pub port: u16,
    /// Plain HTTP.
    pub use_http: bool,
    /// Effective OAGW alias (derived `host[:port]` for hostnames; configured or derived for IP literals).
    pub upstream_alias: String,
    /// Chat endpoint path.
    pub api_path: String,
    /// Auth plugin type.
    pub auth_plugin_type: Option<String>,
    /// Expanded auth config.
    pub auth_config: AuthConfig,
    /// Storage implementation.
    pub storage_kind: StorageKind,
    /// Storage label (provider id when not set).
    pub storage_backend: String,
    /// Azure `api-version`.
    pub api_version: Option<String>,
    /// RAG provider id.
    pub rag_provider: Option<String>,
    /// Validated tenant overrides in configuration order.
    pub tenant_overrides: Vec<ResolvedTenantOverride>,
}

/// One effective OAGW target: a provider entry or a tenant override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTarget {
    /// Provider id.
    pub provider_id: String,
    /// Tenant id for an override target.
    pub tenant_id: Option<String>,
    /// Effective alias.
    pub alias: String,
    /// Host.
    pub host: String,
    /// Port (always from the entry).
    pub port: u16,
    /// Plain HTTP (always from the entry).
    pub use_http: bool,
    /// Auth plugin type.
    pub auth_plugin_type: Option<String>,
    /// Auth config.
    pub auth_config: AuthConfig,
    /// API path (always from the entry).
    pub api_path: String,
}

impl ProviderTarget {
    /// Key path of the target: `providers.<id>` or
    /// `providers.<id>.tenant_overrides.<tenant_id>`.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.tenant_id {
            Some(tenant) => format!("providers.{}.tenant_overrides.{tenant}", self.provider_id),
            None => format!("providers.{}", self.provider_id),
        }
    }
}

/// Validated provider entries and effective targets (entries and overrides in
/// configuration order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRegistry {
    /// Validated entries.
    pub providers: Vec<ResolvedProvider>,
    /// One target per entry and per tenant override.
    pub targets: Vec<ProviderTarget>,
}

/// Default plugin type of API-key authentication.
const API_KEY_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Default chat endpoint path; `{model}` is the `provider_model_id` placeholder.
const DEFAULT_API_PATH: &str = "/v1/responses";

/// The single default entry used when `providers` is absent.
fn default_provider_map() -> ProviderMap {
    let entry = ProviderConfig {
        kind: ProviderKind::OpenaiResponses,
        host: "api.openai.com".to_owned(),
        port: None,
        use_http: false,
        upstream_alias: None,
        api_path: None,
        auth_plugin_type: Some(API_KEY_AUTH_PLUGIN.to_owned()),
        auth_config: Some(AuthConfig::from_pairs([
            ("header", "Authorization"),
            ("prefix", "Bearer "),
            ("secret_ref", "cred://openai-key"),
        ])),
        storage_kind: StorageKind::Openai,
        storage_backend: None,
        api_version: None,
        rag_provider: None,
        tenant_overrides: OrderedMap::default(),
    };
    OrderedMap(vec![("openai".to_owned(), entry)])
}

/// Whether `host` holds only letters, digits, `.`, `-`, `_`, `:`, `[`, `]`.
fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

/// Lower-cases an alias and strips trailing dots, exactly like OAGW's
/// `normalize_alias`. Every alias comparison and grouping uses this form.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// Host as OAGW normalizes it: IPv6 brackets stripped, lower-cased, trailing
/// dots stripped.
fn normalized_host(host: &str) -> String {
    let inner = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    inner.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// Whether `host` is an IP literal (v4 or v6, brackets allowed).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    normalized_host(host).parse::<std::net::IpAddr>().is_ok()
}

/// The alias OAGW derives for a single-endpoint hostname upstream: `host` on
/// the standard port (80 for http, 443 for https), `host:port` otherwise.
#[must_use]
pub fn derived_alias(host: &str, port: u16, use_http: bool) -> String {
    let host = normalized_host(host);
    let standard = if use_http { 80 } else { 443 };
    if port == standard {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// The alias OAGW will store and resolve for the endpoint.
///
/// Hostname endpoints always use the derived alias (OAGW rejects any other).
/// IP-literal endpoints take the normalized configured alias, defaulting to the
/// derived form `ip[:port]`.
#[must_use]
pub fn effective_alias(host: &str, port: u16, use_http: bool, configured: Option<&str>) -> String {
    let configured = configured.filter(|a| !a.is_empty());
    if is_ip_literal(host) {
        configured.map_or_else(|| derived_alias(host, port, use_http), normalize_alias)
    } else {
        derived_alias(host, port, use_http)
    }
}

/// Like [`effective_alias`], warning when a configured alias of a hostname
/// endpoint is ignored. `key` is the key path of the `upstream_alias`.
fn resolve_alias(
    host: &str,
    port: u16,
    use_http: bool,
    configured: Option<&str>,
    key: &str,
) -> String {
    let alias = effective_alias(host, port, use_http, configured);
    if let Some(configured) = configured.filter(|a| !a.is_empty())
        && !is_ip_literal(host)
        && normalize_alias(configured) != alias
    {
        tracing::warn!(
            key = %key,
            configured = %configured,
            derived = %alias,
            "upstream_alias differs from the alias OAGW derives for a hostname endpoint; using the derived alias"
        );
    }
    alias
}

/// Expands `${VAR}` in `host` and applies the host character check.
/// `key` is the key path of the host (used in messages, never the value).
fn expand_host(host: &str, key: &str) -> Result<String, ConfigError> {
    let expanded = expand_env_vars(host).map_err(|_| {
        ConfigError::invalid(key, "references an environment variable that is not set")
    })?;
    if !is_valid_host(&expanded) {
        return Err(ConfigError::invalid(
            key,
            "must be non-empty and contain only letters, digits, '.', '-', '_', ':', '[' and ']'",
        ));
    }
    Ok(expanded)
}

/// Expands `${VAR}` in every `auth_config` value; messages name only the key path.
fn expand_auth_config(auth: &AuthConfig, key: &str) -> Result<AuthConfig, ConfigError> {
    auth.expanded().map_err(|entry| {
        ConfigError::invalid(
            format!("{key}.{entry}"),
            "references an environment variable that is not set",
        )
    })
}

/// Checks the Azure `api-version`: non-blank, letters, digits, `.` and `-` only.
fn check_api_version(version: Option<&str>, key: &str) -> Result<(), ConfigError> {
    let Some(version) = version.filter(|v| !v.trim().is_empty()) else {
        return Err(ConfigError::invalid(
            key,
            "is required and must not be blank when storage_kind is azure",
        ));
    };
    if !version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return Err(ConfigError::invalid(
            key,
            "must contain only letters, digits, '.' and '-'",
        ));
    }
    Ok(())
}

/// Resolves one `tenant_overrides.<tenant_id>` entry against its provider entry.
fn resolve_override(
    provider_id: &str,
    tenant_id: &str,
    over: &TenantOverrideConfig,
    entry_host: &str,
    entry_endpoint: (u16, bool),
    entry_auth_plugin: Option<&String>,
    entry_auth: &AuthConfig,
) -> Result<ResolvedTenantOverride, ConfigError> {
    let key = format!("providers.{provider_id}.tenant_overrides.{tenant_id}");
    let tenant_uuid = Uuid::parse_str(tenant_id)
        .map_err(|_| ConfigError::invalid(&key, "the key must be a tenant UUID"))?;

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-override
    let alias = over.upstream_alias.as_deref().filter(|a| !a.is_empty());
    if over.host.is_none() && alias.is_none() {
        return Err(ConfigError::invalid(
            &key,
            "requires `host` or `upstream_alias`",
        ));
    }
    let host = match &over.host {
        Some(raw_host) => expand_host(raw_host, &format!("{key}.host"))?,
        None => entry_host.to_owned(),
    };
    let own_auth = over
        .auth_config
        .as_ref()
        .map(|auth| expand_auth_config(auth, &format!("{key}.auth_config")))
        .transpose()?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-override

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-override-fallback
    let auth_plugin_type = over
        .auth_plugin_type
        .clone()
        .or_else(|| entry_auth_plugin.cloned());
    let auth_config = own_auth.unwrap_or_else(|| entry_auth.clone());
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-override-fallback

    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-fill-aliases
    let (port, use_http) = entry_endpoint;
    let upstream_alias = resolve_alias(
        &host,
        port,
        use_http,
        alias,
        &format!("{key}.upstream_alias"),
    );
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-fill-aliases

    Ok(ResolvedTenantOverride {
        // Canonical (hyphenated, lower-case) form of the validated key.
        tenant_id: tenant_uuid.to_string(),
        host,
        upstream_alias,
        auth_plugin_type,
        auth_config,
    })
}

/// Validates one `providers.<id>` entry and applies its defaults.
fn resolve_provider(
    id: &str,
    entry: &ProviderConfig,
    all: &ProviderMap,
) -> Result<ResolvedProvider, ConfigError> {
    let key = format!("providers.{id}");

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-kind
    // `kind` is a closed enum: parsing rejects any other value.
    let kind = entry.kind;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-kind

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-host
    let host = expand_host(&entry.host, &format!("{key}.host"))?;
    let auth_config = expand_auth_config(
        &entry.auth_config.clone().unwrap_or_default(),
        &format!("{key}.auth_config"),
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-host

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-port
    let port = match entry.port {
        Some(0) => {
            return Err(ConfigError::invalid(
                format!("{key}.port"),
                "must be between 1 and 65535",
            ));
        }
        Some(port) => port,
        None if entry.use_http => 80,
        None => 443,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-port

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-default-api-path
    let api_path = entry
        .api_path
        .clone()
        .unwrap_or_else(|| DEFAULT_API_PATH.to_owned());
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-default-api-path

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-storage-kind
    // `storage_kind` is a closed enum: parsing rejects any other value.
    let storage_kind = entry.storage_kind;
    let storage_backend = entry
        .storage_backend
        .clone()
        .unwrap_or_else(|| id.to_owned());
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-storage-kind

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-azure
    if storage_kind == StorageKind::Azure {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-api-version
        check_api_version(entry.api_version.as_deref(), &format!("{key}.api_version"))?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-check-api-version
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-azure

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-rag-dangling
    if let Some(rag) = &entry.rag_provider
        && all.get(rag).is_none()
    {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-return-rag-error
        return Err(ConfigError::invalid(
            format!("{key}.rag_provider"),
            format!("names `{rag}`, which is not a configured provider"),
        ));
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-return-rag-error
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-rag-dangling

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-for-each-override
    let mut tenant_overrides = Vec::new();
    for (tenant_id, over) in entry.tenant_overrides.iter() {
        let resolved = resolve_override(
            id,
            tenant_id,
            over,
            &host,
            (port, entry.use_http),
            entry.auth_plugin_type.as_ref(),
            &auth_config,
        )?;
        if tenant_overrides
            .iter()
            .any(|other: &ResolvedTenantOverride| other.tenant_id == resolved.tenant_id)
        {
            return Err(ConfigError::invalid(
                format!("{key}.tenant_overrides.{tenant_id}"),
                "duplicates another tenant override with the same tenant UUID",
            ));
        }
        tenant_overrides.push(resolved);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-for-each-override

    // Explicit alias of the entry, or the expanded host.
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-fill-aliases
    let upstream_alias = resolve_alias(
        &host,
        port,
        entry.use_http,
        entry.upstream_alias.as_deref(),
        &format!("{key}.upstream_alias"),
    );
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-fill-aliases

    Ok(ResolvedProvider {
        id: id.to_owned(),
        kind,
        host,
        port,
        use_http: entry.use_http,
        upstream_alias,
        api_path,
        auth_plugin_type: entry.auth_plugin_type.clone(),
        auth_config,
        storage_kind,
        storage_backend,
        api_version: entry.api_version.clone(),
        rag_provider: entry.rag_provider.clone(),
        tenant_overrides,
    })
}

/// One effective target per entry and per tenant override, in configuration
/// order. Override fields fall back to the entry; `port`, `use_http` and
/// `api_path` always come from the entry.
fn build_targets(providers: &[ResolvedProvider]) -> Vec<ProviderTarget> {
    let mut targets = Vec::new();
    for provider in providers {
        targets.push(ProviderTarget {
            provider_id: provider.id.clone(),
            tenant_id: None,
            alias: provider.upstream_alias.clone(),
            host: provider.host.clone(),
            port: provider.port,
            use_http: provider.use_http,
            auth_plugin_type: provider.auth_plugin_type.clone(),
            auth_config: provider.auth_config.clone(),
            api_path: provider.api_path.clone(),
        });
        for over in &provider.tenant_overrides {
            targets.push(ProviderTarget {
                provider_id: provider.id.clone(),
                tenant_id: Some(over.tenant_id.clone()),
                alias: over.upstream_alias.clone(),
                host: over.host.clone(),
                port: provider.port,
                use_http: provider.use_http,
                auth_plugin_type: over.auth_plugin_type.clone(),
                auth_config: over.auth_config.clone(),
                api_path: provider.api_path.clone(),
            });
        }
    }
    targets
}

/// Names of the connection settings in which two targets differ.
fn differing_fields(first: &ProviderTarget, other: &ProviderTarget) -> Vec<String> {
    let checks = [
        ("host", first.host != other.host),
        ("port", first.port != other.port),
        ("use_http", first.use_http != other.use_http),
        (
            "auth_plugin_type",
            first.auth_plugin_type != other.auth_plugin_type,
        ),
        ("auth_config", first.auth_config != other.auth_config),
        ("api_path", first.api_path != other.api_path),
    ];
    checks
        .into_iter()
        .filter(|(_, differs)| *differs)
        .map(|(name, _)| name.to_owned())
        .collect()
}

/// Compares every later target of an alias group with the first one.
fn check_alias_groups(targets: &[ProviderTarget]) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-for-each-alias-group
    for (index, target) in targets.iter().enumerate() {
        let Some(first) = targets[..index]
            .iter()
            .find(|t| normalize_alias(&t.alias) == normalize_alias(&target.alias))
        else {
            continue;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-alias-conflict
        let fields = differing_fields(first, target);
        if !fields.is_empty() {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-return-alias-conflict
            return Err(ConfigError::AliasConflict {
                alias: target.alias.clone(),
                first: first.label(),
                second: target.label(),
                fields,
            });
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-return-alias-conflict
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-alias-conflict
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-for-each-alias-group
    Ok(())
}

/// Validates the provider map. `None` yields the single default entry
/// `openai` (`openai_responses`, `api.openai.com`, API-key auth from
/// `cred://openai-key`, `storage_kind = openai`).
///
/// # Errors
///
/// Returns [`ConfigError`] naming the key path and rule (never an `auth_config`
/// value) for a bad host, port, `api_version`, dangling `rag_provider`, invalid
/// override or an alias group whose targets differ in `host`, `port`,
/// `use_http`, `auth_plugin_type`, `auth_config` or `api_path`.
pub fn validate_providers(raw: Option<&ProviderMap>) -> Result<ProviderRegistry, ConfigError> {
    let default_map;
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-providers-absent
    let map = if let Some(map) = raw {
        map
    } else {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-default-provider
        default_map = default_provider_map();
        &default_map
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-default-provider
    };
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-if-providers-absent

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-for-each-provider
    let providers = map
        .iter()
        .map(|(id, entry)| resolve_provider(id, entry, map))
        .collect::<Result<Vec<_>, _>>()?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-for-each-provider

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-group-by-alias
    let targets = build_targets(&providers);
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-group-by-alias
    check_alias_groups(&targets)?;

    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-return-providers
    Ok(ProviderRegistry { providers, targets })
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-validate-providers:p1:inst-return-providers
}

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
