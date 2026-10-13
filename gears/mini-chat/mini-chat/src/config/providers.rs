//! Provider registry configuration (`providers.<id>`), see DESIGN Appendix B.1.

use std::collections::BTreeMap;

use serde::Deserialize;
use toolkit::var_expand::{ExpandVars, ExpandVarsError};

/// OAGW API-key auth plugin (GTS instance id).
pub const APIKEY_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Chat API flavour of a provider entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

/// File / vector-store implementation used by a provider entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

/// Per-tenant override of a provider entry. Unset fields fall back to the entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverride {
    pub host: Option<String>,
    pub upstream_alias: Option<String>,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<BTreeMap<String, String>>,
}

/// One provider (`providers.<id>`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub use_http: bool,
    #[serde(default)]
    pub upstream_alias: Option<String>,
    #[serde(default = "default_api_path")]
    pub api_path: String,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    #[serde(default)]
    pub auth_config: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub storage_kind: Option<StorageKind>,
    #[serde(default)]
    pub storage_backend: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub rag_provider: Option<String>,
    #[serde(default)]
    pub tenant_overrides: BTreeMap<String, TenantOverride>,
}

/// Port OAGW treats as the default of the scheme (alias derivation drops it).
const fn standard_port(use_http: bool) -> u16 {
    if use_http { 80 } else { 443 }
}

/// The OAGW upstream alias of an endpoint (spec 4a.4). The resolver and the provisioning code
/// both go through this function, so they always agree.
///
/// `explicit` (the configured `upstream_alias`) wins; otherwise the alias is the host when the
/// host is an IP literal or the port is the scheme's standard one (or unset), else `host:port`.
/// OAGW lower-cases aliases and strips trailing dots, so the result is normalized the same way.
#[must_use]
pub fn derive_alias(
    explicit: Option<&str>,
    host: &str,
    port: Option<u16>,
    use_http: bool,
) -> String {
    let raw = if let Some(alias) = explicit {
        alias.to_owned()
    } else {
        let literal = host.trim_start_matches('[').trim_end_matches(']');
        let is_ip = literal.parse::<std::net::IpAddr>().is_ok();
        match port {
            Some(port) if !is_ip && port != standard_port(use_http) => format!("{host}:{port}"),
            _ => host.to_owned(),
        }
    };
    raw.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

/// The provider set used when `providers` is not configured: one `OpenAI` entry.
#[must_use]
pub fn default_providers() -> BTreeMap<String, ProviderEntry> {
    let auth_config = BTreeMap::from([
        ("header".to_owned(), "Authorization".to_owned()),
        ("prefix".to_owned(), "Bearer ".to_owned()),
        ("secret_ref".to_owned(), "cred://openai-key".to_owned()),
    ]);
    let entry = ProviderEntry {
        kind: ProviderKind::OpenaiResponses,
        host: "api.openai.com".to_owned(),
        port: None,
        use_http: false,
        upstream_alias: None,
        api_path: default_api_path(),
        auth_plugin_type: Some(APIKEY_AUTH_PLUGIN.to_owned()),
        auth_config: Some(auth_config),
        storage_kind: Some(StorageKind::Openai),
        storage_backend: None,
        api_version: None,
        rag_provider: None,
        tenant_overrides: BTreeMap::new(),
    };
    BTreeMap::from([("openai".to_owned(), entry)])
}

// `ExpandVars` is not implemented for `BTreeMap` by the toolkit, so the derive cannot be used
// on the `auth_config` fields; the fields that support `${VAR}` are expanded by hand.
fn expand_map(map: &mut Option<BTreeMap<String, String>>) -> Result<(), ExpandVarsError> {
    for value in map.iter_mut().flat_map(BTreeMap::values_mut) {
        value.expand_vars()?;
    }
    Ok(())
}

impl ExpandVars for TenantOverride {
    fn expand_vars(&mut self) -> Result<(), ExpandVarsError> {
        self.host.expand_vars()?;
        expand_map(&mut self.auth_config)
    }
}

impl ExpandVars for ProviderEntry {
    fn expand_vars(&mut self) -> Result<(), ExpandVarsError> {
        self.host.expand_vars()?;
        expand_map(&mut self.auth_config)?;
        for ov in self.tenant_overrides.values_mut() {
            ov.expand_vars()?;
        }
        Ok(())
    }
}

fn is_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']')
}

