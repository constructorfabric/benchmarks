//! Gear configuration (DESIGN Appendix B.1 - B.9).
//!
//! `MiniChatConfig` rejects unknown keys at the top level and in the strict
//! sections (`streaming`, `estimation_budgets`, `quota`, `outbox`, `context`,
//! `rag`, `client_credentials`, `metrics`, `providers.<id>` with its
//! `tenant_overrides`, `thumbnail`, `knowledge_search`). The worker sections
//! (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`,
//! `cleanup_worker`) ignore unknown keys. Every section is validated at gear
//! init by [`MiniChatConfig::validate`]; `${VAR}` expansion applies to provider
//! `host`/`auth_config`, the same fields of the tenant overrides and to
//! `client_credentials`.

mod provider;
mod sections;
mod validate;

use std::collections::HashMap;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

pub use provider::{ProviderConfig, ProviderKind, ProviderTarget, StorageKind, TenantOverride};
pub use sections::{
    CleanupWorkerConfig, ContextConfig, EstimationBudgetsConfig, KnowledgeSearchConfig,
    MetricsConfig, OrphanWatchdogConfig, OutboxConfig, QuotaConfig, RagConfig, StreamingConfig,
    ThreadSummaryWorkerConfig, ThumbnailConfig, UploadReaperConfig,
};

/// Default `vendor` (selects the model-policy and audit plugin instances).
pub const DEFAULT_VENDOR: &str = "constructorfabric";

/// Default REST prefix.
pub const DEFAULT_URL_PREFIX: &str = "/mini-chat";

/// Id of the single default provider entry.
pub const DEFAULT_PROVIDER_ID: &str = "openai";

/// S2S credentials exchanged through `authn_resolver` for OAGW provisioning.
///
/// Both values are required and must be non-empty. The secret is a
/// [`SecretString`], so it is redacted in `Debug` output and logs.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ClientCredentialsConfig {
    /// OAuth client id.
    #[serde(default)]
    #[expand_vars]
    pub client_id: String,
    /// OAuth client secret (redacted in `Debug`).
    #[serde(default = "empty_secret")]
    #[expand_vars]
    pub client_secret: SecretString,
}

fn empty_secret() -> SecretString {
    SecretString::from(String::new())
}

impl Default for ClientCredentialsConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: empty_secret(),
        }
    }
}

/// Root of the `gears.mini-chat.config` section.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    /// Prefix of all REST routes.
    pub url_prefix: String,
    /// Vendor of the plugin instances this gear selects.
    pub vendor: String,
    /// Metric naming.
    pub metrics: MetricsConfig,
    /// S2S credentials for OAGW provisioning.
    #[expand_vars]
    pub client_credentials: ClientCredentialsConfig,
    /// Provider registry (an explicit map replaces the default entry).
    #[expand_vars]
    pub providers: HashMap<String, ProviderConfig>,
    /// SSE streaming.
    pub streaming: StreamingConfig,
    /// Estimation budgets (only `minimal_generation_floor` is used).
    pub estimation_budgets: EstimationBudgetsConfig,
    /// Quota knobs.
    pub quota: QuotaConfig,
    /// Outbox queues.
    pub outbox: OutboxConfig,
    /// Context assembly.
    pub context: ContextConfig,
    /// Uploads and RAG.
    pub rag: RagConfig,
    /// Image thumbnails.
    pub thumbnail: ThumbnailConfig,
    /// Orphan turn watchdog.
    pub orphan_watchdog: OrphanWatchdogConfig,
    /// Upload reaper.
    pub upload_reaper: UploadReaperConfig,
    /// Thread summary worker.
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    /// Cleanup workers.
    pub cleanup_worker: CleanupWorkerConfig,
    /// Knowledge search.
    pub knowledge_search: KnowledgeSearchConfig,
}

impl Default for MiniChatConfig {
    fn default() -> Self {
        Self {
            url_prefix: DEFAULT_URL_PREFIX.to_owned(),
            vendor: DEFAULT_VENDOR.to_owned(),
            metrics: MetricsConfig::default(),
            client_credentials: ClientCredentialsConfig::default(),
            providers: HashMap::from([(
                DEFAULT_PROVIDER_ID.to_owned(),
                ProviderConfig::default_openai(),
            )]),
            streaming: StreamingConfig::default(),
            estimation_budgets: EstimationBudgetsConfig::default(),
            quota: QuotaConfig::default(),
            outbox: OutboxConfig::default(),
            context: ContextConfig::default(),
            rag: RagConfig::default(),
            thumbnail: ThumbnailConfig::default(),
            orphan_watchdog: OrphanWatchdogConfig::default(),
            upload_reaper: UploadReaperConfig::default(),
            thread_summary_worker: ThreadSummaryWorkerConfig::default(),
            cleanup_worker: CleanupWorkerConfig::default(),
            knowledge_search: KnowledgeSearchConfig::default(),
        }
    }
}

impl MiniChatConfig {
    /// Validates every section.
    ///
    /// # Errors
    /// A description of the first invalid value (never contains secrets).
    pub fn validate(&self) -> Result<(), String> {
        validate::url_prefix("url_prefix", &self.url_prefix)?;
        validate::non_empty("vendor", &self.vendor)?;
        validate::non_empty(
            "client_credentials.client_id",
            &self.client_credentials.client_id,
        )?;
        validate::non_empty(
            "client_credentials.client_secret",
            self.client_credentials.client_secret.expose_secret(),
        )?;
        for (id, provider) in &self.providers {
            provider.validate(id, &self.providers)?;
        }
        self.streaming.validate()?;
        self.estimation_budgets.validate(&self.streaming)?;
        self.quota.validate()?;
        self.outbox.validate()?;
        self.context.validate()?;
        self.rag.validate()?;
        self.thumbnail.validate()?;
        self.orphan_watchdog.validate()?;
        self.upload_reaper.validate()?;
        self.thread_summary_worker.validate()?;
        self.cleanup_worker.validate()?;
        self.knowledge_search.validate()
    }

    /// Names of the deprecated `estimation_budgets.*` keys set to a non-default
    /// value (parsed but unused, B.5.2); init logs one warning per name.
    #[must_use]
    pub fn deprecated_estimation_budget_keys(&self) -> Vec<&'static str> {
        self.estimation_budgets.deprecated_non_default_keys()
    }
}
