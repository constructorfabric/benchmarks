//! Pure fixtures of the message-streaming slice S2-provider-path: a builder of
//! [`ProviderCallPlan`]s (any adapter kind, tool set, history, summary and images) that needs no
//! database, so the adapter request shapes and the terminal rules are plain unit tests.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::sync::Arc;

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, QuotaDecision};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::send::{FILE_GUARD, WEB_GUARD, std_model, t, with_web_search};
use super::{TENANT_A, USER_A1};
use crate::config::ProviderKind;
use crate::domain::provider_registry::EffectiveProvider;
use crate::domain::quota::{
    DowngradeReason, PeriodStarts, PreflightDecision, ReserveReceipt, ToolCallCounts,
};
use crate::domain::streaming::context::{
    ContextMessage, ContextRole, CurrentMessage, ImageInput, ThreadSummaryRow, assemble_context,
};
use crate::domain::streaming::{
    ProviderCallPlan, ToolContribution, ToolDefinition, ToolKind, ToolPart, ToolSet, ToolTurnFacts,
    assemble_tools,
};

/// A stub contribution of another feature: always adds its tool with the given guard and
/// `include` entry.
pub struct StubContribution {
    pub kind: ToolKind,
    pub guard: Option<String>,
    pub include: Option<String>,
}

impl ToolContribution for StubContribution {
    fn kind(&self) -> ToolKind {
        self.kind
    }

    fn contribute(&self, _facts: &ToolTurnFacts) -> Option<ToolPart> {
        Some(ToolPart {
            definition: ToolDefinition {
                kind: self.kind,
                search_context_size: None,
            },
            guard: self.guard.clone(),
            include: self.include.clone(),
        })
    }
}

/// The code interpreter contribution: no guard, `include` `code_interpreter_call.outputs`.
pub fn code_interpreter_contribution() -> Arc<dyn ToolContribution> {
    Arc::new(StubContribution {
        kind: ToolKind::CodeInterpreter,
        guard: None,
        include: Some("code_interpreter_call.outputs".to_owned()),
    })
}

/// The file search contribution with the rig's file search guard.
pub fn file_search_contribution() -> Arc<dyn ToolContribution> {
    Arc::new(StubContribution {
        kind: ToolKind::FileSearch,
        guard: Some(FILE_GUARD.to_owned()),
        include: None,
    })
}

/// The tenant of the fixture plans.
pub const PLAN_TENANT: Uuid = TENANT_A;
/// The requester of the fixture plans.
pub const PLAN_USER: Uuid = USER_A1;

/// A builder of the plan of one send, as the send setup would hand it to the provider task.
pub struct PlanBuilder {
    kind: ProviderKind,
    alias: String,
    api_path: String,
    model_patch: Value,
    web_search_requested: bool,
    web_search_supported: bool,
    contributions: Vec<Arc<dyn ToolContribution>>,
    history: Vec<(ContextRole, String)>,
    summary: Option<String>,
    content: String,
    /// `(primary file id, secondary Anthropic file id)` per image.
    images: Vec<(String, Option<String>)>,
    max_output_tokens: i64,
    selected_model: Option<String>,
}

impl PlanBuilder {
    /// A plan on the standard model: OpenAI Responses, alias `azure.example.com`, path
    /// `/v1/responses`, no tool, no history, no summary, the content `hello`.
    pub fn new() -> Self {
        Self {
            kind: ProviderKind::OpenaiResponses,
            alias: "azure.example.com".to_owned(),
            api_path: "/v1/responses".to_owned(),
            model_patch: json!({}),
            web_search_requested: false,
            web_search_supported: true,
            contributions: Vec::new(),
            history: Vec::new(),
            summary: None,
            content: "hello".to_owned(),
            images: Vec::new(),
            max_output_tokens: 500,
            selected_model: None,
        }
    }