/// The host is the OAGW alias in `/{alias}/...`; `/`, `?`, `#`, `@` would change the proxied path.
fn validate_host(what: &str, host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err(format!("{what} must be non-empty"));
    }
    if let Some(bad) = host.chars().find(|c| !is_host_char(*c)) {
        return Err(format!(
            "{what} '{host}' contains invalid character {bad:?} (allowed: letters, digits, . - _ : [ ])"
        ));
    }
    Ok(())
}

/// One OAGW upstream the gear provisions: a provider entry or one of its tenant overrides, with
/// the override's unset fields taken from the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSpec {
    /// Config path naming the source (`providers.<id>` or `providers.<id>.tenant_overrides.<t>`).
    pub label: String,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<BTreeMap<String, String>>,
}

impl UpstreamSpec {
    /// Names of the upstream settings in which `self` and `other` differ (never their values:
    /// `auth_config` may hold secret references).
    fn differences(&self, other: &Self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !self.host.eq_ignore_ascii_case(&other.host) {
            out.push("host");
        }
        if self.port != other.port {
            out.push("port");
        }
        if self.use_http != other.use_http {
            out.push("scheme");
        }
        if self.auth_plugin_type != other.auth_plugin_type || self.auth_config != other.auth_config
        {
            out.push("auth");
        }
        out
    }
}

/// Rejects two upstream specs that share an OAGW alias but not their upstream settings.
///
/// OAGW keys upstreams by alias within a tenant, and the gear provisions every entry and every
/// tenant override under the one S2S context, so all specs share one alias namespace: the second
/// `create_upstream` of an alias finds the first upstream and reuses it, silently dropping its
/// own host / port / scheme / credentials. Specs with identical settings may share an alias.
///
/// # Errors
/// Names both specs, the alias and the differing settings, and suggests a distinct alias.
pub fn validate_alias_collisions(
    providers: &BTreeMap<String, ProviderEntry>,
) -> Result<(), String> {
    let mut seen: BTreeMap<String, UpstreamSpec> = BTreeMap::new();
    for (id, entry) in providers {
        for spec in entry.upstream_specs(id) {
            match seen.get(&spec.alias) {
                Some(first) => {
                    let diff = first.differences(&spec);
                    if !diff.is_empty() {
                        return Err(format!(
                            "{} and {} share the OAGW upstream alias '{}' but differ in {}; OAGW \
                             keeps one upstream per alias, so the settings of one would be \
                             silently ignored - set a distinct explicit upstream_alias on one of them",
                            first.label,
                            spec.label,
                            spec.alias,
                            diff.join(", ")
                        ));
                    }
                }
                None => {
                    seen.insert(spec.alias.clone(), spec);
                }
            }
        }
    }
    Ok(())
}

impl ProviderEntry {
    /// The upstreams of this entry (`id`): the entry itself, then each tenant override.
    #[must_use]
    pub fn upstream_specs(&self, id: &str) -> Vec<UpstreamSpec> {
        let port = self.port.unwrap_or(standard_port(self.use_http));
        let base = UpstreamSpec {
            label: format!("providers.{id}"),
            alias: self.alias(None),
            host: self.host.clone(),
            port,
            use_http: self.use_http,
            auth_plugin_type: self.auth_plugin_type.clone(),
            auth_config: self.auth_config.clone(),
        };
        let overrides = self
            .tenant_overrides
            .iter()
            .map(|(tenant, ov)| UpstreamSpec {
                label: format!("providers.{id}.tenant_overrides.{tenant}"),
                alias: self.alias(Some(ov)),
                host: ov.host.clone().unwrap_or_else(|| self.host.clone()),
                port,
                use_http: self.use_http,
                auth_plugin_type: ov
                    .auth_plugin_type
                    .clone()
                    .or_else(|| self.auth_plugin_type.clone()),
                auth_config: ov.auth_config.clone().or_else(|| self.auth_config.clone()),
            });
        std::iter::once(base).chain(overrides).collect()
    }

