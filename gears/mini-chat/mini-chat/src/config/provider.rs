//! Provider registry entries (`providers.<id>`, DESIGN B.1, ADR-0005).

use std::collections::HashMap;

use serde::Deserialize;

use super::validate;

/// Adapter kind of a provider entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum ProviderKind {
    /// `OpenAI` / Azure `OpenAI` Responses API.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// Chat Completions API.
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    /// vLLM Responses API.
    #[serde(rename = "vllm_responses")]
    VllmResponses,
    /// Anthropic Messages API.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

/// File and vector-store implementation of a provider entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum StorageKind {
    /// `OpenAI` Files / Vector Stores API.
    #[serde(rename = "openai")]
    OpenAi,
    /// Azure `OpenAI` (needs `api_version`).
    #[serde(rename = "azure")]
    Azure,
}

/// Per-tenant override of a provider entry. Each unset field falls back to
/// the provider entry on its own.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct TenantOverride {
    /// Tenant specific host.
    #[serde(default)]
    #[expand_vars]
    pub host: Option<String>,
    /// Tenant specific OAGW alias.
    #[serde(default)]
    pub upstream_alias: Option<String>,
    /// Tenant specific OAGW auth plugin.
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    /// Tenant specific auth configuration.
    #[serde(default)]
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

/// One `providers.<id>` entry.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Adapter kind.
    pub kind: ProviderKind,
    /// Provider host (also the default OAGW alias).
    #[expand_vars]
    pub host: String,
    /// Port (`443`, or `80` with `use_http`, when unset).
    #[serde(default)]
    pub port: Option<u16>,
    /// Plain HTTP (effective only when OAGW allows HTTP upstreams).
    #[serde(default)]
    pub use_http: bool,
    /// OAGW upstream alias (defaults to `host` at gear init).
    #[serde(default)]
    pub upstream_alias: Option<String>,
    /// Chat endpoint path; `{model}` is replaced by `provider_model_id`.
    #[serde(default = "default_api_path")]
    pub api_path: String,
    /// OAGW auth plugin type.
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    /// OAGW auth plugin configuration (`header`, `prefix`, `secret_ref`, ...).
    #[serde(default)]
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
    /// File / vector-store implementation.
    pub storage_kind: StorageKind,
    /// Label stored in `attachments.storage_backend` (defaults to the provider id).
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Azure `api-version` (required for `storage_kind = azure`).
    #[serde(default)]
    pub api_version: Option<String>,
    /// Provider that serves file / vector-store operations.
    #[serde(default)]
    pub rag_provider: Option<String>,
    /// Per-tenant overrides keyed by tenant id.
    #[serde(default)]
    #[expand_vars]
    pub tenant_overrides: HashMap<String, TenantOverride>,
}

/// The OAGW upstream a provider entry (or one of its tenant overrides) is
/// registered under and routed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTarget {
    /// Endpoint host.
    pub host: String,
    /// Endpoint port.
    pub port: u16,
    /// Plain HTTP endpoint.
    pub use_http: bool,
    /// OAGW upstream alias (the first path segment of every proxied URI).
    pub alias: String,
    /// OAGW auth plugin type.
    pub auth_plugin_type: Option<String>,
    /// OAGW auth plugin configuration, passed through verbatim.
    pub auth_config: Option<HashMap<String, String>>,
}

impl ProviderConfig {
    /// The default `openai` entry: `OpenAI` Responses over HTTPS with API-key
    /// auth from the credstore secret `openai-key`.
    #[must_use]
    pub fn default_openai() -> Self {
        Self {
            kind: ProviderKind::OpenAiResponses,
            host: "api.openai.com".to_owned(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: default_api_path(),
            auth_plugin_type: Some(
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned(),
            ),
            auth_config: Some(HashMap::from([
                ("header".to_owned(), "Authorization".to_owned()),
                ("prefix".to_owned(), "Bearer ".to_owned()),
                ("secret_ref".to_owned(), "cred://openai-key".to_owned()),
            ])),
            storage_kind: StorageKind::OpenAi,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        }
    }

    /// The upstream of the entry itself. `upstream_alias` defaults to `host`,
    /// so an alias is always passed to OAGW.
    #[must_use]
    pub fn target(&self) -> ProviderTarget {
        ProviderTarget {
            host: self.host.clone(),
            port: self.effective_port(),
            use_http: self.use_http,
            alias: self
                .upstream_alias
                .clone()
                .unwrap_or_else(|| self.host.clone()),
            auth_plugin_type: self.auth_plugin_type.clone(),
            auth_config: self.auth_config.clone(),
        }
    }

    /// The upstream of one tenant override. Every unset field falls back to the
    /// entry on its own; the alias defaults to the override's host.
    #[must_use]
    pub fn tenant_target(&self, tenant_override: &TenantOverride) -> ProviderTarget {
        let base = self.target();
        ProviderTarget {
            host: tenant_override.host.clone().unwrap_or(base.host),
            port: base.port,
            use_http: base.use_http,
            alias: tenant_override
                .upstream_alias
                .clone()
                .or_else(|| tenant_override.host.clone())
                .unwrap_or(base.alias),
            auth_plugin_type: tenant_override
                .auth_plugin_type
                .clone()
                .or(base.auth_plugin_type),
            auth_config: tenant_override.auth_config.clone().or(base.auth_config),
        }
    }

    /// Effective port: `port`, else `80` with `use_http`, else `443`.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }

    pub(super) fn validate(
        &self,
        id: &str,
        all: &HashMap<String, ProviderConfig>,
    ) -> Result<(), String> {
        validate::host(&format!("providers.{id}.host"), &self.host)?;
        if self.port == Some(0) {
            return Err(format!("providers.{id}.port must not be 0"));
        }
        if self.storage_kind == StorageKind::Azure {
            let api_version = self.api_version.as_deref().unwrap_or_default();
            let name = format!("providers.{id}.api_version");
            validate::non_empty(&name, api_version)?;
            if !api_version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
            {
                return Err(format!("{name} may only contain letters, digits, . and -"));
            }
        }
        if let Some(rag) = &self.rag_provider
            && !all.contains_key(rag)
        {
            return Err(format!(
                "providers.{id}.rag_provider references unknown provider `{rag}`"
            ));
        }
        for (tenant, tenant_override) in &self.tenant_overrides {
            tenant_override.validate(id, tenant)?;
        }
        Ok(())
    }
}

impl TenantOverride {
    fn validate(&self, provider: &str, tenant: &str) -> Result<(), String> {
        let name = format!("providers.{provider}.tenant_overrides.{tenant}");
        if self.host.is_none() && self.upstream_alias.is_none() {
            return Err(format!("{name} must set host or upstream_alias"));
        }
        if let Some(host) = &self.host {
            validate::host(&format!("{name}.host"), host)?;
        }
        Ok(())
    }
}
