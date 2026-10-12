//! Provider registry entries (`providers.<id>`), ADR-0005.

use serde::Deserialize;
use std::collections::HashMap;
use toolkit_macros::ExpandVars;

/// Adapter used to talk to a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// `OpenAI` and Azure `OpenAI` Responses API.
    OpenaiResponses,
    /// Chat Completions API.
    OpenaiChatCompletions,
    /// vLLM Responses API.
    VllmResponses,
    /// Anthropic Messages API.
    AnthropicMessages,
}

/// File and vector-store implementation of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    /// `OpenAI` Files and Vector Stores API.
    Openai,
    /// Azure `OpenAI` Files and Vector Stores API.
    Azure,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

/// A per-tenant override of a provider entry. Unset fields fall back to the provider entry.
/// An override must set `host` or `upstream_alias`.
#[derive(Debug, Clone, Default, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverride {
    /// Tenant-specific host.
    #[expand_vars]
    pub host: Option<String>,
    /// Tenant-specific OAGW upstream alias.
    pub upstream_alias: Option<String>,
    /// Tenant-specific OAGW auth plugin.
    pub auth_plugin_type: Option<String>,
    /// Tenant-specific auth plugin configuration.
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
}

/// One provider entry.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// Adapter kind (required).
    pub kind: ProviderKind,
    /// Provider host (required, non-empty); OAGW derives the upstream alias from it (see
    /// `infra::llm::upstream_alias`).
    #[expand_vars]
    pub host: String,
    /// Port; defaults to `443` (`80` when `use_http` is set). Must not be `0`.
    #[serde(default)]
    pub port: Option<u16>,
    /// Use plain HTTP (effective only when OAGW allows HTTP upstreams).
    #[serde(default)]
    pub use_http: bool,
    /// OAGW upstream alias of an IP host (normalized); ignored for a hostname, whose alias OAGW
    /// derives. Defaults to the IP, with `:port` when the port is not standard for the scheme.
    #[serde(default)]
    pub upstream_alias: Option<String>,
    /// Chat endpoint path; `{model}` is replaced by `provider_model_id`.
    #[serde(default = "default_api_path")]
    pub api_path: String,
    /// OAGW auth plugin type.
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    /// Configuration of the auth plugin (`header`, `prefix`, `secret_ref`, ...).
    #[serde(default)]
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
    /// File and vector-store implementation (required).
    pub storage_kind: StorageKind,
    /// Label stored in `attachments.storage_backend` and `chat_vector_stores.provider`;
    /// defaults to the provider id.
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Azure `api-version` for RAG requests; required when `storage_kind` is `azure`.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Provider used for file and vector-store operations (for example for Anthropic).
    #[serde(default)]
    pub rag_provider: Option<String>,
    /// Per-tenant overrides keyed by tenant id (UUID string).
    #[serde(default)]
    #[expand_vars]
    pub tenant_overrides: HashMap<String, TenantOverride>,
}

impl ProviderEntry {
    /// The default `openai` entry used when the `providers` section is absent.
    #[must_use]
    pub fn default_openai() -> Self {
        Self {
            kind: ProviderKind::OpenaiResponses,
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
            storage_kind: StorageKind::Openai,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        }
    }

    /// The effective port: the configured one, else `80` for plain HTTP and `443` otherwise.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }
}