    /// The OAGW alias of this entry, or of its tenant override `ov` (the override's host, or the
    /// entry's when the override only sets an alias). See [`derive_alias`].
    #[must_use]
    pub fn alias(&self, ov: Option<&TenantOverride>) -> String {
        let (explicit, host) = match ov {
            Some(ov) => (
                ov.upstream_alias.as_deref(),
                ov.host.as_deref().unwrap_or(&self.host),
            ),
            None => (self.upstream_alias.as_deref(), self.host.as_str()),
        };
        derive_alias(explicit, host, self.port, self.use_http)
    }

    /// Fills `upstream_alias` on the entry and on every tenant override when unset.
    pub fn fill_aliases(&mut self) {
        let overrides: Vec<(String, String)> = self
            .tenant_overrides
            .iter()
            // An override with neither host nor alias is left for `validate` to reject.
            .filter(|(_, ov)| ov.upstream_alias.is_none() && ov.host.is_some())
            .map(|(tenant, ov)| (tenant.clone(), self.alias(Some(ov))))
            .collect();
        for (tenant, alias) in overrides {
            if let Some(ov) = self.tenant_overrides.get_mut(&tenant) {
                ov.upstream_alias = Some(alias);
            }
        }
        if self.upstream_alias.is_none() {
            self.upstream_alias = Some(self.alias(None));
        }
    }

    /// Validates one entry. `all` is the whole provider map (for `rag_provider` references).
    ///
    /// # Errors
    /// Returns a description of the first violated rule.
    pub fn validate(&self, id: &str, all: &BTreeMap<String, ProviderEntry>) -> Result<(), String> {
        let at = |msg: String| format!("providers.{id}: {msg}");
        validate_host("host", &self.host).map_err(&at)?;
        if self.port == Some(0) {
            return Err(at("port must not be 0".to_owned()));
        }
        if let Some(alias) = &self.upstream_alias {
            validate_host("upstream_alias", alias).map_err(&at)?;
        }
        if self.storage_kind == Some(StorageKind::Azure) {
            let version = self.api_version.as_deref().unwrap_or("").trim();
            if version.is_empty() {
                return Err(at(
                    "api_version is required when storage_kind = azure".to_owned()
                ));
            }
            if self.api_version.as_deref() != Some(version)
                || !version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return Err(at(
                    "api_version may contain only letters, digits, '.' and '-'".to_owned(),
                ));
            }
        }
        match &self.rag_provider {
            Some(rag) if rag == id => {
                return Err(at("rag_provider must not name the entry itself".to_owned()));
            }
            Some(rag) => match all.get(rag) {
                None => {
                    return Err(at(format!("rag_provider '{rag}' does not name a provider")));
                }
                Some(target) if target.storage_kind.is_none() => {
                    return Err(at(format!(
                        "rag_provider '{rag}' has no storage_kind (file and vector-store operations need one)"
                    )));
                }
                Some(_) => {}
            },
            None if self.kind == ProviderKind::AnthropicMessages => {
                return Err(at(
                    "kind anthropic_messages requires rag_provider".to_owned()
                ));
            }
            _ => {}
        }
        for (tenant, ov) in &self.tenant_overrides {
            let at_ov = |msg: String| at(format!("tenant_overrides.{tenant}: {msg}"));
            if ov.host.is_none() && ov.upstream_alias.is_none() {
                return Err(at_ov("must set host or upstream_alias".to_owned()));
            }
            if let Some(host) = &ov.host {
                validate_host("host", host).map_err(&at_ov)?;
            }
            if let Some(alias) = &ov.upstream_alias {
                validate_host("upstream_alias", alias).map_err(&at_ov)?;
            }
        }
        Ok(())
    }
}
