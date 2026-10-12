//! Gear configuration (`gears.mini-chat.config`), DESIGN Appendix B.
//!
//! The configuration rejects unknown keys at the top level and in the sections `streaming`,
//! `estimation_budgets`, `quota`, `outbox`, `context`, `rag`, `client_credentials`, `metrics`,
//! `providers.<id>` (and its `tenant_overrides`), `thumbnail` and `knowledge_search`. The worker
//! sections (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker`) accept
//! and ignore unknown keys. Every section is validated at gear init by
//! [`MiniChatConfig::validate`].

mod providers;
mod sections;
mod validation;
mod workers;

use serde::Deserialize;
use std::collections::HashMap;
use toolkit_macros::ExpandVars;

pub use providers::{ProviderEntry, ProviderKind, StorageKind, TenantOverride};
pub use sections::{
    ClientCredentials, ContextConfig, EstimationBudgetsConfig, KnowledgeSearchConfig,
    MetricsConfig, OutboxConfig, QuotaConfig, RagConfig, StreamingConfig, ThumbnailConfig,
};
pub use validation::ConfigValidationError;
pub use workers::{
    CleanupWorkerConfig, DEFAULT_SUMMARY_SYSTEM_PROMPT, OrphanWatchdogConfig,
    ThreadSummaryWorkerConfig, UploadReaperConfig,
};

/// Default prefix of every REST route.
pub const DEFAULT_URL_PREFIX: &str = "/mini-chat";

/// Default vendor selecting the model-policy and audit plugin instances.
pub const DEFAULT_VENDOR: &str = "constructorfabric";

fn default_url_prefix() -> String {
    DEFAULT_URL_PREFIX.to_owned()
}

fn default_vendor() -> String {
    DEFAULT_VENDOR.to_owned()
}

fn default_providers() -> HashMap<String, ProviderEntry> {
    HashMap::from([("openai".to_owned(), ProviderEntry::default_openai())])
}

/// The complete configuration of the mini-chat gear.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct MiniChatConfig {
    /// Prefix of all REST routes.
    #[serde(default = "default_url_prefix")]
    pub url_prefix: String,
    /// Vendor selecting the model-policy and audit plugin instances; must be non-empty.
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// S2S credentials exchanged via `authn_resolver` for OAGW provisioning (required).
    #[expand_vars]
    pub client_credentials: ClientCredentials,
    /// Metrics settings.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// Provider registry; the key is the provider id referenced by catalog entries.
    #[serde(default = "default_providers")]
    #[expand_vars]
    pub providers: HashMap<String, ProviderEntry>,
    /// Streaming (SSE) settings.
    #[serde(default)]
    pub streaming: StreamingConfig,
    /// Estimation budgets (only `minimal_generation_floor` is used).
    #[serde(default)]
    pub estimation_budgets: EstimationBudgetsConfig,
    /// Quota knobs.
    #[serde(default)]
    pub quota: QuotaConfig,
    /// Shared outbox integration.
    #[serde(default)]
    pub outbox: OutboxConfig,
    /// Context assembly settings.
    #[serde(default)]
    pub context: ContextConfig,
    /// File search / RAG and upload limits.
    #[serde(default)]
    pub rag: RagConfig,
    /// Image thumbnail settings.
    #[serde(default)]
    pub thumbnail: ThumbnailConfig,
    /// Knowledge search.
    #[serde(default)]
    pub knowledge_search: KnowledgeSearchConfig,
    /// Orphan watchdog worker.
    #[serde(default)]
    pub orphan_watchdog: OrphanWatchdogConfig,
    /// Upload reaper worker.
    #[serde(default)]
    pub upload_reaper: UploadReaperConfig,
    /// Thread summary worker.
    #[serde(default)]
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    /// Cleanup worker.
    #[serde(default)]
    pub cleanup_worker: CleanupWorkerConfig,
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