    pub fn kind(mut self, kind: ProviderKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn alias(mut self, alias: &str) -> Self {
        self.alias = alias.to_owned();
        self
    }

    pub fn api_path(mut self, api_path: &str) -> Self {
        self.api_path = api_path.to_owned();
        self
    }

    /// Top-level keys of the catalog entry to replace (for example `max_tool_calls`).
    pub fn model(mut self, patch: &Value) -> Self {
        self.model_patch = patch.clone();
        self
    }

    /// The model's `general_config.api_params`.
    pub fn api_params(mut self, api_params: &Value) -> Self {
        let mut patch = self.model_patch.clone();
        let mut general = serde_json::to_value(std_model(&json!({})).general_config).unwrap();
        general["api_params"] = api_params.clone();
        patch["general_config"] = general;
        self.model_patch = patch;
        self
    }

    /// The request enables web search; `supported` is the model's `tool_support.web_search`.
    pub fn web_search(mut self, supported: bool) -> Self {
        self.web_search_requested = true;
        self.web_search_supported = supported;
        self
    }

    pub fn contribution(mut self, contribution: Arc<dyn ToolContribution>) -> Self {
        self.contributions.push(contribution);
        self
    }

    /// One history message (chronological order of the calls).
    pub fn history(mut self, role: ContextRole, text: &str) -> Self {
        self.history.push((role, text.to_owned()));
        self
    }

    pub fn summary(mut self, text: &str) -> Self {
        self.summary = Some(text.to_owned());
        self
    }

    pub fn content(mut self, content: &str) -> Self {
        self.content = content.to_owned();
        self
    }

    /// One current image by its internal provider file id.
    pub fn image(mut self, provider_file_id: &str) -> Self {
        self.images.push((provider_file_id.to_owned(), None));
        self
    }

    /// One current image with its primary provider file id and its secondary Anthropic file id.
    pub fn image_with_secondary(mut self, provider_file_id: &str, secondary_file_id: &str) -> Self {
        self.images.push((
            provider_file_id.to_owned(),
            Some(secondary_file_id.to_owned()),
        ));
        self
    }

    pub fn max_output_tokens(mut self, max: i64) -> Self {
        self.max_output_tokens = max;
        self
    }

    /// The chat selected `selected` while the effective model is the standard one (a downgrade).
    pub fn downgraded_from(mut self, selected: &str) -> Self {
        self.selected_model = Some(selected.to_owned());
        self
    }

    pub fn build(self) -> ProviderCallPlan {
        let base = std_model(&self.model_patch);
        let entry: ModelCatalogEntry = with_web_search(&base, self.web_search_supported);
        let tools: ToolSet = assemble_tools(
            &entry,
            &KillSwitches::default(),
            WEB_GUARD,
            &ToolTurnFacts {
                chat_id: Uuid::from_u128(0xC4A7),
                request_id: Uuid::from_u128(0x7E57),
                web_search_enabled: self.web_search_requested,
            },
            &self.contributions,
        );
        let recent: Vec<ContextMessage> = self
            .history
            .iter()
            .enumerate()
            .map(|(index, (role, text))| ContextMessage {
                id: Uuid::from_u128(0x1000 + index as u128),
                role: *role,
                content: text.clone(),
                created_at: t(10 + i64::try_from(index).unwrap()),
            })
            .collect();
        let summary = self.summary.as_ref().map(|text| ThreadSummaryRow {
            summary_text: text.clone(),
            token_estimate: 42,
            frontier_created_at: t(1),
            frontier_message_id: Uuid::from_u128(0x0F01),
        });
        let images: Vec<ImageInput> = self
            .images
            .iter()
            .enumerate()
            .map(|(index, file)| ImageInput {
                attachment_id: Uuid::from_u128(0xA000 + index as u128),
                provider_file_id: file.0.clone(),
                secondary_file_id: file.1.clone(),
            })
            .collect();
        let context = assemble_context(
            &entry,
            &tools,
            self.max_output_tokens,
            summary.as_ref(),
            &recent,
            &CurrentMessage {
                content: self.content.clone(),
                images,
            },
        )
        .expect("the fixture context fits the budget");
        let started_at: OffsetDateTime = t(100);
        let effective = entry.id.clone();
        let downgrade = self.selected_model.is_some();
        let selected = self
            .selected_model
            .clone()
            .unwrap_or_else(|| effective.clone());
        let decision = PreflightDecision {
            effective_model: effective,
            selected_model: selected.clone(),
            tier: entry.tier,
            provider_id: entry.provider_id.clone(),
            vision_input: false,
            quota_decision: if downgrade {
                QuotaDecision::Downgrade
            } else {
                QuotaDecision::Allow
            },
            downgrade_from: downgrade.then(|| selected.clone()),
            downgrade_reason: downgrade.then_some(DowngradeReason::PremiumQuotaExhausted),
            max_output_tokens_applied: self.max_output_tokens,
            reserve_tokens: 600,
            reserved_credits_micro: 600_000,
            minimal_generation_floor_applied: 50,
            policy_version_applied: 1,
            estimated_text_tokens: 10,
            max_input_tokens: 0,
            started_at,
            period_starts: PeriodStarts::of(started_at),
            limits: Vec::new(),
        };
        let provider = EffectiveProvider {
            provider_id: entry.provider_id.clone(),
            kind: self.kind,
            host: "azure.example.com".to_owned(),
            port: 443,
            use_http: false,
            upstream_alias: self.alias.clone(),
            api_path: self.api_path.clone(),
            auth_plugin_type: None,
            auth_config: std::collections::BTreeMap::new(),
            storage_kind: crate::config::StorageKind::Openai,
            storage_backend: "azure_openai".to_owned(),
            api_version: None,
            rag_provider: None,
        };
        ProviderCallPlan {
            tenant_id: PLAN_TENANT,
            user_id: PLAN_USER,
            chat_id: Uuid::from_u128(0xC4A7),
            request_id: Uuid::from_u128(0x7E57),
            turn_id: Uuid::from_u128(0x7071),
            user_message_id: Uuid::from_u128(0x0E55),
            assistant_message_id: Uuid::from_u128(0xA551),
            selected_model: selected,
            model: entry,
            decision,
            receipt: ReserveReceipt {
                tier: mini_chat_sdk::ModelTier::Standard,
                reserved_credits_micro: 600_000,
                periods: crate::domain::quota::PeriodType::ALL.to_vec(),
            },
            context,
            tools,
            provider,
            web_search_enabled: self.web_search_requested,
            started_at,
        }
    }
}

impl Default for PlanBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts with the given completed calls.
pub fn counts(web: u32, code: u32, file: u32) -> ToolCallCounts {
    ToolCallCounts {
        web_search: web,
        code_interpreter: code,
        file_search: file,
    }
}
